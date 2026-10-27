# wasapi 0.26.0: API reference for noise-filter

Read from the crate source, not the docs.rs pages. Pinned as `wasapi = "=0.26.0"` on `windows 0.62.2` /
`windows-core 0.62.2`, which are the same versions `nf-engine` uses, so the raw COM types match.

**References.** `src/…`, `examples/…` and `tests/…` are relative to
`C:\Users\Administrator\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\wasapi-0.26.0\`.
`W/…` is `windows-0.62.2\src\Windows\Win32\…` in the same registry folder. Lines marked **(MS)** come
from Microsoft's documentation, not from this source; check them before relying on them.

`WasapiRes<T>` is `pub(crate)` (`src/api.rs:70`), so in our code write `Result<T, wasapi::WasapiError>`.

---

## 0. Pitfalls first

1. **None of the COM wrappers are `Send` or `Sync`.** Make them, use them and drop them on one thread
   (§2). Send device ids (`String`) between threads, never the objects.
2. **`read_from_device` returns one packet per call.** Call it until it returns 0. If the slice is too
   short for the packet, the packet is released and its audio is lost (`src/api.rs:2228-2234`). The data
   is copied even when `flags.silent` is set (`:2236-2237`), so zero-fill silent packets yourself.
3. **Hot path:** use `read_from_device` and `write_to_device` with buffers allocated up front.
   `read_from_device_to_deque` can allocate (`:2272`) and panics if `ReleaseBuffer` fails (`:2274`). The
   read, write, padding and buffer-size calls each run `trace!` (`:2068, 2241, 1325, 1335`), so keep the
   `log` max level below Trace. A failed call builds a `windows_core::Error`, which calls
   `GetErrorInfo`, and formatting that error allocates (§15). Don't log errors on the audio thread.
4. **Two places where the docs and the code disagree:**
   - `new_application_loopback_client(pid, false)` captures everything **except** the tree of `pid`. The
     doc says it captures only `pid`.
   - `set_ducking_preference(true)` **opts out** of ducking: the Windows parameter is named `optout`.
5. **Initialisation order:**
   1. `set_properties` (category, RAW) goes before `initialize_*`.
   2. `set_get_eventhandle` goes after `initialize_*` and before `start_stream`, or start fails with
      `AUDCLNT_E_EVENTHANDLE_NOT_SET` (`tests/device/support.rs:270-273`).
   3. After any failed initialise, make a fresh `AudioClient`.
   4. Never call `Initialize` through `as_iaudioclient()` (§16).
6. **Endpoint loopback delivers no packets while nothing is playing.** This comes from the project
   research (`doc/research-2026-10.md` §Windows audio I/O); the crate doesn't cover it. An event wait on a
   loopback stream then times out. Poll it from the mic loop, zero-fill gaps, or run the keep-alive.

---

## 1. Crate surface (`src/lib.rs`)

- Modules `api, capabilities, dataranges, errors, events, waveformat`. Everything is re-exported flat
  (`src/lib.rs:9-14`).
- Re-exports `windows::core::GUID` (`:15`) and `windows::Win32::Media::Audio::{IAudioClient, IMMDevice}`
  (`:21`). `HANDLE` is deliberately not re-exported (`:16-20`). Take it from
  `windows::Win32::Foundation::HANDLE` (0.62.2).
- Re-exports the `SPEAKER_*` channel-position constants (`src/waveformat.rs:12-18`).
- Logs through `log` (`#[macro_use] extern crate log`, `src/lib.rs:23-24`).
- Has no MMCSS, no `IAudioClockAdjustment`/RATEADJUST and no `IAudioEndpointVolume` wrapper. Reach
  these through the windows crate (§16).

---

## 2. COM initialisation and threading

```rust
pub fn initialize_mta() -> HRESULT   // src/api.rs:73  CoInitializeEx(None, COINIT_MULTITHREADED)
pub fn initialize_sta() -> HRESULT   // src/api.rs:78  CoInitializeEx(None, COINIT_APARTMENTTHREADED)
pub fn deinitialize()                // src/api.rs:83  CoUninitialize()
```

- The calls return a raw `HRESULT`, so use `initialize_mta().ok()?`. `?` converts into `WasapiError`
  through `#[from] windows_core::Error` (`src/errors.rs:34`).
  - **(MS)** `S_FALSE` (already initialised in the same mode) passes `.ok()`, because `HRESULT::ok` accepts
    every code `>= 0` (`windows-result-0.4.1/src/hresult.rs:12-14, 34-39`).
  - **(MS)** `RPC_E_CHANGED_MODE` (the thread is already STA) is an error. A UI thread is STA, and the
    crate's own doc example says "Don't do this on a UI thread" (`src/api.rs:800`).
- **One call per thread that touches wasapi objects.** `examples/record.rs` and `examples/loopback.rs`
  call it only on `main` and then run capture on spawned threads, which leans on COM's implicit MTA. Don't
  copy that: call `initialize_mta()` at the top of every engine thread (as
  `examples/record_application.rs:33` does).
- **`deinitialize` only after every wasapi object on that thread has been dropped.** The crate's tests
  never call it because "uninitialising while interface pointers are still alive is undefined behaviour"
  (`tests/device/support.rs:101-107`). Pattern:
  ```rust
  struct Com;                                   // declare first in the thread fn, so it drops last
  impl Com { fn mta() -> Result<Self, wasapi::WasapiError> { wasapi::initialize_mta().ok()?; Ok(Com) } }
  impl Drop for Com { fn drop(&mut self) { wasapi::deinitialize() } }   // only built on success
  ```
- **Apartment rule from the crate docs:** "An `IMMDevice` is neither agile nor marshalable, so it has to
  be used on the apartment it was created on, and the interfaces activated from it inherit that
  restriction" (`src/api.rs:713-716`).
- **The GUI (windows-reactor) pulls in `windows-core 0.100.0`** (`Cargo.lock`), and those types are
  distinct from the 0.62.2 ones. Pass plain Rust data to the GUI, never windows types.

### Send/Sync (auto traits; the crate has no `unsafe impl Send/Sync`, by grep of `src/`)

Every windows interface is `struct X(IUnknown)` with `IUnknown(NonNull<c_void>)`
(`windows-core-0.62.2/src/imp/mod.rs:60-64`, `src/unknown.rs:12`), so neither `Send` nor `Sync`.

| `!Send + !Sync` (hold COM pointers or a raw `HANDLE`) | `Send + Sync` (plain data) |
|---|---|
| `DeviceEnumerator`, `DeviceCollection`, `DeviceCollectionIter`, `Device`, `AudioClient`, `AudioCaptureClient`, `AudioRenderClient`, `AudioClock`, `AudioSessionControl`, `AudioSessionManager`, `AudioSessionEnumerator`, `AudioMeterInformation`, `AudioEffectsManager`, `AcousticEchoCancellationControl`, `CapabilityProbe`, `DeviceEventRegistration`, `EventRegistration`, `Handle` (`HANDLE(pub *mut c_void)`, `W/Foundation/mod.rs:5291`) | `WaveFormat`, `StreamMode`, `ShareMode`, `TimingMode`, `Direction`, `Role`, `SampleType`, `DeviceState`, `SessionState`, `BufferInfo`, `BufferFlags`, `SharedModeEnginePeriod`, `AudioClientProperties`, `StreamCategory`, `StreamOption`, `DataRange`, `HardwareSupport`, `DisconnectReason`, `EventCallbacks`, `DeviceEventCallbacks`, `WasapiError` (`windows_core::Error`'s `ErrorInfo` is `unsafe impl Send/Sync`, `windows-result-0.4.1/src/error.rs:350-351`) |

The callback structs are `Send + Sync` because every closure must be `Fn + Send + Sync + 'static`
(`src/events.rs:51-57, 317-321`).

To share our own quit event between threads, wrap it:
`struct SendHandle(HANDLE); unsafe impl Send for SendHandle {}` (kernel event handles work from any
thread).

---

## 3. Enums and helpers (`src/api.rs`)

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum Direction { Render, Capture }              // :89
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum Role { Console, Multimedia, Communications } // :140
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum StreamMode {                               // :200
    PollingShared   { autoconvert: bool, buffer_duration_hns: i64 },
    PollingExclusive{ buffer_duration_hns: i64, period_hns: i64 },
    EventsShared    { autoconvert: bool, buffer_duration_hns: i64 },
    EventsExclusive { period_hns: i64 },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum ShareMode { Shared, Exclusive }             // :233
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum TimingMode { Polling, Events }              // :242
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum SampleType { Float, Int }                   // :258
#[derive(Debug, Eq, PartialEq)]              pub enum SessionState { Active, Inactive, Expired }  // :275 (no Clone)
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum DeviceState { Active, Disabled, NotPresent, Unplugged } // :297
pub fn calculate_period_100ns(frames: i64, samplerate: i64) -> i64                             // :357
```

- `calculate_period_100ns(480, 48_000) == 100_000` (10 ms). It rounds to the nearest 100 ns (`:358`).
- Conversions:
  - `From<Direction> for EDataFlow` (`:123-135`) and `From<Role> for ERole` (`:176-189`).
  - `TryFrom<EDataFlow>` returns `IllegalDeviceDirection` for `eAll` (`:103-121`).
  - `TryFrom<DEVICE_STATE> for DeviceState` (`:326-345`).
- `Display` is implemented for `Direction`, `Role`, `ShareMode`, `SampleType`, `SessionState` and
  `DeviceState`. **`TimingMode` has no `Display`** (only `Debug`).

---

## 4. Devices

```rust
impl DeviceEnumerator {                                                         // src/api.rs:366
    pub fn new() -> WasapiRes<DeviceEnumerator>                                 // :368 CoCreateInstance(MMDeviceEnumerator)
    pub fn get_device_collection(&self, direction: &Direction) -> WasapiRes<DeviceCollection>   // :375
    pub fn get_default_device(&self, direction: &Direction) -> WasapiRes<Device>                // :388 = Role::Console
    pub fn get_default_device_for_role(&self, direction: &Direction, role: &Role) -> WasapiRes<Device> // :393
    pub fn get_device(&self, device_id: &str) -> WasapiRes<Device>              // :412 queries direction via IMMEndpoint
    pub fn register_notification_callback(&self, callbacks: DeviceEventCallbacks)
        -> WasapiRes<DeviceEventRegistration>                                   // :433 (§13)
}
impl DeviceCollection {                                                         // :473
    pub fn get_nbr_devices(&self) -> WasapiRes<u32>                             // :475
    pub fn get_device_at_index(&self, idx: u32) -> WasapiRes<Device>            // :481
    pub fn get_device_with_name(&self, name: &str) -> WasapiRes<Device>         // :490 exact friendly-name match, else DeviceNotFound
    pub fn get_direction(&self) -> Direction                                    // :504
}
impl<'a> IntoIterator for &'a DeviceCollection { type Item = WasapiRes<Device>; ... } // :530
impl Device {                                                                   // :548
    pub unsafe fn from_raw(device: IMMDevice, direction: Direction) -> Device   // :557 caller guarantees direction
    pub fn from_immdevice(device: IMMDevice) -> WasapiRes<Device>               // :563 safe: queries GetDataFlow
    pub fn get_iaudioclient(&self) -> WasapiRes<AudioClient>                    // :571 Activate::<IAudioClient>; a new client per call
    pub fn get_state(&self) -> WasapiRes<DeviceState>                           // :620
    pub fn get_friendlyname(&self) -> WasapiRes<String>                         // :627 PKEY_Device_FriendlyName  "CABLE Input (VB-Audio Virtual Cable)"
    pub fn get_interface_friendlyname(&self) -> WasapiRes<String>               // :632 PKEY_DeviceInterface_FriendlyName "VB-Audio Virtual Cable"
    pub fn get_description(&self) -> WasapiRes<String>                          // :637 PKEY_Device_DeviceDesc "CABLE Input"
    pub fn get_device_format(&self) -> WasapiRes<WaveFormat>                    // :643 PKEY_AudioEngine_DeviceFormat (control-panel format)
    pub fn get_id(&self) -> WasapiRes<String>                                   // :694 persistent endpoint id
    pub fn get_direction(&self) -> Direction                                    // :702
    pub fn get_iaudiosessionmanager(&self) -> WasapiRes<AudioSessionManager>    // :601
    pub fn get_audiometerinformation(&self) -> WasapiRes<AudioMeterInformation> // :611
    pub fn get_data_ranges(&self) -> WasapiRes<Vec<DataRange>>                  // :596 driver-declared upper bounds
    pub fn as_immdevice(&self) -> &IMMDevice                                    // :719
}
```

- **Only active devices are listed.** `get_device_collection` passes `DEVICE_STATE_ACTIVE`
  (`src/api.rs:379`), so `get_state()` on a listed device is always `Active`. To list
  disabled or unplugged devices, make your own enumerator (the wrapper's is private):
  ```rust
  use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATEMASK_ALL, EDataFlow};
  use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
  let raw: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
  let coll = unsafe { raw.EnumAudioEndpoints(EDataFlow::from(Direction::Capture), DEVICE_STATE(DEVICE_STATEMASK_ALL))? };
  for i in 0..unsafe { coll.GetCount()? } {
      let dev = wasapi::Device::from_immdevice(unsafe { coll.Item(i)? })?;   // then get_state(), get_id(), ...
  }
  ```
  (`DEVICE_STATE(pub u32)` and `DEVICE_STATEMASK_ALL: u32 = 15`, `W/Media/Audio/mod.rs:1848-1849`.)
- **The `for d in &collection` iterator calls `get_nbr_devices().unwrap()` on every step** (`:519`), so a
  COM error panics. Use `get_nbr_devices` plus `get_device_at_index` for code that must not panic.
- **`get_default_device*` returns `WasapiError::Windows`** (E_NOTFOUND, **MS**) when there is no default
  device. It calls `debug!` with the friendly name (`:407`); log arguments are only evaluated when the
  level is enabled.
- **Keep a device by `get_id()`, never by name.** VB-Cable, as the crate's own tests identify it:
  - the interface friendly name is `"VB-Audio Virtual Cable"`;
  - there's a second render endpoint whose description contains "16ch" ("CABLE In 16ch"), which you
    should skip;
  - the capture endpoint is "CABLE Output".

  (`tests/device/support.rs:20-26, 175-197`.)
- **`get_device_format` isn't the mix format.** It "reports whatever the control panel last stored"
  (`tests/device/support.rs:177-179`). For shared mode use `AudioClient::get_mixformat`.
- **Reading `RawProcessingSupported` (for `StreamOption::Raw`):** the crate has no wrapper. Use the raw
  device:
  ```rust
  use windows::Win32::{Foundation::PROPERTYKEY, System::Com::STGM_READ,
      System::Com::StructuredStorage::{PropVariantClear, PropVariantToBoolean}};
  // PKEY_Devices_AudioDevice_RawProcessingSupported, W/Storage/EnhancedStorage/mod.rs:1208 (copied to avoid that feature)
  const PKEY_RAW: PROPERTYKEY = PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0x8943b373_388c_4395_b557_bc6dbaffafdb), pid: 2 };
  fn raw_supported(dev: &wasapi::Device) -> bool { unsafe {
      let Ok(store) = dev.as_immdevice().OpenPropertyStore(STGM_READ) else { return false };
      let Ok(mut v) = store.GetValue(&PKEY_RAW) else { return false };
      let r = PropVariantToBoolean(&v).map(|b| b.as_bool()).unwrap_or(false);
      let _ = PropVariantClear(&mut v);
      r
  } }
  ```
  Features this needs on `windows`: `Win32_UI_Shell_PropertiesSystem` for `OpenPropertyStore`
  (`W/Media/Audio/mod.rs:5383`), plus `Win32_System_Com_StructuredStorage` and `Win32_System_Variant`. wasapi
  already enables them on the same `windows` (unified), but list them in `nf-engine` anyway.

---

## 5. WaveFormat (`src/waveformat.rs`)

```rust
#[derive(Clone)] pub struct WaveFormat { pub wave_fmt: WAVEFORMATEXTENSIBLE }               // :71 (custom Debug)
pub fn new(storebits: usize, validbits: usize, sample_type: &SampleType, samplerate: usize,
           channels: usize, channel_mask: Option<u32>) -> Self                               // :159
pub fn from_waveformatex(wavefmt: WAVEFORMATEX) -> WasapiRes<Self>                           // :209
pub fn to_waveformatex(&self) -> WasapiRes<Self>                                             // :245
pub fn parse_from_blob_bytes(blob: &[u8]) -> WasapiRes<Self>                                 // :97
pub unsafe fn parse(waveformatex: *const WAVEFORMATEX) -> WasapiRes<Self>                    // :135
pub fn as_waveformatex_ref(&self) -> &WAVEFORMATEX                                           // :281
pub fn get_blockalign(&self) -> u32        // :286 bytes per frame
pub fn get_avgbytespersec(&self) -> u32    // :291
pub fn get_bitspersample(&self) -> u16     // :296 container bits
pub fn get_validbitspersample(&self) -> u16// :301
pub fn get_samplespersec(&self) -> u32     // :306
pub fn get_nchannels(&self) -> u16         // :311
pub fn get_dwchannelmask(&self) -> u32     // :319
pub fn get_subformat(&self) -> WasapiRes<SampleType>   // :324 Err(UnsupportedSubformat) for non PCM/float
impl From<WAVEFORMATEXTENSIBLE> for WaveFormat                                               // :334
pub fn make_channelmasks(channels: usize) -> Vec<u32>     // :396 allocates
pub fn make_simple_channelmask(channels: usize) -> u32    // :440
```

- `new` always builds `WAVE_FORMAT_EXTENSIBLE` with `cbSize 22` (`:170-178`). For float use
  `new(32, 32, &SampleType::Float, 48_000, ch, mask)`.
- **The default mono mask is `SPEAKER_FRONT_LEFT` (0x1), not mono.** With `channel_mask: None` the mask
  is `(1 << ch) - 1` (`:188-198`). `KSAUDIO_SPEAKER_MONO` is `SPEAKER_FRONT_CENTER` (`:24`), which is also
  `make_channelmasks(1)[0]` (`:398`). For mono pass `Some(SPEAKER_FRONT_CENTER)` explicitly. Stereo's
  default 0x3 already equals `KSAUDIO_SPEAKER_STEREO`.
  ```rust
  use wasapi::{WaveFormat, SampleType, SPEAKER_FRONT_CENTER};
  let mono48  = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 1, Some(SPEAKER_FRONT_CENTER)); // blockalign 4
  let stereo48 = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);                      // blockalign 8, mask 0x3
  ```
- `to_waveformatex` refuses 24-bit and other ambiguous formats (`:252-254`). It keeps the extensible
  fields and sets `cbSize = 0`, so only the `WAVEFORMATEX` part reaches WASAPI (`:241-277`).
- `from_waveformatex` divides by `channels` (`:220`), so a zero-channel header would panic. Mix formats
  never have zero channels.
- **Samples are interleaved little-endian in the container width.** Frame bytes = `get_blockalign()`.

---

## 6. AudioClient (`src/api.rs:748-1610`)

The struct caches `direction`, `sharemode: Option<ShareMode>`, `timingmode: Option<TimingMode>` and
`bytes_per_frame: Option<usize>` (`:748-754`). The rest of the crate trusts that cache.

```rust
// creation
Device::get_iaudioclient(&self) -> WasapiRes<AudioClient>                                     // :571
pub fn new_application_loopback_client(process_id: u32, include_tree: bool) -> WasapiRes<Self> // :809 (§11)
pub fn from_iaudioclient(client: IAudioClient, direction: Direction) -> Self                   // :940 direction = endpoint's
pub fn as_iaudioclient(&self) -> &IAudioClient                                                 // :967 (§16)
// format and period queries (no initialise needed)
pub fn get_mixformat(&self) -> WasapiRes<WaveFormat>                                           // :972
pub fn is_supported(&self, wave_fmt: &WaveFormat, sharemode: &ShareMode) -> WasapiRes<Option<WaveFormat>> // :1014
pub fn is_supported_exclusive_with_quirks(&self, wave_fmt: &WaveFormat) -> WasapiRes<WaveFormat>  // :1096
pub fn get_device_period(&self) -> WasapiRes<(i64, i64)>               // :1138 (default_hns, min_hns)
pub fn calculate_aligned_period_near(&self, desired_period: i64, align_bytes: Option<u32>,
                                     wave_fmt: &WaveFormat) -> WasapiRes<i64>                  // :1158 exclusive helper
// initialisation
pub fn set_properties(&self, properties: AudioClientProperties) -> WasapiRes<()>               // :1448 IAudioClient2, before init
pub fn initialize_client(&mut self, wavefmt: &WaveFormat, direction: &Direction,
                         stream_mode: &StreamMode) -> WasapiRes<()>                            // :1229
pub fn initialize_shared_audio_stream(&mut self, wavefmt: &WaveFormat, timing_mode: TimingMode,
                                      period_in_frames: u32) -> WasapiRes<()>                  // :1582 IAudioClient3
pub fn set_get_eventhandle(&self) -> WasapiRes<Handle>                                         // :1315
// running
pub fn get_buffer_size(&self) -> WasapiRes<u32>                        // :1323 frames
pub fn get_current_padding(&self) -> WasapiRes<u32>                    // :1333 frames
pub fn get_available_space_in_frames(&self) -> WasapiRes<u32>          // :1341
pub fn start_stream(&self) -> WasapiRes<()>                            // :1366
pub fn stop_stream(&self) -> WasapiRes<()>                             // :1372
pub fn reset_stream(&self) -> WasapiRes<()>                            // :1378
// services (need an initialised client; GetService before Initialize fails, MS)
pub fn get_audiorenderclient(&self) -> WasapiRes<AudioRenderClient>    // :1384
pub fn get_audiocaptureclient(&self) -> WasapiRes<AudioCaptureClient>  // :1393
pub fn get_audiosessioncontrol(&self) -> WasapiRes<AudioSessionControl>// :1403
pub fn get_audioclock(&self) -> WasapiRes<AudioClock>                  // :1409
// cached state
pub fn get_direction(&self) -> Direction                               // :1415
pub fn get_sharemode(&self) -> Option<ShareMode>                       // :1421
pub fn get_timing_mode(&self) -> Option<TimingMode>                    // :1427
// IAudioClient2/3 queries
pub fn get_buffer_size_limits(&self, wavefmt: &WaveFormat, timing_mode: TimingMode) -> WasapiRes<(i64, i64)> // :1497 offload endpoints only
pub fn is_offload_capable(&self, category: StreamCategory) -> WasapiRes<bool>                // :1521
pub fn get_current_shared_mode_engine_period(&self) -> WasapiRes<(WaveFormat, u32)>          // :1531
pub fn get_shared_mode_engine_period(&self, wavefmt: &WaveFormat) -> WasapiRes<SharedModeEnginePeriod> // :1549
// Windows voice-processing AEC (not ours, see §14)
pub fn get_aec_control(&self) -> WasapiRes<AcousticEchoCancellationControl>                  // :1433
pub fn is_aec_supported(&self) -> WasapiRes<bool>                                            // :1455
pub fn get_audio_effects_manager(&self) -> WasapiRes<AudioEffectsManager>                    // :1442
```

### get_mixformat / is_supported
- `get_mixformat` copies the `GetMixFormat` result and frees it with `CoTaskMemFree` (`:972-993`).
  - With `cbSize == 22` and the extensible tag, the struct is used as is.
  - Otherwise it goes through `from_waveformatex`, which only accepts the PCM and IEEE_FLOAT tags.
  - **(MS)** The shared-mode mix format is normally 32-bit float, but handle int anyway.
- **`is_supported(fmt, &ShareMode::Shared)`** has three outcomes (`:1032-1072`):
  - `Ok(None)`: supported as is.
  - `Ok(Some(closest))`: not supported, and Windows suggests `closest` (S_FALSE).
  - `Err(..)`: refused, e.g. `AUDCLNT_E_UNSUPPORTED_FORMAT`.
- **`is_supported(fmt, &ShareMode::Exclusive)`** returns `Ok(None)` or `Err` (`:1020-1031`).
  `is_supported_exclusive_with_quirks` retries the format as `WAVEFORMATEX` and then each mask from
  `make_channelmasks`. It returns the accepted format, whose mask may differ (`:1096-1135`).

### initialize_client (`:1229-1311`): what the crate actually passes to `IAudioClient::Initialize`

| `StreamMode` | share | stream flags the crate sets | `hnsBufferDuration` | `hnsPeriodicity` |
|---|---|---|---|---|
| `PollingShared{autoconvert, buffer_duration_hns}` | SHARED | `AUTOCONVERTPCM\|SRC_DEFAULT_QUALITY` if `autoconvert` | `buffer_duration_hns` | 0 |
| `EventsShared{autoconvert, buffer_duration_hns}` | SHARED | the above `\| EVENTCALLBACK` | `buffer_duration_hns` | 0 |
| `PollingExclusive{buffer_duration_hns, period_hns}` | EXCLUSIVE | none | `buffer_duration_hns` | `period_hns` |
| `EventsExclusive{period_hns}` | EXCLUSIVE | `EVENTCALLBACK` | `period_hns` | `period_hns` |

(Rows from `:1261-1295`.)

- **Direction rules, checked before any Windows call** (`:1249-1260`; `tests/device/errors.rs:54-91`):
  - A Render device initialised with `Direction::Capture` in shared mode adds `AUDCLNT_STREAMFLAGS_LOOPBACK`.
  - The same in exclusive mode gives `Err(WasapiError::LoopbackWithExclusiveMode)`.
  - A Capture device with `Direction::Render` gives `Err(WasapiError::RenderToCaptureDevice)`.
- **No other flags can be set:** no RATEADJUST, no NOPERSIST, no session flags. The session GUID is always
  `None`, which means the process's default session per device (`:1303`). Autoconvert is impossible in
  exclusive mode by type, so `WasapiError::AutomaticFormatConversionInExclusiveMode` is never built.
- **In shared mode the engine owns the period.** "The audio engine decides the period, and this cannot
  be changed" (`:204, :218`). `buffer_duration_hns` sets capacity only. The examples pass
  `get_device_period().1` or `.0` (`examples/record.rs:35-41`, `examples/playsine_events.rs:66-71`), and
  `tests/device/support.rs:307-314` passes the default. For a smaller shared period use
  `initialize_shared_audio_stream`.
- **What a successful initialise caches** (`:1306-1309`):
  - `direction` = the stream direction (Capture for loopback, `tests/device/errors.rs:282-283`);
  - `sharemode`, `timingmode`;
  - `bytes_per_frame = wavefmt.get_blockalign()`.
- **A failed `Initialize` invalidates the client.** "release the client and make a new one"
  (`tests/device/errors.rs:25-30`). A second initialise gives `AUDCLNT_E_ALREADY_INITIALIZED`
  (`:31-52`).
- **Exclusive `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` recovery (MS sample):**
  1. Read `get_buffer_size()` from the failed client.
  2. `calculate_period_100ns(frames, rate)`.
  3. Make a **new** client and initialise it with the **new** period.

  `examples/playnoise_exclusive.rs:90-99` computes `aligned_period` but retries with the old `mode`.
  Don't copy that.

### set_get_eventhandle (`:1315-1319`)
- `CreateEventA(None, bManualReset=false, bInitialState=false, null)`: an auto-reset, unnamed event,
  then `SetEventHandle`.
- Call it once, after initialise and before `start_stream`. Each call creates a new event.
- Only for `TimingMode::Events`. **(MS)** A polling client gets `AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED`.

### Buffer, padding, free space
- `get_buffer_size()` is the buffer capacity in frames.
- `get_current_padding()` is the frames in the buffer: queued-not-played for render, captured-not-read
  for capture.
- `get_available_space_in_frames()` (`:1341-1357`):
  - EXCLUSIVE+Events returns `GetBufferSize` (the device "reports it as entirely full",
    `tests/device/streams.rs:49-60`);
  - other initialised modes return `buffer - padding` (two COM calls);
  - an uninitialised client gives `Err(ClientNotInit)` (a guard in the crate).
- **Shared-mode latency is the padding you keep, not the buffer size.** On render, write only enough to
  hold padding at your target.

### start / stop / reset
- `start_stream`: "For playback, fill the buffer with data before starting the stream" (`:1359-1365`).
  Prefill with silence, only as much as your target fill.
- `reset_stream` on a running stream gives `AUDCLNT_E_NOT_STOPPED`. After stop plus reset the render
  buffer is empty (`tests/device/errors.rs:198-230`).
- **`get_audiorenderclient` and `get_audiocaptureclient` snapshot `bytes_per_frame` when called**
  (`:1384-1400`, `unwrap_or_default()`, so 0 if not initialised). Call them after initialising. A 0 frame
  size makes `read_from_device` divide by zero (`:2205`).

### Client properties: category and RAW (`:1626-1804`)

```rust
#[derive(Copy, Clone, Debug)] pub struct AudioClientProperties(windows::Win32::Media::Audio::AudioClientProperties); // :1631
pub fn new() -> Self                                                        // :1635 cbSize, bIsOffload=FALSE, AudioCategory_Other, AUDCLNT_STREAMOPTIONS_NONE
pub fn set_offload(self, is_offload: bool) -> Self                         // :1645
pub fn set_category<T: Into<AUDIO_STREAM_CATEGORY>>(self, category: T) -> Self   // :1655
pub fn set_option<T: Into<AUDCLNT_STREAMOPTIONS>>(self, option: T) -> Self       // :1668 ORs (accumulates)
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum StreamCategory { Other, ForegroundOnlyMedia /*deprecated*/, Communications,
    Alerts, SoundEffects, GameEffects, GameMedia, GameChat, Speech, Movie, Media, FarFieldSpeech, UniformSpeech, VoiceTyping } // :1722
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum StreamOption { Raw, MatchFormat, Ambisonics }  // :1785
```

```rust
// before initialize_client / initialize_shared_audio_stream:
let props = AudioClientProperties::new()
    .set_category(StreamCategory::Other)          // NOT Communications for our mic (doc/todo/8-audio-io.md)
    .set_option(StreamOption::Raw);               // only if raw_supported(&device), §4
client.set_properties(props)?;                     // casts to IAudioClient2 (:1449)
```

- The `aec` example calls it before `initialize_client` (`examples/aec.rs:37-46`).
- **Don't set a category on the loopback stream.** The example says Communications "is not valid for
  speaker loopback stream" (`examples/aec.rs:38`).
- **(MS)** An endpoint without raw support refuses RAW with `AUDCLNT_E_RAW_MODE_UNSUPPORTED`
  (`W/Media/Audio/mod.rs:1480`). Check the property first.

### IAudioClient3 low-latency shared mode (`:1528-1624`)

```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SharedModeEnginePeriod { pub default: u32, pub fundamental: u32, pub min: u32, pub max: u32 } // frames, :1614
```

- `get_shared_mode_engine_period(&fmt)` and `get_current_shared_mode_engine_period()` work on a client
  that has never been initialised (`tests/device/audioclient23.rs:23-24, 122`). The test asks with the
  mix format.
- **What the crate passes to `initialize_shared_audio_stream`** (`:1582-1609`):
  - the flags are `EVENTCALLBACK` or 0, so there is **no loopback, no autoconvert and no RATEADJUST**,
    and no session GUID (`None`);
  - **(MS)** it accepts only `EVENTCALLBACK` anyway.
  - It sets `sharemode = Shared`, `timingmode` and `bytes_per_frame`, but leaves `direction` alone (the
    device's).
  - It doesn't check direction.
- **Limits:**
  - pass the format the engine runs at (the mix format), because nothing converts it;
  - the period should be a multiple of `fundamental` within `[min, max]` **(MS)**.
- **(MS)** If another client already runs the engine at another period or format, expect
  `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED` or `AUDCLNT_E_ENGINE_FORMAT_LOCKED` (`W/Media/Audio/mod.rs:1462-1463`).
  Re-read `get_current_shared_mode_engine_period()` after initialising.
- A small period on an endpoint switches every stream on it. Small periods are documented only for
  Microsoft's inbox HDAudio driver, so expect VB-Cable and USB mics to report `min == default`
  (`doc/research-2026-10.md` §Windows audio I/O).
- The crate's device test covers render only (`tests/device/audioclient23.rs:180-229`).

---

## 7. Capture (`src/api.rs:2105-2285`)

```rust
pub struct AudioCaptureClient { /* IAudioCaptureClient, sharemode, bytes_per_frame */ }        // :2182
pub fn get_next_packet_size(&self) -> WasapiRes<Option<u32>>       // :2192 None in exclusive mode
pub fn read_from_device(&self, data: &mut [u8]) -> WasapiRes<(u32, BufferInfo)>  // :2204
pub fn read_from_device_to_deque(&self, data: &mut VecDeque<u8>) -> WasapiRes<BufferInfo> // :2247  (not for the hot path)
pub fn get_sharemode(&self) -> Option<ShareMode>                    // :2282

#[derive(Debug)] pub struct BufferInfo { pub flags: BufferFlags, pub index: u64, pub timestamp: u64 } // :2107 (no Clone)
pub fn new(flags: u32, index: u64, timestamp: u64) -> Self   // :2118
pub fn none() -> Self                                        // :2126
#[derive(Debug)] pub struct BufferFlags { pub data_discontinuity: bool, pub silent: bool, pub timestamp_error: bool } // :2137 (no Clone)
pub fn new(flags: u32) -> Self   // :2148   bits: DISCONTINUITY=1, SILENT=2, TIMESTAMP_ERROR=4 (W/Media/Audio/mod.rs:1445-1447)
pub fn none() -> Self            // :2157
pub fn to_u32(&self) -> u32      // :2166   unknown bits dropped (tests/public_api.rs:146)
```

**What `read_from_device` does** (`:2204-2243`):

1. `cap = data.len() / bytes_per_frame` frames. If `cap == 0` it returns `(0, BufferInfo::none())`
   **without calling `GetBuffer`**.
2. One `GetBuffer`. **(MS)** `AUDCLNT_S_BUFFER_EMPTY` is a success code, so it surfaces as `Ok` with 0
   frames.
3. If 0 frames: `ReleaseBuffer(0)` and return `(0, info)`.
4. If the packet doesn't fit, it **releases the whole packet (the audio is lost)** and returns
   `Err(DataLengthTooShort { received: cap_frames, expected: packet_frames })`. Both values are in
   **frames** here, but in **bytes** in `write_to_device_from_deque` (`:2087-2090`).
5. Otherwise it copies `frames * bytes_per_frame` bytes into `data[..]`, releases, and returns
   `(frames, info)`. The bytes past that are untouched.

- **Drain loop:** call until it returns 0 frames (`tests/device/support.rs:430-438`). In shared mode,
  `get_next_packet_size()` gives exactly the next read's frames (`tests/device/streams.rs:208-267`).
- **Buffer sizing:** a packet never exceeds the buffer. Allocate `2 * get_buffer_size() * blockalign`
  once, as the test does (`tests/device/support.rs:413-417`). For process loopback the buffer size is
  junk, so cap it there (`:39-45`).
- **`flags.silent`:** **(MS)** treat the packet as silence. The crate copies whatever bytes are in the
  buffer anyway.
- **`index`** is the device position, in frames, of the packet's first frame (`pu64DevicePosition`,
  `:2219`). **`timestamp`** is the QPC time of that first frame, converted to **100 ns** units
  (`pu64QPCPosition`, `:2220`; units **(MS)**, crate doc `:2112`). Ignore `timestamp` while
  `timestamp_error` is set.
- **Don't rely on `data_discontinuity` to find gaps:** it "is not set in practice by any driver seen so
  far" (`tests/device/streams.rs:116-120`). Check `info.index != prev.index + prev_frames` instead.
- **Exclusive mode:** `get_next_packet_size()` returns `None` (`:2193-2195`). Drive the loop with
  padding or the buffer size.
- **`read_from_device_to_deque`:** `VecDeque::extend` (`:2272`) may reallocate, and `.unwrap()` on
  `ReleaseBuffer` (`:2274`) panics. It also doesn't release a 0-frame `GetBuffer` (`:2263-2266`).

---

## 8. Render (`src/api.rs:2033-2103`)

```rust
pub struct AudioRenderClient { /* IAudioRenderClient, bytes_per_frame */ }                      // :2034
pub fn write_to_device(&self, nbr_frames: usize, data: &[u8], buffer_flags: Option<BufferFlags>) -> WasapiRes<()> // :2044
pub fn write_to_device_from_deque(&self, nbr_frames: usize, data: &mut VecDeque<u8>,
                                  buffer_flags: Option<BufferFlags>) -> WasapiRes<()>          // :2076
```

- `nbr_frames == 0` is an `Ok` no-op (`:2050-2052`).
- **`data.len()` must equal `nbr_frames * blockalign` exactly**, or you get
  `Err(DataLengthMismatch { received, expected })` in bytes (`:2053-2059`). Data is interleaved in the
  initialised format; for f32 mono 48 kHz that's 4 bytes per frame.
- **What a write does:** `GetBuffer(n)`, `copy_from_slice`, then `ReleaseBuffer(n, flags)`
  (`:2060-2067`).
- **A write larger than the free space is refused, and the client survives**
  (`tests/device/errors.rs:136-149`). Check `get_available_space_in_frames()` or padding first.
- **`Some(BufferFlags { silent: true, .. })` sets `AUDCLNT_BUFFERFLAGS_SILENT`,** but the bytes are still
  copied first. Pass a pre-zeroed slice of the right length.
- **No zero-copy path:** the wrapper always copies. For a direct `GetBuffer` pointer, call
  `as_iaudioclient().GetService::<IAudioRenderClient>()` yourself (allowed; only `Initialize` is
  forbidden). The 2-4 KB copy per 10 ms block isn't worth that.
- **The deque variant** refuses short input without consuming any (`tests/device/errors.rs:121-131`).
  It drains with two memcpys (`:2021-2031`). It doesn't allocate, but it needs a `VecDeque`.

---

## 9. Handle (`src/api.rs:2287-2325`)

```rust
pub struct Handle { handle: HANDLE }            // :2288   Drop -> CloseHandle (:2292-2296)
pub fn wait_for_event(&self, timeout_ms: u32) -> WasapiRes<()>   // :2300
pub fn as_handle(&self) -> HANDLE                                 // :2322 borrowed; never close it
```

- `wait_for_event` returns `Err(WasapiError::EventTimeout)` for **any** result other than `WAIT_OBJECT_0`,
  including `WAIT_FAILED` and abandoned (`:2301-2304`). Infinite is `u32::MAX`
  (`windows::Win32::System::Threading::INFINITE`, `W/System/Threading/mod.rs:2145`).
- **Shutdown pattern:** wait on the stream event and our own manual-reset quit event together
  (`tests/device/raw.rs:41-86`):
  ```rust
  use windows::Win32::{Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT}, System::Threading::WaitForMultipleObjects};
  let handles: [HANDLE; 2] = [mic.event.as_handle(), quit.0];
  match unsafe { WaitForMultipleObjects(&handles, false, 200) } {
      r if r == WAIT_OBJECT_0 => { /* drain mic, run DSP, feed outputs */ }
      r if r.0 == WAIT_OBJECT_0.0 + 1 => break,
      r if r == WAIT_TIMEOUT => { /* stalled or invalidated device: notify control thread */ }
      _ => { /* WAIT_FAILED */ }
  }
  ```
  `WaitForMultipleObjects(lphandles: &[HANDLE], bwaitall: bool, dwmilliseconds: u32) -> WAIT_EVENT`
  (`W/System/Threading/mod.rs:1919`). `WAIT_EVENT` derives `PartialEq` (`W/Foundation/mod.rs:10482-10487`).
  The quit event comes from `CreateEventW(None, true, false, None)`, which needs the `Win32_Security`
  feature (`W/System/Threading/mod.rs:249-251`).
- Keep the `Handle` alive until the stream is stopped. Stop explicitly before dropping it.

---

## 10. AudioClock (`src/api.rs:1983-2006`)

```rust
pub fn get_frequency(&self) -> WasapiRes<u64>        // :1991  "the unit for the value is undefined"
pub fn get_position(&self) -> WasapiRes<(u64, u64)>  // :2000  (position, qpc_position)
```

- Seconds = `position / frequency` (`:1996-1999`). Don't assume the position is in frames: convert with
  `pos * rate / freq`.
- The second value is the QPC time at which the position was read. **(MS)** It's in 100 ns units.
- Not available on process-loopback clients (§11).
- **Render latency** = `frames_written_total - pos * rate / freq` frames still queued.
- **Capture age of a packet** = `qpc_now_100ns - info.timestamp`.
- **QPC now in 100 ns** (`windows::Win32::System::Performance`, enabled in `nf-engine`):
  ```rust
  use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
  fn qpc_100ns(qpf: i64) -> u64 { let mut c = 0i64; unsafe { let _ = QueryPerformanceCounter(&mut c); }
      (c as i128 * 10_000_000 / qpf as i128) as u64 }   // qpf read once with QueryPerformanceFrequency at setup
  ```

---

## 11. Loopback

### Endpoint loopback (the speaker reference)
- Open the **render** device and initialise with `Direction::Capture` and a **shared** mode. The crate
  adds `AUDCLNT_STREAMFLAGS_LOOPBACK` (`src/api.rs:1249-1252`).
- Use `get_audiocaptureclient()` and `read_from_device` as for a mic (`tests/device/errors.rs:259-296`,
  which uses `PollingShared` at the mix format).
- **The crate's tests don't cover event-driven endpoint loopback.** Poll it from the mic loop (§18.3).
- `autoconvert: true` is allowed (the flags are ORed). Whether the engine honours AUTOCONVERTPCM on
  endpoint loopback isn't tested by the crate, so capture at the mix format and convert yourself.
- **Keep-alive:** a second client on the same render device, a `Direction::Render` shared stream at the
  default period, writing silence. It keeps loopback packets flowing (`doc/todo/8-audio-io.md`).

### Process loopback: `new_application_loopback_client` (`src/api.rs:757-887`)
```rust
pub fn new_application_loopback_client(process_id: u32, include_tree: bool) -> WasapiRes<Self>
```
- **Activation:**
  - `ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, IAudioClient, PROCESS_LOOPBACK params)`
    (`:854-859`);
  - **it blocks the calling thread on a Condvar with no timeout** (`:862-866`);
  - the completion handler is `#[implement]`, which is agile by default
    (`windows-implement-0.60.2/src/lib.rs:103-104, 165`).
- **`include_tree` is inverted relative to its doc:**
  - doc (`:759-761`): "if false only audio from the target process is captured";
  - code (`:817-821`): `true` gives `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`, `false` gives
    `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE`, i.e. **all audio except that process tree**;
  - so **exclude-self** is `new_application_loopback_client(std::process::id(), false)`;
  - there's no "target process only, without children" mode;
  - for `true`, pass the **parent** pid of the app (`examples/record_application.rs:107-110`).
- **Returned state:** `direction = Render`, `sharemode = Some(Shared)`, `timingmode = None`
  (`:879-885`). Then
  `initialize_client(&fmt, &Direction::Capture, &StreamMode::EventsShared{ autoconvert: true, buffer_duration_hns: 0 })`.
  "the period … is irrelevant" (`:768-769`; `tests/device/errors.rs:350-360`).
- **The format must be chosen by you,** e.g. `WaveFormat::new(32,32,&SampleType::Float,48_000,2,None)`,
  because `get_mixformat` fails.
- **Methods that don't work here** (`:771-784`, checked by `tests/device/errors.rs:317-348`):
  - `get_mixformat`, `is_supported`, `is_supported_exclusive_with_quirks`, `get_device_period`,
    `calculate_aligned_period_near` and `get_current_padding` fail;
  - `get_buffer_size` returns junk such as 3131961357;
  - `get_available_space_in_frames` fails;
  - `get_audiorenderclient`, `get_audiosessioncontrol` and `get_audioclock` give E_NOINTERFACE.
- **What does work:**
  - `get_audiocaptureclient`, `set_get_eventhandle` and `get_next_packet_size`
    (`examples/record_application.rs:49-81`);
  - `read_from_device` with a buffer sized from `get_next_packet_size` or a fixed cap.
- Whether `BufferInfo` timestamps are meaningful in this mode isn't covered by the source or the tests.
- The crate source doesn't say which endpoint's mix this captures. It activates the virtual
  process-loopback device, not an endpoint.
- **(MS)** Needs Windows 10 build 20348 or later.

---

## 12. Sessions, ducking, meters

```rust
impl AudioSessionControl {                                                         // src/api.rs:1811
    pub fn get_state(&self) -> WasapiRes<SessionState>                             // :1813
    pub fn register_session_notification(&self, callbacks: EventCallbacks) -> WasapiRes<EventRegistration> // :1832
    pub fn get_process_id(&self) -> WasapiRes<u32>                                 // :1848
    pub fn set_ducking_preference(&self, preference: bool) -> WasapiRes<()>        // :1855
    pub fn get_display_name(&self) -> WasapiRes<String>                            // :1866
    pub fn get_icon_path(&self) -> WasapiRes<String>                               // :1874
    pub fn get_session_identifier(&self) -> WasapiRes<String>                      // :1882
    pub fn get_session_instance_identifier(&self) -> WasapiRes<String>             // :1891
    pub fn get_audiometerinformation(&self) -> WasapiRes<AudioMeterInformation>    // :1899
}
pub struct EventRegistration { .. }   // :1969  Drop -> UnregisterAudioSessionNotification (:1974-1981)
```

- **`set_ducking_preference(true)` means opt OUT of auto-ducking.** It calls
  `IAudioSessionControl2::SetDuckingPreference(optout: bool)` (`:1855-1861`; `W/Media/Audio/mod.rs:3804`),
  and the crate's doc text doesn't say which way `true` goes.
  ```rust
  let session = cable_client.get_audiosessioncontrol()?;   // after initialize, before start; keep it alive
  session.set_ducking_preference(true)?;                   // our session is not ducked
  ```
- The session control needs an initialised client.
- **Session events** (`src/events.rs:50-151`): `EventCallbacks::new()` and the setters below. Each takes
  `impl Fn(..) + 'static + Sync + Send` and has an `unset_*` partner.

  | Setter | Callback signature |
  |---|---|
  | `set_simple_volume_callback` | `(f32, bool, GUID)` |
  | `set_channel_volume_callback` | `(usize, f32, GUID)` |
  | `set_state_callback` | `(SessionState)` |
  | `set_disconnected_callback` | `(DisconnectReason)` |
  | `set_iconpath_callback` | `(String, GUID)` |
  | `set_displayname_callback` | `(String, GUID)` |
  | `set_groupingparam_callback` | `(GUID, GUID)` |

  - `DisconnectReason { DeviceRemoval, ServerShutdown, FormatChanged, SessionLogoff, SessionDisconnected, ExclusiveModeOverride, Unknown }`
    (`src/events.rs:156`). Use `disconnected` to trigger a stream rebuild.
  - Keep the returned `EventRegistration` alive (`examples/playsine_events.rs:99-113`).
- **Meters:**
  - `AudioMeterInformation::get_peak_value(&self) -> WasapiRes<f32>` (`:1917`, 0.0..1.0, not affected by
    volume settings);
  - `get_metering_channel_count` (`:1922`);
  - `get_channels_peak_values(&self) -> WasapiRes<Vec<f32>>` allocates (`:1927-1933`);
  - `query_hardware_support` (`:1938`).

  These are system meters. The GUI meter should come from our own DSP.

---

## 13. Device notifications (`src/events.rs:315-504`; registration `src/api.rs:420-465`)

```rust
impl DeviceEventCallbacks {      // all take impl Fn(..) + 'static + Sync + Send; each has unset_*
    pub fn new() -> Self
    pub fn set_device_state_callback(&mut self, c: impl Fn(String, DeviceState) + 'static + Sync + Send)  // :344 (id, new state)
    pub fn set_device_added_callback(&mut self, c: impl Fn(String) + 'static + Sync + Send)               // :357
    pub fn set_device_removed_callback(&mut self, c: impl Fn(String) + 'static + Sync + Send)             // :367
    pub fn set_default_device_callback(&mut self, c: impl Fn(Direction, Role, Option<String>) + 'static + Sync + Send) // :379 None = no default any more
    pub fn set_property_value_callback(&mut self, c: impl Fn(String, PROPERTYKEY) + 'static + Sync + Send) // :392
}
pub struct DeviceEventRegistration { .. }   // src/api.rs:453  Drop -> UnregisterEndpointNotificationCallback
```

- **Callbacks run on a Windows audio thread.** "They should return quickly, and must not call back into
  the DeviceEnumerator that they were registered on" (`src/api.rs:430-432`).
  - Send a message to the control thread and do no COM in the callback.
  - Registration takes ownership of the callbacks (`:428`). Keep the registration alive (it's `!Send`)
    (`examples/device_notifications.rs:28-32`).
- **Values the crate skips** (with a `warn!`):
  - a null or unreadable id, an `eAll` flow, or an unknown role/state (`src/events.rs:419-427, 466-486`);
  - a null default id is passed on as `None` (`:464-472`).
- **(MS)** A default-device change can arrive once per role, so filter on `Role`.
- Inside callbacks, unknown values are skipped, never raised. `IllegalDeviceState`/`IllegalDeviceRole`
  only come from direct calls such as `Device::get_state` (`src/api.rs:326-338`).
  ```rust
  use std::sync::mpsc;
  enum DevMsg { Default(Direction, Role, Option<String>), State(String, DeviceState), Added(String), Removed(String) }
  let (tx, rx) = mpsc::channel::<DevMsg>();              // Sender: Send + Sync, so it fits Fn + Send + Sync
  let mut cb = DeviceEventCallbacks::new();
  let t = tx.clone(); cb.set_default_device_callback(move |d, r, id| { let _ = t.send(DevMsg::Default(d, r, id)); });
  let t = tx.clone(); cb.set_device_state_callback(move |id, s| { let _ = t.send(DevMsg::State(id, s)); });
  let t = tx.clone(); cb.set_device_added_callback(move |id| { let _ = t.send(DevMsg::Added(id)); });
  let t = tx;         cb.set_device_removed_callback(move |id| { let _ = t.send(DevMsg::Removed(id)); });
  let _reg = enumerator.register_notification_callback(cb)?;   // control thread; RegisterNotifications(err) on failure
  ```

---

## 14. Other wrappers (brief)

- **Windows' own AEC** (the voice-processing APO), not ours:
  - `is_aec_supported()` needs an initialised client (`tests/device/errors.rs:232-236`);
  - `get_aec_control()` gives E_NOINTERFACE without it;
  - `AcousticEchoCancellationControl::set_echo_cancellation_render_endpoint(&self, endpoint_id: Option<String>)`
    (`src/api.rs:2376`);
  - `IAudioEffectsManager` needs Windows 11 22000 or later (`:1469`).

  The example turns it on by setting the Communications category on a capture from the default
  Communications device (`examples/aec.rs:24-25, 39-40, 50-56`). We use RAW or Other plus sonora, so
  don't stack the two.
- **`CapabilityProbe::new(&Device)` and its `supported_formats*`** (`src/capabilities.rs:178-258`) probe
  exclusive formats. `DataRange` (`src/dataranges.rs:52`) holds driver-declared bounds. Neither is
  needed in shared mode.
- **`AudioSessionManager` and `AudioSessionEnumerator`** list sessions on a device (`src/api.rs:1684-1715`,
  `examples/processes.rs`).

---

## 15. Errors (`src/errors.rs`)

```rust
#[derive(Debug, thiserror::Error)]
pub enum WasapiError {
    DeviceNotFound(String),            // get_device_with_name (src/api.rs:500)
    IllegalDeviceState(u32), IllegalDeviceRole(i32), IllegalDeviceDirection(i32), IllegalSessionState(i32),
    UnsupportedFormat,                 // WaveFormat parsing, is_supported_exclusive_with_quirks
    UnsupportedSubformat(windows_core::GUID),   // WaveFormat::get_subformat
    ClientNotInit,                     // get_available_space_in_frames only (src/api.rs:1354)
    RegisterNotifications(windows_core::Error), // both register_* calls
    DataLengthMismatch { received: usize, expected: usize },  // write_to_device (bytes)
    DataLengthTooShort { received: usize, expected: usize },  // read_from_device (frames), write_to_device_from_deque (bytes)
    EventTimeout,                      // Handle::wait_for_event, any non-WAIT_OBJECT_0
    AutomaticFormatConversionInExclusiveMode,   // never constructed
    LoopbackWithExclusiveMode, RenderToCaptureDevice,         // initialize_client guards
    Windows(#[from] windows_core::Error),       // every failed COM call
    FromUtf16Error(#[from] std::string::FromUtf16Error),
}
```

- **Match on an HRESULT:**
  `matches!(e, WasapiError::Windows(w) if w.code() == AUDCLNT_E_DEVICE_INVALIDATED)`
  (`tests/device/support.rs:203-205`).
- **Codes** (`windows::Win32::Media::Audio`, `W/Media/Audio/mod.rs:1448-1541`):

  | Code | Value |
  |---|---|
  | `AUDCLNT_E_NOT_INITIALIZED` | 0x88890001 |
  | `AUDCLNT_E_ALREADY_INITIALIZED` | 0x88890002 |
  | `AUDCLNT_E_WRONG_ENDPOINT_TYPE` | 0x88890003 |
  | `AUDCLNT_E_DEVICE_INVALIDATED` | 0x88890004 |
  | `AUDCLNT_E_BUFFER_TOO_LARGE` | 0x88890006 |
  | `AUDCLNT_E_OUT_OF_ORDER` | 0x88890007 |
  | `AUDCLNT_E_UNSUPPORTED_FORMAT` | 0x88890008 |
  | `AUDCLNT_E_DEVICE_IN_USE` | 0x8889000A |
  | `AUDCLNT_E_SERVICE_NOT_RUNNING` | 0x88890010 |
  | `AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED` | 0x88890011 |
  | `AUDCLNT_E_EVENTHANDLE_NOT_SET` | 0x88890014 |
  | `AUDCLNT_E_BUFFER_SIZE_ERROR` | 0x88890016 |
  | `AUDCLNT_E_BUFFER_ERROR` | 0x88890018 |
  | `AUDCLNT_E_INVALID_DEVICE_PERIOD` | 0x88890020 |
  | `AUDCLNT_E_INVALID_STREAM_FLAG` | 0x88890021 |
  | `AUDCLNT_E_RESOURCES_INVALIDATED` | 0x88890026 |
  | `AUDCLNT_E_RAW_MODE_UNSUPPORTED` | 0x88890027 |
  | `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED` | 0x88890028 |
  | `AUDCLNT_E_ENGINE_FORMAT_LOCKED` | 0x88890029 |
  | `AUDCLNT_S_BUFFER_EMPTY` (success) | 0x08890001 |

  Also `AUDCLNT_STREAMFLAGS_RATEADJUST = 0x100000`.
- **(MS)** `DEVICE_INVALIDATED` and `RESOURCES_INVALIDATED` mean you should tear down and rebuild the
  stream from the device id.
- **Cost of an error:**
  - `From<HRESULT> for windows_core::Error` calls `GetErrorInfo` (`windows-result-0.4.1/src/error.rs:160-164, 274-281`);
  - `Display` formats a system message, which allocates;
  - on the audio thread, keep the `HRESULT` (`w.code().0`) in an atomic or a ring slot, and format it on
    the control thread.
  - `WasapiError` is `Send`, so it can also go over a channel.

---

## 16. Raw access and what isn't wrapped

- `AudioClient::as_iaudioclient(&self) -> &IAudioClient` (`src/api.rs:967`).
  - **"Never call `Initialize` on this interface."** Doing so leaves the frame size unset, "which makes
    `read_from_device` panic and `write_to_device` play noise without reporting an error" (`:961-966`).
  - Casting is fine: `client.as_iaudioclient().cast::<IAudioClient3>()` (`tests/device/raw.rs:88-129`),
    and so is `GetService`.
- `Device::as_immdevice(&self) -> &IMMDevice` (`:719`) gives `Activate` for `IAudioEndpointVolume` etc.
  (`tests/device/raw.rs:17-39`) and `OpenPropertyStore`.
- `Handle::as_handle` (§9). `AudioClient::from_iaudioclient(IAudioClient, Direction)` wraps a client
  activated elsewhere. The direction is the endpoint's (`:889-948`).
- **RATEADJUST / `IAudioClockAdjustment`:**
  - the flag has to be passed to `Initialize`, which `initialize_client` can't do;
  - raw `Initialize` breaks the wrapper (above);
  - so a RATEADJUST stream has to be **fully raw**: windows `IAudioClient::Initialize`,
    `GetService::<IAudioRenderClient>()` and `GetService::<IAudioClockAdjustment>()`, with no wasapi
    capture or render wrappers.
  - `doc/research-2026-10.md` §Windows audio I/O ("use … `AudioClient::as_iaudioclient`") is only right
    in that sense. Our drift plan uses slip or a resampler, so this is optional.
- **MMCSS isn't in the crate.** Call `AvSetMmThreadCharacteristicsW("Pro Audio")` on the audio thread
  (windows `Win32_System_Threading`).

---

## 17. Hot-path cost table (crate code only; what WASAPI does internally isn't visible here)

| Call | Heap allocation in the crate | Other cost |
|---|---|---|
| `AudioCaptureClient::read_from_device` | none | GetBuffer + memcpy + ReleaseBuffer; `trace!` (`:2241`) |
| `AudioCaptureClient::get_next_packet_size` | none | 1 COM call |
| `AudioRenderClient::write_to_device` | none | GetBuffer + memcpy + ReleaseBuffer; `trace!` (`:2068`) |
| `get_current_padding`, `get_buffer_size` | none | 1 COM call + `trace!` |
| `get_available_space_in_frames` | none | 1-2 COM calls |
| `AudioClock::get_position` | none | 1 COM call |
| `Handle::wait_for_event` | none | blocks (that's the point) |
| `read_from_device_to_deque` | **yes** (`extend`, `:2272`) | **panics** on ReleaseBuffer error (`:2274`) |
| any failed call (`Err(Windows)`) | windows Error + `GetErrorInfo`; `Display` allocates | rare; keep it off the hot path |
| `get_channels_peak_values`, `get_audio_effects`, `make_channelmasks`, all `String` getters, `get_mixformat`, `is_supported` | **yes** | setup or control thread only |

The `log` macros cost one relaxed atomic load when disabled (`log-0.4.x/src/macros.rs:35`). Never run
with the max level at Trace in a build used for audio. The `log` feature `release_max_level_debug`
compiles them out entirely.

---

## 18. Code skeletons

Shared helpers:
```rust
use wasapi::*;
fn f32_bytes(s: &[f32]) -> &[u8] {            // f32 has no padding; u8 has alignment 1
    unsafe { std::slice::from_raw_parts(s.as_ptr().cast::<u8>(), std::mem::size_of_val(s)) }
}
#[derive(Clone, Copy)] enum Pcm { F32, I16, I24, I32 }
fn pcm_of(f: &WaveFormat) -> Result<Pcm, WasapiError> {
    Ok(match (f.get_subformat()?, f.get_bitspersample()) {
        (SampleType::Float, 32) => Pcm::F32,
        (SampleType::Int, 16) => Pcm::I16,
        (SampleType::Int, 24) => Pcm::I24,            // packed 3 bytes
        (SampleType::Int, 32) => Pcm::I32,            // 32-bit, or 24-in-32 (valid bits in the MSBs, MS)
        _ => return Err(WasapiError::UnsupportedFormat),
    })
}
/// Interleaved device bytes -> mono f32 (channel average). No allocation.
fn to_mono(pcm: Pcm, ch: usize, src: &[u8], dst: &mut [f32]) {
    let bps = match pcm { Pcm::I16 => 2, Pcm::I24 => 3, Pcm::F32 | Pcm::I32 => 4 };
    let k = 1.0 / ch as f32;
    for (o, frame) in dst.iter_mut().zip(src.chunks_exact(bps * ch)) {
        let mut acc = 0.0f32;
        for s in frame.chunks_exact(bps) {
            acc += match pcm {
                Pcm::F32 => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                Pcm::I16 => i16::from_le_bytes([s[0], s[1]]) as f32 * (1.0 / 32_768.0),
                Pcm::I24 => i32::from_le_bytes([0, s[0], s[1], s[2]]) as f32 * (1.0 / 2_147_483_648.0),
                Pcm::I32 => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 * (1.0 / 2_147_483_648.0),
            };
        }
        *o = acc * k;
    }
}
```

### 18.1 Event-driven shared capture at the mix format, to mono f32 48 kHz
```rust
struct Mic { client: AudioClient, cap: AudioCaptureClient, event: Handle,
             pcm: Pcm, ch: usize, block_align: usize, raw: Vec<u8>, mono: Vec<f32>, next_index: Option<u64> }

fn open_mic(en: &DeviceEnumerator, id: &str) -> Result<Mic, WasapiError> {
    let device = en.get_device(id)?;
    let mut client = device.get_iaudioclient()?;
    if raw_supported(&device) {                                   // §4
        client.set_properties(AudioClientProperties::new().set_option(StreamOption::Raw))?;
    }
    let mix = client.get_mixformat()?;
    // At 48 kHz take the mix format as is and convert ourselves. Otherwise let the engine
    // resample and downmix (AUTOCONVERTPCM | SRC_DEFAULT_QUALITY), or reopen at the mix rate and use our resampler.
    let (fmt, autoconvert) = if mix.get_samplespersec() == 48_000 { (mix, false) } else {
        (WaveFormat::new(32, 32, &SampleType::Float, 48_000, 1, Some(SPEAKER_FRONT_CENTER)), true)
    };
    let (def_hns, _min_hns) = client.get_device_period()?;
    client.initialize_client(&fmt, &Direction::Capture,
        &StreamMode::EventsShared { autoconvert, buffer_duration_hns: def_hns })?;
    let event = client.set_get_eventhandle()?;                    // after init, before start
    let cap = client.get_audiocaptureclient()?;                   // after init (snapshots bytes_per_frame)
    let frames = client.get_buffer_size()? as usize;
    let block_align = fmt.get_blockalign() as usize;
    Ok(Mic { pcm: pcm_of(&fmt)?, ch: fmt.get_nchannels() as usize, block_align,
             raw: vec![0u8; 2 * frames * block_align], mono: vec![0.0; 2 * frames],   // allocated once
             next_index: None, client, cap, event })
}

impl Mic {
    /// Call when `event` fires. Drains every ready packet. No allocation.
    fn drain(&mut self, mut sink: impl FnMut(&[f32], u64 /*qpc_100ns*/, u64 /*gap_frames*/)) -> Result<(), WasapiError> {
        loop {
            let (frames, info) = self.cap.read_from_device(&mut self.raw)?;
            if frames == 0 { return Ok(()); }
            let n = frames as usize;
            let gap = self.next_index.map_or(0, |e| info.index.saturating_sub(e)); // don't trust data_discontinuity
            self.next_index = Some(info.index + frames as u64);
            let mono = &mut self.mono[..n];
            if info.flags.silent { mono.fill(0.0) }                // the crate copies junk for silent packets
            else { to_mono(self.pcm, self.ch, &self.raw[..n * self.block_align], mono) }
            sink(mono, if info.flags.timestamp_error { 0 } else { info.timestamp }, gap);
        }
    }
}
// Thread body: let _com = Com::mta()?; (MMCSS) ; let en = DeviceEnumerator::new()?; let mut mic = open_mic(&en, &id)?;
// mic.client.start_stream()?; then loop on WaitForMultipleObjects([mic.event.as_handle(), quit]) (§9),
// calling mic.drain(..) and feeding the 480-frame block assembler; finally mic.client.stop_stream()?.
```

### 18.2 Render to a device (the cable), serviced from the mic loop (polling, no second event)
```rust
struct Out { client: AudioClient, render: AudioRenderClient, _session: AudioSessionControl, buffer_frames: u32 }

fn open_cable(en: &DeviceEnumerator, id: &str) -> Result<Out, WasapiError> {
    let device = en.get_device(id)?;
    let mut client = device.get_iaudioclient()?;
    let fmt = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 1, Some(SPEAKER_FRONT_CENTER));
    let (def_hns, _) = client.get_device_period()?;
    // autoconvert lets the engine map mono 48 k to the cable's mix format. If the mix is 48 kHz f32 stereo,
    // use it with autoconvert: false and duplicate the channel yourself.
    client.initialize_client(&fmt, &Direction::Render,
        &StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: 3 * def_hns })?; // capacity, not latency
    let session = client.get_audiosessioncontrol()?;
    session.set_ducking_preference(true)?;                        // true = opt OUT of ducking
    let render = client.get_audiorenderclient()?;
    let buffer_frames = client.get_buffer_size()?;
    Ok(Out { client, render, _session: session, buffer_frames })
}

/// Queue up to `block`, keeping at most `target` frames buffered. Returns the frames written. No allocation.
fn push(out: &Out, block: &[f32], target: u32) -> Result<usize, WasapiError> {
    let queued = out.client.get_current_padding()?;
    let room = target.saturating_sub(queued).min(out.buffer_frames - queued) as usize;
    let n = room.min(block.len());
    out.render.write_to_device(n, f32_bytes(&block[..n]), None)?;   // n == 0 is a no-op
    Ok(n)
}
// Before start_stream: write `target` frames from a preallocated zero slice with
// Some(BufferFlags { silent: true, data_discontinuity: false, timestamp_error: false }).
// Event-driven variant: StreamMode::EventsShared + set_get_eventhandle(), wait, then write.
```

### 18.3 Loopback capture of a render device (the reference), plus keep-alive
```rust
struct Reference { lb: AudioClient, cap: AudioCaptureClient, fmt: WaveFormat, raw: Vec<u8>,
                   ka: AudioClient, ka_render: AudioRenderClient, ka_align: usize, zeros: Vec<u8> }

fn open_reference(en: &DeviceEnumerator, speaker_id: &str) -> Result<Reference, WasapiError> {
    let spk = en.get_device(speaker_id)?;                         // Direction::Render endpoint
    let mut lb = spk.get_iaudioclient()?;
    let fmt = lb.get_mixformat()?;
    let (def_hns, _) = lb.get_device_period()?;
    // Render device + Direction::Capture + shared => AUDCLNT_STREAMFLAGS_LOOPBACK. No set_properties here.
    lb.initialize_client(&fmt, &Direction::Capture,
        &StreamMode::PollingShared { autoconvert: false, buffer_duration_hns: 4 * def_hns })?;
    let cap = lb.get_audiocaptureclient()?;
    let raw = vec![0u8; 2 * lb.get_buffer_size()? as usize * fmt.get_blockalign() as usize];

    let mut ka = spk.get_iaudioclient()?;                         // keep-alive: silent render at the default period
    let ka_fmt = ka.get_mixformat()?;
    ka.initialize_client(&ka_fmt, &Direction::Render,
        &StreamMode::PollingShared { autoconvert: false, buffer_duration_hns: 4 * def_hns })?;
    let ka_render = ka.get_audiorenderclient()?;
    let ka_align = ka_fmt.get_blockalign() as usize;
    let zeros = vec![0u8; ka.get_buffer_size()? as usize * ka_align];
    Ok(Reference { lb, cap, fmt, raw, ka, ka_render, ka_align, zeros })
}

impl Reference {
    fn start(&self) -> Result<(), WasapiError> { self.service_keepalive()?; self.ka.start_stream()?; self.lb.start_stream() }
    /// Called on every mic event: top up the keep-alive, then drain the loopback packets.
    fn service_keepalive(&self) -> Result<(), WasapiError> {
        let n = self.ka.get_available_space_in_frames()? as usize;
        self.ka_render.write_to_device(n, &self.zeros[..n * self.ka_align],
            Some(BufferFlags { silent: true, data_discontinuity: false, timestamp_error: false }))
    }
    fn drain(&mut self, mut sink: impl FnMut(&[u8], &BufferInfo)) -> Result<(), WasapiError> {
        let ba = self.fmt.get_blockalign() as usize;
        loop {
            let (frames, info) = self.cap.read_from_device(&mut self.raw)?;
            if frames == 0 { return Ok(()); }                        // no packets during silence: zero-fill on the mic clock
            sink(&self.raw[..frames as usize * ba], &info);          // honour info.flags.silent; convert with to_mono
        }
    }
}
// Exclude-self process loopback instead (all system audio except this process):
// let mut c = AudioClient::new_application_loopback_client(std::process::id(), false)?;  // blocks, so not on the UI thread
// c.initialize_client(&WaveFormat::new(32,32,&SampleType::Float,48_000,2,None), &Direction::Capture,
//                     &StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 })?;
// Size reads from get_next_packet_size(), not get_buffer_size() (junk). No clock, padding or session.
```

### 18.4 Low-latency IAudioClient3 initialisation with fallback
```rust
struct Opened { client: AudioClient, format: WaveFormat, period_frames: u32, low_latency: bool }

/// Smallest engine period that divides `hop` (e.g. 480 for RNNoise), is a multiple of `fundamental`
/// and lies within [min, max].
fn pick_period(p: &SharedModeEnginePeriod, hop: u32) -> Option<u32> {
    if p.fundamental == 0 { return None; }
    let mut f = p.min.div_ceil(p.fundamental) * p.fundamental;
    while f <= p.max { if hop % f == 0 { return Some(f); } f += p.fundamental; }
    None
}

fn open_low_latency(device: &Device, hop: u32) -> Result<Opened, WasapiError> {
    let direction = device.get_direction();                       // Capture (mic) or Render (cable); not loopback
    let mut client = device.get_iaudioclient()?;
    let mix = client.get_mixformat()?;                            // IAudioClient3 has no autoconvert: use the engine format
    if let Ok(p) = client.get_shared_mode_engine_period(&mix) {   // works before initialise
        if let Some(period) = pick_period(&p, hop).filter(|&f| f < p.default) {
            match client.initialize_shared_audio_stream(&mix, TimingMode::Events, period) {
                Ok(()) => {
                    // Another client may hold the engine at a different period (MS): read back what applies.
                    let actual = client.get_current_shared_mode_engine_period().map(|(_, f)| f).unwrap_or(period);
                    return Ok(Opened { client, format: mix, period_frames: actual, low_latency: true });
                }
                // e.g. AUDCLNT_E_ENGINE_PERIODICITY_LOCKED. A failed init leaves the client unusable, so start over.
                Err(_) => client = device.get_iaudioclient()?,
            }
        }
    }
    let (def_hns, _) = client.get_device_period()?;
    client.initialize_client(&mix, &direction,
        &StreamMode::EventsShared { autoconvert: false, buffer_duration_hns: def_hns })?;
    let rate = mix.get_samplespersec() as i64;
    let period_frames = ((def_hns * rate + 5_000_000) / 10_000_000) as u32;    // 100_000 hns @ 48 k = 480
    Ok(Opened { client, format: mix, period_frames, low_latency: false })
}
// Then, on either path: set_get_eventhandle(), get_audiocaptureclient()/get_audiorenderclient(), start_stream().
// Call set_properties (RAW) on each fresh client before its initialise.
```

### 18.5 Device list for the GUI (control thread, MTA)
```rust
pub struct DevInfo { pub id: String, pub name: String, pub adapter: String, pub state: DeviceState }
fn list(en: &DeviceEnumerator, dir: Direction) -> Result<Vec<DevInfo>, WasapiError> {
    let coll = en.get_device_collection(&dir)?;                   // active devices only
    let n = coll.get_nbr_devices()?;                              // avoid the panicking iterator
    let mut v = Vec::with_capacity(n as usize);
    for i in 0..n {
        let d = coll.get_device_at_index(i)?;
        v.push(DevInfo { id: d.get_id()?, name: d.get_friendlyname()?,
                         adapter: d.get_interface_friendlyname().unwrap_or_default(), state: d.get_state()? });
    }
    Ok(v)                                                         // plain Strings: safe to send to the GUI
}
// Defaults: en.get_default_device_for_role(&Direction::Capture, &Role::Console)?.get_id()?  (and Role::Communications)
```

---

## 19. Docs vs code, collected

| Where | Doc says | Code does |
|---|---|---|
| `new_application_loopback_client` `include_tree` (`src/api.rs:759-761` vs `:817-821`) | false = target only | false = EXCLUDE target tree (everything else) |
| `set_ducking_preference(preference)` (`:1854-1861`) | "Sets the default … auto-ducking" | `true` = opt-out (`optout`, `W/Media/Audio/mod.rs:3804`) |
| `DataLengthTooShort` | "got X, expected at least Y" | frames from `read_from_device`, bytes from `write_to_device_from_deque` |
| `examples/loopback.rs` | named "loopback" | mic to speakers pass-through, not loopback capture |
| `examples/playnoise_exclusive.rs:90-99` | realign recovery | retries with the old, unaligned `mode` |
| `examples/record.rs`, `loopback.rs`, `aec.rs` | – | spawned threads skip `initialize_mta` (implicit MTA) |
| `WasapiError::AutomaticFormatConversionInExclusiveMode` | an error variant | never constructed |
| `doc/research-2026-10.md` (RATEADJUST via `as_iaudioclient`) | – | `as_iaudioclient` must not `Initialize`, so RATEADJUST needs a fully raw stream |
