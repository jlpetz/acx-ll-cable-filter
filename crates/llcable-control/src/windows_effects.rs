//! On-demand discovery of the effects Windows reports on a recording endpoint. No audio stream or effect-change subscription.
use std::io;

use windows::Media::AudioProcessing;
use windows::Media::Capture::MediaCategory;
use windows::Media::Effects::{AudioCaptureEffectsManager, AudioEffectType, AudioEffectsManager, IAudioEffectsManagerStatics};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW, CM_MapCrToWin32Err, CONFIGRET,
    CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows::Win32::Foundation::ERROR_GEN_FAILURE;
use windows::Win32::Media::Audio::DEVINTERFACE_AUDIO_CAPTURE;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoGetActivationFactory, RoInitialize, RoUninitialize};
use windows::core::{HSTRING, Interface, RuntimeName, Type};

/// Effect types in the order reported for each recording category, using Windows' default audio processing.
/// An empty list means Windows reported no effects. A category that couldn't be queried retains its error.
#[derive(Debug)]
pub struct CaptureEffects {
    pub ordinary: io::Result<Vec<String>>,
    pub communications: io::Result<Vec<String>>,
}

/// Query a current capture [`crate::Port::id`] on a background MTA or uninitialized worker thread.
/// The report describes the endpoint pipeline for the requested categories. It excludes processing inside other apps.
pub fn capture_effects(endpoint_id: &str) -> io::Result<CaptureEffects> {
    let interface = capture_interface(endpoint_id)?;
    // SAFETY: initializes the calling worker. Each success, including S_FALSE, is balanced before returning.
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(windows_error)?;
    let result = query_categories(&interface);
    // SAFETY: query_categories has released all WinRT references, including the activation factory and error information.
    unsafe { RoUninitialize() };
    result
}

fn query_categories(interface: &HSTRING) -> io::Result<CaptureEffects> {
    // The last worker can tear down the MTA between clicks, or between ports on the same click. The generated static
    // CreateAudioCaptureEffectsManagerWithMode caches an agile factory across those lifetimes. Own it for this query instead.
    // SAFETY: RoInitialize succeeded on this thread; this is the SDK runtime class and its matching statics interface.
    let factory: IAudioEffectsManagerStatics =
        unsafe { RoGetActivationFactory(&HSTRING::from(AudioEffectsManager::NAME)) }.map_err(windows_error)?;
    Ok(CaptureEffects {
        ordinary: effects_for(&factory, interface, MediaCategory::Other).map_err(windows_error),
        communications: effects_for(&factory, interface, MediaCategory::Communications).map_err(windows_error),
    })
}

fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}

fn capture_interface(endpoint_id: &str) -> io::Result<HSTRING> {
    if endpoint_id.is_empty() || endpoint_id.contains('\0') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Missing or invalid recording endpoint ID"));
    }
    // MMDevice endpoints are children of the SWD MMDEVAPI enumerator. Keep the MMDevice ID opaque and ask PnP
    // for the actual present recording interface, including its reference string. A stale or playback ID cannot match.
    let node = HSTRING::from(format!(r"SWD\MMDEVAPI\{endpoint_id}"));
    let flags = CM_GET_DEVICE_INTERFACE_LIST_PRESENT;
    for _ in 0..8 {
        let mut len = 0;
        // SAFETY: node is a NUL-terminated instance ID; len is writable for the duration of the call.
        let status = unsafe { CM_Get_Device_Interface_List_SizeW(&mut len, &DEVINTERFACE_AUDIO_CAPTURE, &node, flags) };
        if status != CR_SUCCESS {
            return Err(config_error(status));
        }
        let mut data = vec![0; len.max(1) as usize];
        // SAFETY: data supplies the requested WCHAR count; node and the class GUID outlive the call.
        let status = unsafe { CM_Get_Device_Interface_ListW(&DEVINTERFACE_AUDIO_CAPTURE, &node, &mut data, flags) };
        if status == CR_BUFFER_SMALL {
            continue;
        }
        if status != CR_SUCCESS {
            return Err(config_error(status));
        }
        return single_capture_interface(&data);
    }
    Err(config_error(CR_BUFFER_SMALL))
}

fn single_capture_interface(data: &[u16]) -> io::Result<HSTRING> {
    let mut paths = data.split(|&c| c == 0).take_while(|s| !s.is_empty());
    let first = paths.next().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Recording endpoint is no longer active"))?;
    if paths.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Windows returned multiple recording interfaces for one endpoint"));
    }
    Ok(HSTRING::from_wide(first))
}

fn config_error(cr: CONFIGRET) -> io::Error {
    // SAFETY: a pure mapping of a configuration manager error to a Win32 error.
    io::Error::from_raw_os_error(unsafe { CM_MapCrToWin32Err(cr, ERROR_GEN_FAILURE.0) } as i32)
}

fn effects_for(factory: &IAudioEffectsManagerStatics, interface: &HSTRING, category: MediaCategory) -> windows::core::Result<Vec<String>> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: the typed factory and borrowed HSTRING outlive the ABI call. On success Windows returns an owned reference
    // to the manager's default interface. from_abi checks for null and takes ownership, as in the SDK's generated projection.
    let manager: AudioCaptureEffectsManager = unsafe {
        (factory.vtable().CreateAudioCaptureEffectsManagerWithMode)(
            factory.as_raw(),
            std::mem::transmute_copy(interface),
            category,
            AudioProcessing::Default,
            &mut raw,
        )
        .ok()?;
        Type::from_abi(raw)?
    };
    let effects = manager.GetAudioCaptureEffects()?;
    (0..effects.Size()?).map(|index| Ok(effect_name(effects.GetAt(index)?.AudioEffectType()?))).collect()
}

fn effect_name(effect: AudioEffectType) -> String {
    match effect {
        AudioEffectType::Other => "Other effect",
        AudioEffectType::AcousticEchoCancellation => "Acoustic echo cancellation",
        AudioEffectType::NoiseSuppression => "Noise suppression",
        AudioEffectType::AutomaticGainControl => "Automatic gain control",
        AudioEffectType::BeamForming => "Beamforming",
        AudioEffectType::ConstantToneRemoval => "Constant tone removal",
        AudioEffectType::Equalizer => "Equalizer",
        AudioEffectType::LoudnessEqualizer => "Loudness equalization",
        AudioEffectType::BassBoost => "Bass boost",
        AudioEffectType::VirtualSurround => "Virtual surround",
        AudioEffectType::VirtualHeadphones => "Virtual headphones",
        AudioEffectType::SpeakerFill => "Speaker fill",
        AudioEffectType::RoomCorrection => "Room correction",
        AudioEffectType::BassManagement => "Bass management",
        AudioEffectType::EnvironmentalEffects => "Environmental effects",
        AudioEffectType::SpeakerProtection => "Speaker protection",
        AudioEffectType::SpeakerCompensation => "Speaker compensation",
        AudioEffectType::DynamicRangeCompression => "Dynamic range compression",
        AudioEffectType::FarFieldBeamForming => "Far-field beamforming",
        AudioEffectType::DeepNoiseSuppression => "Deep noise suppression",
        other => return format!("Unknown effect ({})", other.0),
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_or_ambiguous_endpoints_have_no_substitute() {
        let interfaces = |s: &str| single_capture_interface(&s.encode_utf16().collect::<Vec<_>>());
        assert_eq!(interfaces("\0").unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(interfaces("one\0two\0\0").unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(interfaces("two\0\0").unwrap().to_string(), "two");
    }
}
