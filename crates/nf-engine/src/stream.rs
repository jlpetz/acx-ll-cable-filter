//! WASAPI stream wrappers (engine.md "stream"). Each opens one stream and converts at the edge to the
//! app's 48 kHz mono f32. Shared mode, but for the mic and the output when they're asked for in
//! exclusive mode (`IoConfig::mic_exclusive`, `output_exclusive`, the default; engine.md, "Exclusive mode"):
//! event-driven at the device's smallest suitable period, in the best format it takes, falling back
//! to shared mode with a note when it refuses (exclusive.rs has the choices and the fall-back).
//!
//! Format rule: with the mix format at 48 kHz, use it as is (we downmix or upmix the channels, and
//! the mic may use IAudioClient3's small periods); otherwise ask for 48 kHz f32 at the mix's
//! channel count and let WASAPI convert (`AUTOCONVERTPCM`, which rules out IAudioClient3).
//!
//! Every object here is `!Send`: streams are made, used and dropped on the audio thread (or the
//! latency-test thread). The per-packet methods (`read_packet`, `padding`, `write`, `clock`)
//! don't allocate; a failed call returns the raw HRESULT.

use nf_core::{BLOCK, SAMPLE_RATE};
use wasapi::{
    AudioCaptureClient, AudioClient, AudioClientProperties, AudioClock, AudioRenderClient, AudioSessionControl, BufferFlags, Device,
    Direction, Handle, SampleType, StreamMode, StreamOption, TimingMode, WasapiError, WaveFormat, calculate_period_100ns,
};
use windows::Win32::Foundation::{HANDLE, PROPERTYKEY};
use windows::Win32::System::Com::STGM_READ;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToBoolean};
use windows::core::GUID;

use crate::api::StreamInfo;
use crate::convert::{SampleFormat, from_mono, to_mono};
use crate::errors::{StreamRole, Warning, describe_refusal, hr};
use crate::exclusive::{
    EventRender, ExclusiveFormat, ExclusiveRefusal, PeriodDevice, RING_FRAMES, exclusive_or_shared, exclusive_periods, first_exclusive,
    formats_to_try,
};
use crate::pipeline::{CaptureSource, ClockReading, Hr, Packet, RenderSink};
use crate::timing::Qpc;

/// `PKEY_Devices_AudioDevice_RawProcessingSupported` (copied to avoid the EnhancedStorage feature).
const PKEY_RAW_SUPPORTED: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0x8943b373_388c_4395_b557_bc6dbaffafdb), pid: 2 };

/// The raw HRESULT behind a wasapi error, without formatting anything. The crate's own (non-COM)
/// errors map to the nearest AUDCLNT code.
pub(crate) fn hr_of(e: &WasapiError) -> i32 {
    match e {
        WasapiError::Windows(w) | WasapiError::RegisterNotifications(w) => w.code().0,
        WasapiError::DeviceNotFound(_) => hr::E_NOTFOUND,
        WasapiError::UnsupportedFormat | WasapiError::UnsupportedSubformat(_) => hr::AUDCLNT_E_UNSUPPORTED_FORMAT,
        WasapiError::ClientNotInit => hr::AUDCLNT_E_NOT_INITIALIZED,
        WasapiError::DataLengthMismatch { .. } | WasapiError::DataLengthTooShort { .. } => hr::AUDCLNT_E_BUFFER_SIZE_ERROR,
        _ => hr::E_FAIL,
    }
}

#[inline]
fn hr(e: WasapiError) -> Hr {
    hr_of(&e)
}

/// The sample encoding of a WASAPI format, if we can convert it.
pub(crate) fn sample_format(f: &WaveFormat) -> Option<SampleFormat> {
    match (f.get_subformat().ok()?, f.get_bitspersample()) {
        (SampleType::Float, 32) => Some(SampleFormat::F32),
        (SampleType::Int, 16) => Some(SampleFormat::I16),
        (SampleType::Int, 24) => Some(SampleFormat::I24),
        // 24 valid bits in a 32-bit container is its own encoding (convert.rs: the low byte padding, written as zero).
        (SampleType::Int, 32) if f.get_validbitspersample() == 24 => Some(SampleFormat::I24In32),
        (SampleType::Int, 32) => Some(SampleFormat::I32),
        _ => None,
    }
}

/// The stream format for a device whose mix format is `mix`, and whether WASAPI must convert:
/// the mix itself when it runs at 48 kHz in a format we read; otherwise 48 kHz f32 at the mix's
/// channel count, converted by WASAPI.
pub(crate) fn stream_format(mix: &WaveFormat) -> (WaveFormat, bool) {
    if mix.get_samplespersec() == SAMPLE_RATE && sample_format(mix).is_some() {
        return (mix.clone(), false);
    }
    let channels = mix.get_nchannels().max(1) as usize;
    let mask = mix.get_dwchannelmask();
    let fmt = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, channels, (mask != 0).then_some(mask));
    (fmt, true)
}

/// The smallest IAudioClient3 period below `default` that is a multiple of `fundamental`, lies in
/// `[min, max]` and divides `hop`, so mic packets tile the 480-frame block exactly.
pub(crate) fn pick_period(default: u32, fundamental: u32, min: u32, max: u32, hop: u32) -> Option<u32> {
    if fundamental == 0 || hop == 0 {
        return None;
    }
    let mut f = min.div_ceil(fundamental).max(1) * fundamental;
    while f <= max && f < default {
        if hop.is_multiple_of(f) {
            return Some(f);
        }
        f += fundamental;
    }
    None
}

/// Frames in `hns` at `rate`, rounded.
pub(crate) fn hns_to_frames(hns: i64, rate: u32) -> usize {
    ((hns.max(0) * rate as i64 + 5_000_000) / 10_000_000) as usize
}

/// Whether the endpoint advertises RAW processing (no OEM effects ahead of our AEC).
fn raw_supported(device: &Device) -> bool {
    // SAFETY: the property store and PROPVARIANT come from COM calls on this thread's device and
    // live for the block; the PROPVARIANT is cleared once, after its last use.
    unsafe {
        let Ok(store) = device.as_immdevice().OpenPropertyStore(STGM_READ) else { return false };
        let Ok(mut v) = store.GetValue(&PKEY_RAW_SUPPORTED) else { return false };
        let supported = PropVariantToBoolean(&v).map(|b| b.as_bool()).unwrap_or(false);
        let _ = PropVariantClear(&mut v);
        supported
    }
}

/// A fresh client, with RAW set (before initialise, as IAudioClient2 requires) when asked.
fn new_client(device: &Device, raw: bool) -> Result<AudioClient, WasapiError> {
    let client = device.get_iaudioclient()?;
    if raw {
        client.set_properties(AudioClientProperties::new().set_option(StreamOption::Raw))?;
    }
    Ok(client)
}

/// The best-effort session control, with ducking opted out (`true` = opt out).
fn session_without_ducking(client: &AudioClient) -> Option<AudioSessionControl> {
    let session = client.get_audiosessioncontrol().ok()?;
    let _ = session.set_ducking_preference(true);
    Some(session)
}

/// How a stream was opened, for the status line and the latency report.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpenInfo {
    pub name: String,
    /// The device-side (mix) format.
    pub sample_rate: u32,
    pub channels: u16,
    /// The engine period in the stream's frames (48 kHz for every stream but the keep-alive).
    pub period_frames: usize,
    pub period_ms: f32,
    pub low_latency: bool,
    pub autoconvert: bool,
    pub raw: bool,
    /// In exclusive mode, in this format; `None` in shared mode.
    pub exclusive: Option<ExclusiveFormat>,
    /// Asked for in exclusive mode, refused for this (it runs shared).
    pub refused: Option<ExclusiveRefusal>,
}

impl OpenInfo {
    pub(crate) fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            device_name: self.name.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            period_ms: self.period_ms,
            low_latency: self.low_latency,
            autoconvert: self.autoconvert,
            raw: self.raw,
            exclusive: self.exclusive.map(ExclusiveFormat::label),
            exclusive_refused: self.refused.map(describe_refusal),
        }
    }
}

/// Mic options from `IoConfig`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MicOptions {
    pub low_latency: bool,
    pub raw: bool,
    /// Exclusive mode first (`IoConfig::mic_exclusive`); RAW and IAudioClient3 are shared mode's.
    pub exclusive: bool,
    /// The exclusive period guard's floor for this mic, in frames (0: none; control.rs).
    pub exclusive_floor: usize,
}

/// One capture stream, in shared mode (or the mic in exclusive mode): the mic (event-driven), the latency test's output capture
/// (event-driven) or the speakers' loopback (polled from the mic loop: loopback events stop
/// during silence).
pub(crate) struct CaptureStream {
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Option<Handle>,
    _session: Option<AudioSessionControl>,
    format: SampleFormat,
    channels: usize,
    raw: Box<[u8]>,
    max_frames: usize,
    pub(crate) info: OpenInfo,
}

impl CaptureStream {
    /// The mic. Asked for in exclusive mode, that first (event-driven at its period: exclusive.rs), and shared mode with the
    /// note saying why when the device refuses. In shared mode, RAW where supported (and asked), IAudioClient3's smallest
    /// suitable period where the driver allows (and asked, and the mix is 48 kHz), each falling back.
    pub(crate) fn open_mic(device: &Device, opts: MicOptions) -> Result<(Self, Option<Warning>), WasapiError> {
        let name = device.get_friendlyname().unwrap_or_default();
        let mix = device.get_iaudioclient()?.get_mixformat()?;
        let (mut mic, warning) = exclusive_or_shared(
            opts.exclusive,
            StreamRole::Mic,
            &name,
            || Self::open_mic_exclusive(device, &name, &mix, opts.exclusive_floor),
            || Self::open_mic_shared(device, &name, &mix, opts),
        )?;
        if let Some(Warning::ExclusiveUnavailable { why, .. }) = &warning {
            mic.info.refused = Some(*why);
        }
        Ok((mic, warning))
    }

    fn open_mic_exclusive(device: &Device, name: &str, mix: &WaveFormat, floor: usize) -> Result<Self, ExclusiveRefusal> {
        let (client, fmt, kind, frames) = open_exclusive(device, Direction::Capture, mix, floor)?;
        let info = exclusive_info(name, &fmt, frames, kind);
        Self::finish(client, &fmt, true, info).map_err(|e| ExclusiveRefusal::Refused(hr_of(&e)))
    }

    fn open_mic_shared(device: &Device, name: &str, mix: &WaveFormat, opts: MicOptions) -> Result<Self, WasapiError> {
        let raw_options: &[bool] = if opts.raw && raw_supported(device) { &[true, false] } else { &[false] };
        let mut last = WasapiError::UnsupportedFormat;
        for &raw in raw_options {
            if opts.low_latency
                && let Some((client, period)) = open_low_latency(device, mix, raw)
            {
                let info = info_for(name, mix, period as usize, true, false, raw);
                return Self::finish(client, mix, true, info);
            }
            match open_shared(device, mix, Direction::Capture, raw, true, 400_000) {
                Ok((client, fmt, autoconvert, period_hns)) => {
                    let info = info_for(name, mix, hns_to_frames(period_hns, SAMPLE_RATE), false, autoconvert, raw);
                    return Self::finish(client, &fmt, true, info);
                }
                // A refused initialise leaves the client unusable; the next attempt makes a new one.
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// An event-driven capture of any capture endpoint at the default period (the latency test).
    pub(crate) fn open_events(device: &Device) -> Result<Self, WasapiError> {
        let name = device.get_friendlyname().unwrap_or_default();
        let mix = device.get_iaudioclient()?.get_mixformat()?;
        let (client, fmt, autoconvert, period_hns) = open_shared(device, &mix, Direction::Capture, false, true, 400_000)?;
        let info = info_for(&name, &mix, hns_to_frames(period_hns, SAMPLE_RATE), false, autoconvert, false);
        Self::finish(client, &fmt, true, info)
    }

    /// The loopback of a render endpoint (the echo reference), polled. No category or RAW: those
    /// aren't valid on a loopback stream.
    pub(crate) fn open_loopback(device: &Device) -> Result<Self, WasapiError> {
        let name = device.get_friendlyname().unwrap_or_default();
        let mix = device.get_iaudioclient()?.get_mixformat()?;
        // A render device initialised for capture in shared mode is a loopback stream. The buffer
        // only needs to ride out a late mic wake-up; data age is what we drain each wake.
        let (client, fmt, autoconvert, period_hns) = open_shared(device, &mix, Direction::Capture, false, false, 2_000_000)?;
        let info = info_for(&name, &mix, hns_to_frames(period_hns, SAMPLE_RATE), false, autoconvert, false);
        Self::finish(client, &fmt, false, info)
    }

    fn finish(client: AudioClient, fmt: &WaveFormat, events: bool, info: OpenInfo) -> Result<Self, WasapiError> {
        let format = sample_format(fmt).ok_or(WasapiError::UnsupportedFormat)?;
        // After initialise, before start; get_audiocaptureclient snapshots the frame size.
        let event = if events { Some(client.set_get_eventhandle()?) } else { None };
        let capture = client.get_audiocaptureclient()?;
        let session = session_without_ducking(&client);
        let buffer = client.get_buffer_size()? as usize;
        let block_align = fmt.get_blockalign() as usize;
        // A packet never exceeds the buffer; twice is the crate tests' margin.
        let max_frames = (2 * buffer).max(BLOCK);
        Ok(Self {
            client,
            capture,
            event,
            _session: session,
            format,
            channels: fmt.get_nchannels().max(1) as usize,
            raw: vec![0u8; max_frames * block_align].into_boxed_slice(),
            max_frames,
            info,
        })
    }

    /// The largest packet `read_packet` can return, in frames.
    pub(crate) fn max_packet_frames(&self) -> usize {
        self.max_frames
    }

    pub(crate) fn start(&self) -> Result<(), Hr> {
        self.client.start_stream().map_err(hr)
    }

    pub(crate) fn stop(&self) {
        let _ = self.client.stop_stream();
    }

    /// Waits up to `ms` for the stream's event; false on timeout (or for a polled stream).
    pub(crate) fn wait(&self, ms: u32) -> bool {
        match &self.event {
            Some(e) => e.wait_for_event(ms).is_ok(),
            None => false,
        }
    }

    /// The stream's event, for a wait on it and the output's together; `None` for a polled stream. Valid while it lives.
    pub(crate) fn event(&self) -> Option<HANDLE> {
        self.event.as_ref().map(Handle::as_handle)
    }
}

impl CaptureSource for CaptureStream {
    fn read_packet(&mut self, out: &mut [f32]) -> Result<Option<Packet>, Hr> {
        let (frames, info) = self.capture.read_from_device(&mut self.raw).map_err(hr)?;
        if frames == 0 {
            return Ok(None);
        }
        let n = (frames as usize).min(out.len());
        let dst = &mut out[..n];
        if info.flags.silent {
            // The crate copies whatever is in the buffer even for silent packets.
            dst.fill(0.0);
        } else {
            to_mono(self.format, self.channels, &self.raw, dst);
        }
        Ok(Some(Packet {
            frames: n,
            device_pos: info.index,
            qpc_hns: (!info.flags.timestamp_error).then_some(info.timestamp),
            discontinuity: info.flags.data_discontinuity,
        }))
    }
}

/// One polled shared-mode render stream, serviced from the mic loop: the output cable, the
/// latency test's probe, or (native format) the keep-alive.
pub(crate) struct RenderStream {
    client: AudioClient,
    render: AudioRenderClient,
    clock: Option<AudioClock>,
    clock_freq: f64,
    /// Stamps each clock reading with the time it was taken.
    qpc: Qpc,
    _session: Option<AudioSessionControl>,
    /// `None` for a native format we can't encode (only silence is written then).
    format: Option<SampleFormat>,
    channels: usize,
    block_align: usize,
    rate: f64,
    buffer_frames: usize,
    scratch: Box<[u8]>,
    zeros: Box<[u8]>,
    pub(crate) info: OpenInfo,
}

impl RenderStream {
    /// A 48 kHz stream: the mix format when it runs at 48 kHz, else 48 kHz f32 with WASAPI's
    /// converter. `buffer_hns` is the capacity; latency is the padding we keep.
    pub(crate) fn open(device: &Device, buffer_hns: i64) -> Result<Self, WasapiError> {
        let name = device.get_friendlyname().unwrap_or_default();
        let mix = device.get_iaudioclient()?.get_mixformat()?;
        let (client, fmt, autoconvert, period_hns) = open_shared(device, &mix, Direction::Render, false, false, buffer_hns)?;
        let info = info_for(&name, &mix, hns_to_frames(period_hns, SAMPLE_RATE), false, autoconvert, false);
        Self::finish(client, &fmt, info)
    }

    /// The keep-alive: the mix format as is, at the default period (never a small one: that would
    /// switch every stream on the endpoint).
    pub(crate) fn open_native(device: &Device, buffer_hns: i64) -> Result<Self, WasapiError> {
        let name = device.get_friendlyname().unwrap_or_default();
        let mut client = device.get_iaudioclient()?;
        let mix = client.get_mixformat()?;
        let (def_hns, _) = client.get_device_period()?;
        client.initialize_client(&mix, &Direction::Render, &StreamMode::PollingShared {
            autoconvert: false,
            buffer_duration_hns: buffer_hns.max(2 * def_hns),
        })?;
        let info = info_for(&name, &mix, hns_to_frames(def_hns, mix.get_samplespersec()), false, false, false);
        Self::finish(client, &mix, info)
    }

    fn finish(client: AudioClient, fmt: &WaveFormat, info: OpenInfo) -> Result<Self, WasapiError> {
        let render = client.get_audiorenderclient()?;
        let session = session_without_ducking(&client);
        let clock = client.get_audioclock().ok();
        let clock_freq = clock.as_ref().and_then(|c| c.get_frequency().ok()).unwrap_or(0) as f64;
        let buffer_frames = client.get_buffer_size()? as usize;
        let block_align = fmt.get_blockalign() as usize;
        Ok(Self {
            client,
            render,
            clock: clock.filter(|_| clock_freq > 0.0),
            clock_freq,
            qpc: Qpc::new(),
            _session: session,
            format: sample_format(fmt),
            channels: fmt.get_nchannels().max(1) as usize,
            block_align,
            rate: fmt.get_samplespersec() as f64,
            buffer_frames,
            scratch: vec![0u8; buffer_frames * block_align].into_boxed_slice(),
            zeros: vec![0u8; buffer_frames * block_align].into_boxed_slice(),
            info,
        })
    }

    pub(crate) fn start(&self) -> Result<(), Hr> {
        self.client.start_stream().map_err(hr)
    }

    pub(crate) fn stop(&self) {
        let _ = self.client.stop_stream();
    }

    /// The device period in the stream's frames.
    pub(crate) fn period_frames(&self) -> usize {
        self.info.period_frames
    }
}

impl RenderSink for RenderStream {
    fn take_late(&mut self) -> u64 {
        0
    }
    // A shared stream: the engine drains it and plays its own silence, so there's nothing to take.
    fn take_underruns(&mut self) -> u64 {
        0
    }
    fn take_silence(&mut self) -> u64 {
        0
    }
    fn buffer_frames(&self) -> usize {
        self.buffer_frames
    }

    fn padding(&mut self) -> Result<usize, Hr> {
        self.client.get_current_padding().map(|p| p as usize).map_err(hr)
    }

    fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
        let Some(format) = self.format else { return Err(hr::AUDCLNT_E_UNSUPPORTED_FORMAT) };
        let n = mono.len().min(self.buffer_frames);
        let bytes = n * self.block_align;
        let Some(dst) = self.scratch.get_mut(..bytes) else { return Err(hr::AUDCLNT_E_BUFFER_SIZE_ERROR) };
        from_mono(format, self.channels, &mono[..n], dst);
        self.render.write_to_device(n, dst, None).map_err(hr)
    }

    fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
        let n = frames.min(self.buffer_frames);
        let Some(zeros) = self.zeros.get(..n * self.block_align) else { return Err(hr::AUDCLNT_E_BUFFER_SIZE_ERROR) };
        let flags = BufferFlags { data_discontinuity: false, silent: true, timestamp_error: false };
        self.render.write_to_device(n, zeros, Some(flags)).map_err(hr)
    }

    fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
        read_clock(self.clock.as_ref(), self.rate, self.clock_freq, &self.qpc)
    }
}

/// One `AudioClock` reading in frames at `rate`, stamped with the time it was taken; `None` without a clock, and while the
/// position reads 0 (it does for a few ms after start).
fn read_clock(clock: Option<&AudioClock>, rate: f64, freq: f64, qpc: &Qpc) -> Result<Option<ClockReading>, Hr> {
    let Some(clock) = clock else { return Ok(None) };
    let (pos, at) = clock.get_position().map_err(hr)?;
    let now = qpc.now_hns();
    if pos == 0 {
        return Ok(None);
    }
    Ok(Some(ClockReading { pos_frames: pos as f64 * rate / freq, qpc_hns: at as f64, now_hns: now as f64 }))
}

/// An event-driven exclusive-mode render stream's device end (exclusive.rs, [`EventRender`]): its two buffers of one
/// period, the one the device doesn't play handed over whole at each of its events.
pub(crate) struct ExclusiveRenderDevice {
    client: AudioClient,
    render: AudioRenderClient,
    event: Handle,
    clock: Option<AudioClock>,
    clock_freq: f64,
    qpc: Qpc,
    _session: Option<AudioSessionControl>,
    format: SampleFormat,
    channels: usize,
    period: usize,
    scratch: Box<[u8]>,
    pub(crate) info: OpenInfo,
}

impl ExclusiveRenderDevice {
    /// The output in exclusive mode, or why not (the caller falls back to shared mode).
    pub(crate) fn open(device: &Device, floor: usize) -> Result<Self, ExclusiveRefusal> {
        let refused = |e: WasapiError| ExclusiveRefusal::Refused(hr_of(&e));
        let name = device.get_friendlyname().unwrap_or_default();
        let mix = device.get_iaudioclient().and_then(|c| c.get_mixformat()).map_err(refused)?;
        let (client, fmt, kind, period) = open_exclusive(device, Direction::Render, &mix, floor)?;
        let format = sample_format(&fmt).ok_or(ExclusiveRefusal::NoFormat)?;
        // After initialise, before start.
        let event = client.set_get_eventhandle().map_err(refused)?;
        let render = client.get_audiorenderclient().map_err(refused)?;
        let session = session_without_ducking(&client);
        let clock = client.get_audioclock().ok();
        let clock_freq = clock.as_ref().and_then(|c| c.get_frequency().ok()).unwrap_or(0) as f64;
        let block_align = fmt.get_blockalign() as usize;
        Ok(Self {
            client,
            render,
            event,
            clock: clock.filter(|_| clock_freq > 0.0),
            clock_freq,
            qpc: Qpc::new(),
            _session: session,
            format,
            channels: fmt.get_nchannels().max(1) as usize,
            period,
            scratch: vec![0u8; period * block_align].into_boxed_slice(),
            info: exclusive_info(&name, &fmt, period, kind),
        })
    }

    pub(crate) fn start(&self) -> Result<(), Hr> {
        self.client.start_stream().map_err(hr)
    }

    pub(crate) fn stop(&self) {
        let _ = self.client.stop_stream();
    }

    /// The device's event (it wants its next buffer). Valid while the device lives.
    pub(crate) fn event(&self) -> HANDLE {
        self.event.as_handle()
    }

    /// Whether the event has fired since it was last waited on (and resets it: it's an auto-reset event).
    pub(crate) fn signalled(&self) -> bool {
        self.event.wait_for_event(0).is_ok()
    }
}

impl PeriodDevice for ExclusiveRenderDevice {
    fn period(&self) -> usize {
        self.period
    }

    fn hand(&mut self, frames: &[f32]) -> Result<(), Hr> {
        let frames = frames.get(..self.period).unwrap_or(frames);
        from_mono(self.format, self.channels, frames, &mut self.scratch);
        // A whole buffer, as exclusive mode wants each event (`EventRender` hands exactly a period).
        self.render.write_to_device(self.period, &self.scratch, None).map_err(hr)
    }

    fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
        read_clock(self.clock.as_ref(), SAMPLE_RATE as f64, self.clock_freq, &self.qpc)
    }

    fn now_hns(&mut self) -> f64 {
        self.qpc.now_hns() as f64
    }
}

/// The output stream: shared mode (polled, written at the mic's wakes), or exclusive mode (event-driven: the pipeline
/// fills [`EventRender`]'s ring, and each device event moves a period from it to the device).
pub(crate) enum Output {
    Shared(RenderStream),
    Exclusive(EventRender<ExclusiveRenderDevice>),
}

impl Output {
    /// The output as `IoConfig` asks for it: exclusive mode first when asked (at `floor` frames or longer: the exclusive
    /// period guard's, 0 for none), else (or when the device refuses, with the note) shared mode.
    pub(crate) fn open(device: &Device, exclusive: bool, buffer_hns: i64, floor: usize) -> Result<(Self, Option<Warning>), WasapiError> {
        let name = device.get_friendlyname().unwrap_or_default();
        let (mut out, warning) = exclusive_or_shared(
            exclusive,
            StreamRole::Output,
            &name,
            || ExclusiveRenderDevice::open(device, floor).map(|d| Output::Exclusive(EventRender::new(d, RING_FRAMES))),
            || RenderStream::open(device, buffer_hns).map(Output::Shared),
        )?;
        if let (Output::Shared(s), Some(Warning::ExclusiveUnavailable { why, .. })) = (&mut out, &warning) {
            s.info.refused = Some(*why);
        }
        Ok((out, warning))
    }

    pub(crate) fn info(&self) -> &OpenInfo {
        match self {
            Output::Shared(s) => &s.info,
            Output::Exclusive(e) => &e.device().info,
        }
    }

    /// The device period in frames: the shared engine's pass, or the exclusive buffer.
    pub(crate) fn period_frames(&self) -> usize {
        match self {
            Output::Shared(s) => s.period_frames(),
            Output::Exclusive(e) => e.period(),
        }
    }

    pub(crate) fn is_exclusive(&self) -> bool {
        matches!(self, Output::Exclusive(_))
    }

    /// Records why an output asked for in exclusive mode was opened shared without trying (`audio::output_exclusive`), for
    /// its readout.
    pub(crate) fn withheld(&mut self, why: ExclusiveRefusal) {
        if let Output::Shared(s) = self {
            s.info.refused = Some(why);
        }
    }

    /// Starts the stream. In exclusive mode the first buffer (the prefill's zeros) goes in before the start, as WASAPI wants,
    /// and the first event's answer is timed from the start.
    pub(crate) fn start(&mut self) -> Result<(), Hr> {
        match self {
            Output::Shared(s) => s.start(),
            Output::Exclusive(e) => {
                e.preroll()?;
                e.device().start()?;
                e.started();
                Ok(())
            }
        }
    }

    pub(crate) fn stop(&self) {
        match self {
            Output::Shared(s) => s.stop(),
            Output::Exclusive(e) => e.device().stop(),
        }
    }

    /// The event the audio thread waits on besides the mic's: the exclusive device's. `None` in shared mode (polled).
    pub(crate) fn event(&self) -> Option<HANDLE> {
        match self {
            Output::Shared(_) => None,
            Output::Exclusive(e) => Some(e.device().event()),
        }
    }

    /// Answers the exclusive device's event: when the wait said it fired (`signalled`), or it has since. A no-op in shared
    /// mode.
    pub(crate) fn service(&mut self, signalled: bool) -> Result<(), Hr> {
        match self {
            Output::Shared(_) => Ok(()),
            Output::Exclusive(e) => {
                if signalled || e.device().signalled() {
                    e.service()
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl RenderSink for Output {
    #[inline]
    fn buffer_frames(&self) -> usize {
        match self {
            Output::Shared(s) => s.buffer_frames(),
            Output::Exclusive(e) => e.buffer_frames(),
        }
    }
    #[inline]
    fn padding(&mut self) -> Result<usize, Hr> {
        match self {
            Output::Shared(s) => s.padding(),
            Output::Exclusive(e) => e.padding(),
        }
    }
    #[inline]
    fn write(&mut self, mono: &[f32]) -> Result<(), Hr> {
        match self {
            Output::Shared(s) => s.write(mono),
            Output::Exclusive(e) => e.write(mono),
        }
    }
    #[inline]
    fn write_silence(&mut self, frames: usize) -> Result<(), Hr> {
        match self {
            Output::Shared(s) => s.write_silence(frames),
            Output::Exclusive(e) => e.write_silence(frames),
        }
    }
    #[inline]
    fn clock(&mut self) -> Result<Option<ClockReading>, Hr> {
        match self {
            Output::Shared(s) => s.clock(),
            Output::Exclusive(e) => e.clock(),
        }
    }
    #[inline]
    fn take_underruns(&mut self) -> u64 {
        match self {
            Output::Shared(s) => s.take_underruns(),
            Output::Exclusive(e) => e.take_underruns(),
        }
    }
    #[inline]
    fn take_silence(&mut self) -> u64 {
        match self {
            Output::Shared(s) => s.take_silence(),
            Output::Exclusive(e) => e.take_silence(),
        }
    }
    #[inline]
    fn take_late(&mut self) -> u64 {
        match self {
            Output::Shared(s) => s.take_late(),
            Output::Exclusive(e) => e.take_late(),
        }
    }
}

/// A silent render stream on the reference endpoint, so its loopback never stops delivering.
pub(crate) struct KeepAlive {
    stream: RenderStream,
}

impl KeepAlive {
    pub(crate) fn open(device: &Device) -> Result<Self, WasapiError> {
        Ok(Self { stream: RenderStream::open_native(device, 1_000_000)? })
    }

    /// Fills the buffer with silence. Called on each wake that processes a block (performance/F3),
    /// and once before start.
    pub(crate) fn top_up(&mut self) -> Result<(), Hr> {
        let padding = self.stream.padding()?;
        let n = self.stream.buffer_frames().saturating_sub(padding);
        if n > 0 { self.stream.write_silence(n) } else { Ok(()) }
    }

    pub(crate) fn start(&self) -> Result<(), Hr> {
        self.stream.start()
    }

    pub(crate) fn stop(&self) {
        self.stream.stop()
    }
}

fn info_for(name: &str, mix: &WaveFormat, period_frames: usize, low_latency: bool, autoconvert: bool, raw: bool) -> OpenInfo {
    let rate = if autoconvert || low_latency { SAMPLE_RATE } else { mix.get_samplespersec().max(1) };
    OpenInfo {
        name: name.to_owned(),
        sample_rate: mix.get_samplespersec(),
        channels: mix.get_nchannels(),
        period_frames,
        period_ms: period_frames as f32 * 1000.0 / rate as f32,
        low_latency,
        autoconvert,
        raw,
        exclusive: None,
        refused: None,
    }
}

/// How an exclusive stream was opened: at 48 kHz in `fmt`, with a buffer (the period) of `period_frames`.
fn exclusive_info(name: &str, fmt: &WaveFormat, period_frames: usize, kind: ExclusiveFormat) -> OpenInfo {
    OpenInfo {
        name: name.to_owned(),
        sample_rate: fmt.get_samplespersec(),
        channels: fmt.get_nchannels(),
        period_frames,
        period_ms: period_frames as f32 * 1000.0 / SAMPLE_RATE as f32,
        low_latency: false,
        autoconvert: false,
        raw: false,
        exclusive: Some(kind),
        refused: None,
    }
}

/// The formats tried in exclusive mode (exclusive.rs, best first), at 48 kHz and the mix's channel count and mask.
fn exclusive_candidates(mix: &WaveFormat) -> impl Iterator<Item = (WaveFormat, ExclusiveFormat)> + '_ {
    let channels = mix.get_nchannels().max(1) as usize;
    let mask = mix.get_dwchannelmask();
    ExclusiveFormat::ALL.into_iter().map(move |kind| {
        let (store, valid, float) = kind.bits();
        let st = if float { SampleType::Float } else { SampleType::Int };
        (WaveFormat::new(store, valid, &st, SAMPLE_RATE as usize, channels, (mask != 0).then_some(mask)), kind)
    })
}

/// An event-driven exclusive-mode client on `device` (exclusive.rs): each format of [`ExclusiveFormat::ALL`] it takes (all of
/// them when the check takes none: [`formats_to_try`]), best first, at each of [`exclusive_periods`] (the choice, the
/// device's minimum, its default), until one initialises ([`first_exclusive`]: a refused period tries the next period, a
/// refused format the next format, anything else ends it).
/// Returns the client, its format, which one that is, and the period in frames (its buffer). Not real-time.
fn open_exclusive(
    device: &Device, dir: Direction, mix: &WaveFormat, floor: usize,
) -> Result<(AudioClient, WaveFormat, ExclusiveFormat, usize), ExclusiveRefusal> {
    let refused = |e: WasapiError| ExclusiveRefusal::Refused(hr_of(&e));
    let probe = device.get_iaudioclient().map_err(refused)?;
    // The quirks retry a format as a plain WAVEFORMATEX and with other channel masks; the accepted one comes back. None
    // accepted: the initialise says why (in use, not allowed, or unsupported after all).
    let accepted: Vec<(WaveFormat, ExclusiveFormat)> = exclusive_candidates(mix)
        .filter_map(|(want, kind)| probe.is_supported_exclusive_with_quirks(&want).ok().map(|f| (f, kind)))
        .collect();
    let formats = formats_to_try(accepted, || exclusive_candidates(mix).collect());
    let (default_hns, min_hns) = probe.get_device_period().map_err(refused)?;
    drop(probe);
    first_exclusive(&formats, &exclusive_periods(default_hns, min_hns, floor), |(fmt, kind), period_hns| {
        init_exclusive(device, fmt, &dir, period_hns).map(|(client, frames)| (client, fmt.clone(), *kind, frames))
    })
}

/// One event-driven exclusive client in `fmt` at `period_hns`, on a fresh client, retried once at the size the driver gives
/// when it wants its buffer aligned (Microsoft's recovery: the failed client's `GetBufferSize`, and a fresh client).
/// Returns the client and its buffer (the period) in frames.
fn init_exclusive(device: &Device, fmt: &WaveFormat, dir: &Direction, mut period_hns: i64) -> Result<(AudioClient, usize), Hr> {
    let code = |e: WasapiError| hr_of(&e);
    let mut realigned = false;
    loop {
        let mut client = device.get_iaudioclient().map_err(code)?;
        match client.initialize_client(fmt, dir, &StreamMode::EventsExclusive { period_hns }) {
            Ok(()) => {
                let frames = client.get_buffer_size().map_err(code)? as usize;
                return Ok((client, frames));
            }
            Err(e) if hr_of(&e) == hr::AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED && !realigned => {
                let frames = client.get_buffer_size().map_err(code)?;
                period_hns = calculate_period_100ns(i64::from(frames), i64::from(SAMPLE_RATE));
                realigned = true;
            }
            Err(e) => return Err(hr_of(&e)),
        }
    }
}

/// IAudioClient3 at the smallest period that divides the block; `None` (with nothing left
/// initialised) when the driver doesn't offer one below its default or refuses it.
fn open_low_latency(device: &Device, mix: &WaveFormat, raw: bool) -> Option<(AudioClient, u32)> {
    // IAudioClient3 can't convert: only when the engine already runs at 48 kHz.
    let (_, autoconvert) = stream_format(mix);
    if autoconvert {
        return None;
    }
    let mut client = new_client(device, raw).ok()?;
    let p = client.get_shared_mode_engine_period(mix).ok()?;
    let period = pick_period(p.default, p.fundamental, p.min, p.max, BLOCK as u32)?;
    // e.g. AUDCLNT_E_ENGINE_PERIODICITY_LOCKED: the caller falls back with a fresh client.
    client.initialize_shared_audio_stream(mix, TimingMode::Events, period).ok()?;
    // Another client may hold the engine at a different period: report what applies.
    let actual = client.get_current_shared_mode_engine_period().map(|(_, f)| f).unwrap_or(period);
    Some((client, actual))
}

/// `initialize_client` in shared mode at the format rule's format. Returns the client, the
/// format, whether WASAPI converts, and the device's default period.
fn open_shared(
    device: &Device, mix: &WaveFormat, dir: Direction, raw: bool, events: bool, buffer_hns: i64,
) -> Result<(AudioClient, WaveFormat, bool, i64), WasapiError> {
    let mut client = new_client(device, raw)?;
    let (fmt, autoconvert) = stream_format(mix);
    let (def_hns, _) = client.get_device_period()?;
    let buffer_duration_hns = buffer_hns.max(2 * def_hns);
    let mode = if events {
        StreamMode::EventsShared { autoconvert, buffer_duration_hns }
    } else {
        StreamMode::PollingShared { autoconvert, buffer_duration_hns }
    };
    client.initialize_client(&fmt, &dir, &mode)?;
    Ok((client, fmt, autoconvert, def_hns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_divides_the_block() {
        // A typical HDAudio endpoint: 480 default, 48 fundamental, 48..480.
        assert_eq!(pick_period(480, 48, 48, 480, 480), Some(48));
        // Minimum 128 in steps of 32: 160 is the first that divides 480.
        assert_eq!(pick_period(480, 32, 128, 480, 480), Some(160));
        // Nothing below the default: VB-Cable and most USB mics.
        assert_eq!(pick_period(480, 480, 480, 480, 480), None);
        // Nothing that divides 480.
        assert_eq!(pick_period(480, 7, 7, 400, 480), None);
        assert_eq!(pick_period(480, 0, 0, 480, 480), None);
        // min not a multiple of the fundamental: rounds up.
        assert_eq!(pick_period(480, 48, 50, 480, 480), Some(96));
    }

    #[test]
    fn frames_from_hns() {
        assert_eq!(hns_to_frames(100_000, 48_000), 480);
        assert_eq!(hns_to_frames(30_000, 48_000), 144);
        assert_eq!(hns_to_frames(100_000, 44_100), 441);
        assert_eq!(hns_to_frames(-5, 48_000), 0);
    }

    #[test]
    fn format_rule() {
        let stereo48 = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
        let (f, conv) = stream_format(&stereo48);
        assert!(!conv);
        assert_eq!((f.get_samplespersec(), f.get_nchannels()), (48_000, 2));
        assert_eq!(sample_format(&f), Some(SampleFormat::F32));

        let s16_48 = WaveFormat::new(16, 16, &SampleType::Int, 48_000, 1, None);
        assert!(!stream_format(&s16_48).1);
        assert_eq!(sample_format(&s16_48), Some(SampleFormat::I16));

        let at44 = WaveFormat::new(32, 32, &SampleType::Float, 44_100, 6, Some(0x3F));
        let (f, conv) = stream_format(&at44);
        assert!(conv);
        assert_eq!((f.get_samplespersec(), f.get_nchannels(), f.get_dwchannelmask()), (48_000, 6, 0x3F));
        assert_eq!(sample_format(&f), Some(SampleFormat::F32));

        let s24 = WaveFormat::new(32, 24, &SampleType::Int, 48_000, 2, None);
        assert_eq!(sample_format(&s24), Some(SampleFormat::I24In32), "24-in-32: its own encoding");
        let s32 = WaveFormat::new(32, 32, &SampleType::Int, 48_000, 2, None);
        assert_eq!(sample_format(&s32), Some(SampleFormat::I32));
    }

    #[test]
    fn info_reports_the_period_at_the_stream_rate() {
        let at44 = WaveFormat::new(32, 32, &SampleType::Float, 44_100, 2, None);
        let i = info_for("Speakers", &at44, 480, false, true, false);
        assert_eq!(i.sample_rate, 44_100);
        assert!((i.period_ms - 10.0).abs() < 1e-6);
        let s = i.stream_info();
        assert_eq!(s.device_name, "Speakers");
        assert!(s.autoconvert && !s.raw && !s.low_latency && s.exclusive.is_none());
        let at48 = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
        let i = info_for("Mic", &at48, 144, true, false, true);
        assert!((i.period_ms - 3.0).abs() < 1e-6);
        // Exclusive: the device format is the stream's, and the status names it.
        let s16 = WaveFormat::new(16, 16, &SampleType::Int, 48_000, 2, None);
        let e = exclusive_info("CABLE Input", &s16, 96, ExclusiveFormat::I16);
        assert!((e.period_ms - 2.0).abs() < 1e-6 && !e.raw && !e.low_latency && !e.autoconvert);
        assert_eq!(e.stream_info().exclusive, Some("16-bit"));
        let refused = OpenInfo { refused: Some(ExclusiveRefusal::Refused(hr::AUDCLNT_E_DEVICE_IN_USE)), ..i };
        assert!(refused.stream_info().exclusive_refused.is_some_and(|r| r.starts_with("another app is using it")));
        assert_eq!(sample_format(&s16), Some(SampleFormat::I16));
    }

    /// Exclusive mode tries 32-bit float, 24 bits in 32 and 16-bit, at 48 kHz and the mix's channels and mask, whatever the
    /// mix's rate (exclusive mode can't convert), and each reads as the converter's format.
    #[test]
    fn exclusive_formats_are_48_khz_at_the_mix_channels() {
        let mix = WaveFormat::new(32, 32, &SampleType::Float, 44_100, 2, Some(0x3));
        let c: Vec<(WaveFormat, ExclusiveFormat)> = exclusive_candidates(&mix).collect();
        assert_eq!(c.iter().map(|(_, k)| *k).collect::<Vec<_>>(), ExclusiveFormat::ALL);
        for (f, _) in &c {
            assert_eq!((f.get_samplespersec(), f.get_nchannels(), f.get_dwchannelmask()), (48_000, 2, 0x3));
        }
        assert_eq!(sample_format(&c[0].0), Some(SampleFormat::F32));
        assert_eq!((c[1].0.get_bitspersample(), c[1].0.get_validbitspersample()), (32, 24));
        assert_eq!(sample_format(&c[1].0), Some(SampleFormat::I24In32), "24-in-32: the low byte padding, written as zero");
        assert_eq!(sample_format(&c[2].0), Some(SampleFormat::I16));
    }

    #[test]
    fn error_codes_without_formatting() {
        assert_eq!(hr_of(&WasapiError::UnsupportedFormat), hr::AUDCLNT_E_UNSUPPORTED_FORMAT);
        assert_eq!(hr_of(&WasapiError::ClientNotInit), hr::AUDCLNT_E_NOT_INITIALIZED);
        assert_eq!(hr_of(&WasapiError::DataLengthTooShort { received: 1, expected: 2 }), hr::AUDCLNT_E_BUFFER_SIZE_ERROR);
        assert_eq!(hr_of(&WasapiError::EventTimeout), hr::E_FAIL);
        let w = windows::core::Error::from(windows::core::HRESULT(hr::AUDCLNT_E_DEVICE_INVALIDATED));
        assert_eq!(hr_of(&WasapiError::Windows(w)), hr::AUDCLNT_E_DEVICE_INVALIDATED);
    }
}
