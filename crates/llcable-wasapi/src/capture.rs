use wasapi::{
    AudioClient, BufferFlags, Device, DeviceEnumerator, Direction, Handle, SampleType, StreamMode, TimingMode, WasapiError, WaveFormat,
    calculate_period_100ns,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_ERROR, IAudioCaptureClient, IAudioRenderClient};

use crate::convert::{SampleFormat, to_stereo};

const RATE: usize = 48_000;

#[derive(Clone, Copy, Debug)]
pub struct CaptureInfo {
    pub channels: usize,
    pub period: usize,
    pub exclusive: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Packet {
    pub frames: usize,
    pub position: Option<u64>,
    pub discontinuity: bool,
}

pub struct Capture {
    capture: IAudioCaptureClient,
    event: Handle,
    client: AudioClient,
    info: CaptureInfo,
    encoding: SampleFormat,
    frames: usize,
    keepalive: Option<KeepAlive>,
}

impl Capture {
    /// Open on a COM MTA thread. First try native exclusive capture at the shortest device period, then a
    /// shared 48 kHz stream, allowing Windows to convert the device rate if needed.
    pub fn open(id: &str) -> Result<Self, String> {
        let en = DeviceEnumerator::new().map_err(|e| e.to_string())?;
        let loopback = id.strip_prefix("loopback:");
        let id = loopback.unwrap_or(id);
        let device = en.get_device(id).map_err(|e| e.to_string())?;
        if (device.get_direction() == Direction::Render) != loopback.is_some() {
            return Err("Select a recording endpoint or a playback monitor.".into());
        }
        let probe = device.get_iaudioclient().map_err(|e| e.to_string())?;
        let mix = probe.get_mixformat().map_err(|e| e.to_string())?;
        let channels = usize::from(mix.get_nchannels());
        if !(1..=2).contains(&channels) {
            return Err(format!("This source has {channels} channels; select a mono or stereo endpoint."));
        }
        let mask = mix.get_dwchannelmask();
        let fmt = |bits, valid, kind| WaveFormat::new(bits, valid, &kind, RATE, channels, (mask != 0).then_some(mask));
        let (default, min) = probe.get_device_period().map_err(|e| e.to_string())?;
        if loopback.is_some() {
            let float = fmt(32, 32, SampleType::Float);
            let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
            // wasapi sets LOOPBACK on a shared capture stream created on a render endpoint.
            client
                .initialize_client(&float, &Direction::Capture, &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: 400_000.max(2 * default),
                })
                .map_err(|e| e.to_string())?;
            let period = (default as f64 * RATE as f64 / 10_000_000.).round().max(1.) as usize;
            let mut result = Self::finish(client, SampleFormat::F32, channels, false, period).map_err(|e| e.to_string())?;
            result.keepalive = Some(KeepAlive::open(&device, &mix).map_err(|e| e.to_string())?);
            return Ok(result);
        }
        for (want, encoding) in [
            (fmt(32, 32, SampleType::Float), SampleFormat::F32),
            (fmt(32, 24, SampleType::Int), SampleFormat::I24In32),
            (fmt(16, 16, SampleType::Int), SampleFormat::I16),
            (fmt(24, 24, SampleType::Int), SampleFormat::I24),
            (fmt(32, 32, SampleType::Int), SampleFormat::I32),
        ] {
            let Ok(accepted) = probe.is_supported_exclusive_with_quirks(&want) else { continue };
            for period in [min, default] {
                if period <= 0 {
                    continue;
                }
                if let Ok(client) = exclusive(&device, &accepted, period)
                    && let Ok(capture) = Self::finish(client, encoding, channels, true, 0)
                {
                    return Ok(capture);
                }
            }
        }
        let float = fmt(32, 32, SampleType::Float);
        let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
        if let Ok(p) = client.get_shared_mode_engine_period(&float)
            && p.min > 0
            && client.initialize_shared_audio_stream(&float, TimingMode::Events, p.min).is_ok()
        {
            let actual = client.get_current_shared_mode_engine_period().map_or(p.min, |(_, frames)| frames);
            return Self::finish(client, SampleFormat::F32, channels, false, actual as usize).map_err(|e| e.to_string());
        }
        client = device.get_iaudioclient().map_err(|e| e.to_string())?;
        client
            .initialize_client(&float, &Direction::Capture, &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 400_000.max(2 * default),
            })
            .map_err(|e| e.to_string())?;
        let period = (default as f64 * RATE as f64 / 10_000_000.).round().max(1.) as usize;
        Self::finish(client, SampleFormat::F32, channels, false, period).map_err(|e| e.to_string())
    }

    fn finish(client: AudioClient, encoding: SampleFormat, channels: usize, exclusive: bool, period: usize) -> Result<Self, WasapiError> {
        let event = client.set_get_eventhandle()?;
        // SAFETY: the client is initialized for capture and GetService returns its associated COM interface.
        let capture = unsafe { client.as_iaudioclient().GetService::<IAudioCaptureClient>()? };
        let frames = client.get_buffer_size()? as usize;
        let info = CaptureInfo { channels, period: if exclusive { frames } else { period }, exclusive };
        Ok(Self { capture, event, client, info, encoding, frames, keepalive: None })
    }

    pub fn info(&self) -> CaptureInfo {
        self.info
    }
    pub fn capacity(&self) -> usize {
        self.frames
    }
    pub fn event(&self) -> HANDLE {
        self.event.as_handle()
    }
    pub fn start(&self) -> Result<(), WasapiError> {
        if let Some(k) = &self.keepalive {
            k.refill()?;
            k.client.start_stream()?;
        }
        self.client.start_stream()
    }
    pub fn stop(&self) {
        let _ = self.client.stop_stream();
        if let Some(k) = &self.keepalive {
            let _ = k.client.stop_stream();
        }
    }
    /// Returns just the frames Windows has delivered. A caller supplies a buffer of at least `capacity()` frames.
    pub fn read(&mut self, dst: &mut [[f32; 2]]) -> Result<Option<Packet>, WasapiError> {
        if dst.len() < self.frames {
            return Err(WasapiError::DataLengthTooShort { received: dst.len(), expected: self.frames });
        }
        if let Some(k) = &self.keepalive {
            k.refill()?;
        }
        let (mut buf, mut frames, mut flags, mut position) = (std::ptr::null_mut(), 0, 0, 0);
        // SAFETY: outputs are valid locals. This is the sole reader on the owning thread.
        // GetBuffer returns zero frames when shared capture has no outstanding packet.
        if let Err(e) = unsafe { self.capture.GetBuffer(&mut buf, &mut frames, &mut flags, Some(&mut position), None) } {
            // An exclusive driver can signal its event without a complete packet. Wait for its next event.
            if self.info.exclusive && e.code() == AUDCLNT_E_BUFFER_ERROR {
                return Ok(None);
            }
            return Err(e.into());
        }
        let frames = frames as usize;
        if frames == 0 {
            return Ok(None);
        }
        if frames > dst.len() {
            // SAFETY: GetBuffer succeeded above. Release the oversized packet before reporting the error.
            unsafe {
                self.capture.ReleaseBuffer(frames as u32)?;
            }
            return Err(WasapiError::DataLengthTooShort { received: dst.len(), expected: frames });
        }
        let flags = BufferFlags::new(flags);
        if flags.silent || buf.is_null() {
            dst[..frames].fill([0.; 2]);
        } else {
            // SAFETY: WASAPI owns this initialized, nonsilent packet through ReleaseBuffer. Decode it
            // directly into the caller's preallocated stereo block, with no intermediate byte copy.
            let bytes = unsafe { std::slice::from_raw_parts(buf, frames * self.info.channels * self.encoding.bytes()) };
            to_stereo(self.encoding, self.info.channels, bytes, &mut dst[..frames]);
        }
        // SAFETY: this releases the complete packet acquired above on this thread.
        unsafe {
            self.capture.ReleaseBuffer(frames as u32)?;
        }
        Ok(Some(Packet { frames, position: (!flags.timestamp_error).then_some(position), discontinuity: flags.data_discontinuity }))
    }
}

/// A shared, silent playback session keeps the monitor's clock ticking while other apps are silent.
struct KeepAlive {
    render: IAudioRenderClient,
    client: AudioClient,
    frames: u32,
}

impl KeepAlive {
    fn open(device: &Device, mix: &WaveFormat) -> Result<Self, WasapiError> {
        let mut client = device.get_iaudioclient()?;
        client.initialize_client(mix, &Direction::Render, &StreamMode::PollingShared {
            autoconvert: false,
            buffer_duration_hns: 1_000_000,
        })?;
        // SAFETY: the initialized render client owns the requested COM service.
        let render = unsafe { client.as_iaudioclient().GetService::<IAudioRenderClient>()? };
        let frames = client.get_buffer_size()?;
        Ok(Self { render, client, frames })
    }
    fn refill(&self) -> Result<(), WasapiError> {
        let n = self.frames.saturating_sub(self.client.get_current_padding()?);
        if n > 0 {
            // SAFETY: n is the currently free space. SILENT tells WASAPI that all samples are zero,
            // so the application never reads or writes the returned pointer.
            unsafe {
                self.render.GetBuffer(n)?;
                self.render.ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
        }
        Ok(())
    }
}

fn exclusive(device: &Device, format: &WaveFormat, mut period: i64) -> Result<AudioClient, WasapiError> {
    let mut align = true;
    loop {
        let mut client = device.get_iaudioclient()?;
        match client.initialize_client(format, &Direction::Capture, &StreamMode::EventsExclusive { period_hns: period }) {
            Ok(()) => return Ok(client),
            Err(WasapiError::Windows(e)) if align && e.code().0 as u32 == 0x88890019 => {
                period = calculate_period_100ns(i64::from(client.get_buffer_size()?), RATE as i64);
                align = false;
            }
            Err(e) => return Err(e),
        }
    }
}
