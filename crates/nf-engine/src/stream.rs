//! WASAPI stream wrappers (engine.md "stream"). Each opens one shared-mode stream and converts at
//! the edge to the app's 48 kHz mono f32.
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
    AudioCaptureClient, AudioClient, AudioClientProperties, AudioClock, AudioRenderClient, AudioSessionControl,
    BufferFlags, Device, Direction, Handle, SampleType, StreamMode, StreamOption, TimingMode, WasapiError, WaveFormat,
};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::STGM_READ;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToBoolean};
use windows::core::GUID;

use crate::api::StreamInfo;
use crate::convert::{SampleFormat, from_mono, to_mono};
use crate::errors::hr;
use crate::pipeline::{CaptureSource, ClockReading, Hr, Packet, RenderSink};
use crate::timing::Qpc;

/// `PKEY_Devices_AudioDevice_RawProcessingSupported` (copied to avoid the EnhancedStorage feature).
const PKEY_RAW_SUPPORTED: PROPERTYKEY =
    PROPERTYKEY { fmtid: GUID::from_u128(0x8943b373_388c_4395_b557_bc6dbaffafdb), pid: 2 };

/// The raw HRESULT behind a wasapi error, without formatting anything. The crate's own (non-COM)
/// errors map to the nearest AUDCLNT code.
pub(crate) fn hr_of(e: &WasapiError) -> i32 {
    match e {
        WasapiError::Windows(w) | WasapiError::RegisterNotifications(w) => w.code().0,
        WasapiError::DeviceNotFound(_) => hr::E_NOTFOUND,
        WasapiError::UnsupportedFormat | WasapiError::UnsupportedSubformat(_) => hr::AUDCLNT_E_UNSUPPORTED_FORMAT,
        WasapiError::ClientNotInit => hr::AUDCLNT_E_NOT_INITIALIZED,
        WasapiError::DataLengthMismatch { .. } | WasapiError::DataLengthTooShort { .. } => {
            hr::AUDCLNT_E_BUFFER_SIZE_ERROR
        }
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
        }
    }
}

/// Mic options from `IoConfig`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MicOptions {
    pub low_latency: bool,
    pub raw: bool,
}

/// One shared-mode capture stream: the mic (event-driven), the latency test's output capture
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
    /// The mic: RAW where supported (and asked), IAudioClient3's smallest suitable period where the
    /// driver allows (and asked, and the mix is 48 kHz), each falling back.
    pub(crate) fn open_mic(device: &Device, opts: MicOptions) -> Result<Self, WasapiError> {
        let name = device.get_friendlyname().unwrap_or_default();
        let mix = device.get_iaudioclient()?.get_mixformat()?;
        let raw_options: &[bool] = if opts.raw && raw_supported(device) { &[true, false] } else { &[false] };
        let mut last = WasapiError::UnsupportedFormat;
        for &raw in raw_options {
            if opts.low_latency
                && let Some((client, period)) = open_low_latency(device, &mix, raw)
            {
                let info = info_for(&name, &mix, period as usize, true, false, raw);
                return Self::finish(client, &mix, true, info);
            }
            match open_shared(device, &mix, Direction::Capture, raw, true, 400_000) {
                Ok((client, fmt, autoconvert, period_hns)) => {
                    let info = info_for(&name, &mix, hns_to_frames(period_hns, SAMPLE_RATE), false, autoconvert, raw);
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
        let (client, fmt, autoconvert, period_hns) =
            open_shared(device, &mix, Direction::Capture, false, false, 2_000_000)?;
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
        client.initialize_client(
            &mix,
            &Direction::Render,
            &StreamMode::PollingShared { autoconvert: false, buffer_duration_hns: buffer_hns.max(2 * def_hns) },
        )?;
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
        let Some(clock) = &self.clock else { return Ok(None) };
        let (pos, qpc) = clock.get_position().map_err(hr)?;
        let now = self.qpc.now_hns();
        if pos == 0 {
            // The position reads 0 for a few ms after start.
            return Ok(None);
        }
        Ok(Some(ClockReading { pos_frames: pos as f64 * self.rate / self.clock_freq, qpc_hns: qpc as f64, now_hns: now as f64 }))
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
    device: &Device,
    mix: &WaveFormat,
    dir: Direction,
    raw: bool,
    events: bool,
    buffer_hns: i64,
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
        assert_eq!(sample_format(&s24), Some(SampleFormat::I32), "24-in-32 reads as i32");
    }

    #[test]
    fn info_reports_the_period_at_the_stream_rate() {
        let at44 = WaveFormat::new(32, 32, &SampleType::Float, 44_100, 2, None);
        let i = info_for("Speakers", &at44, 480, false, true, false);
        assert_eq!(i.sample_rate, 44_100);
        assert!((i.period_ms - 10.0).abs() < 1e-6);
        let s = i.stream_info();
        assert_eq!(s.device_name, "Speakers");
        assert!(s.autoconvert && !s.raw && !s.low_latency);
        let at48 = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
        let i = info_for("Mic", &at48, 144, true, false, true);
        assert!((i.period_ms - 3.0).abs() < 1e-6);
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
