use wasapi::{AudioClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode, TimingMode, WasapiError, WaveFormat};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::IAudioRenderClient;

/// Event-driven, shared 48 kHz playback. Stereo endpoints preserve L/R. A mono endpoint receives their average.
/// Request the smallest supported shared period. If the device rate differs, use Windows' rate converter.
pub struct Playback {
    render: IAudioRenderClient,
    event: Handle,
    client: AudioClient,
    channels: usize,
    period: usize,
    capacity: usize,
}

impl Playback {
    pub fn open(id: &str) -> Result<Self, String> {
        let device = DeviceEnumerator::new().and_then(|e| e.get_device(id)).map_err(|e| e.to_string())?;
        if device.get_direction() != Direction::Render {
            return Err("Select a playback endpoint for this destination.".into());
        }
        let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
        let mix = client.get_mixformat().map_err(|e| e.to_string())?;
        let channels = usize::from(mix.get_nchannels());
        if !(1..=2).contains(&channels) {
            return Err("For playback select a mono or stereo endpoint.".into());
        }
        let mask = mix.get_dwchannelmask();
        let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, channels, (mask != 0).then_some(mask));
        let period = if let Ok(periods) = client.get_shared_mode_engine_period(&format)
            && periods.min > 0
            && client.initialize_shared_audio_stream(&format, TimingMode::Events, periods.min).is_ok()
        {
            client.get_current_shared_mode_engine_period().map_or(periods.min, |(_, actual)| actual) as usize
        } else {
            client = device.get_iaudioclient().map_err(|e| e.to_string())?;
            let (default, _) = client.get_device_period().map_err(|e| e.to_string())?;
            client
                .initialize_client(&format, &Direction::Render, &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: 400_000.max(default * 2),
                })
                .map_err(|e| e.to_string())?;
            (default as f64 * 48_000. / 10_000_000.).round().max(1.) as usize
        };
        let capacity = client.get_buffer_size().map_err(|e| e.to_string())? as usize;
        if capacity == 0 || period == 0 {
            return Err("The playback endpoint reported an empty buffer.".into());
        }
        let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
        // SAFETY: initialized render client, sole caller, interface remains on this thread.
        let render = unsafe { client.as_iaudioclient().GetService::<IAudioRenderClient>() }.map_err(|e| e.to_string())?;
        Ok(Self { client, render, event, period, capacity, channels })
    }
    pub fn period(&self) -> usize {
        self.period
    }
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn event(&self) -> HANDLE {
        self.event.as_handle()
    }
    pub fn start(&self) -> Result<(), WasapiError> {
        self.client.start_stream()
    }
    pub fn stop(&self) {
        let _ = self.client.stop_stream();
    }
    /// Maintain up to two negotiated periods at WASAPI, regardless of the endpoint's maximum buffer capacity.
    pub fn needed(&self) -> Result<usize, WasapiError> {
        let target = self.capacity.min(self.period.saturating_mul(2));
        Ok(target.saturating_sub(self.client.get_current_padding()? as usize))
    }
    pub fn write(&mut self, packet: &[[f32; 2]]) -> Result<(), WasapiError> {
        if packet.is_empty() {
            return Ok(());
        }
        if packet.len() > self.capacity {
            return Err(WasapiError::DataLengthMismatch { received: packet.len(), expected: self.capacity });
        }
        let n = packet.len() as u32;
        // SAFETY: GetBuffer grants n frames of negotiated f32 output until ReleaseBuffer; copying bytewise also works on
        // drivers that provide only byte alignment. Mono converts into individual little-endian samples in the same buffer.
        unsafe {
            let buffer = self.render.GetBuffer(n)?;
            if self.channels == 2 {
                std::ptr::copy_nonoverlapping(packet.as_ptr().cast::<u8>(), buffer, std::mem::size_of_val(packet));
            } else {
                for (frame, s) in std::slice::from_raw_parts_mut(buffer, packet.len() * 4).as_chunks_mut::<4>().0.iter_mut().zip(packet) {
                    *frame = (0.5 * (s[0] + s[1])).to_le_bytes();
                }
            }
            self.render.ReleaseBuffer(n, 0)?;
        }
        Ok(())
    }
}
