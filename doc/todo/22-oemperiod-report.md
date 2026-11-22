# TODO 22: report to Microsoft: an endpoint's default engine period can't change without re-creating the device

The owner (2026-11-23): "Having to re-register devices in this way seems strange, I would assume [it] wasn't intended." Report
it, with a reproduction. Nothing has been posted; posting is the owner's.

## What we found (TODO 19's spike notes, 2026-11-22/23; Windows Server 2025, build 26100; our ACX driver)
- **The default period follows the INF.** `PKEY_AudioEngine_OEMPeriod` (`{E4870E26-3CC5-4CD2-BA46-CA0A9A70ED04},6`, a VT_BLOB of
  an 8-byte HNS period, 5-9 ms per Learn), written by the INF under the capture interface's `EP\0`, sets the endpoint's
  default device period. With 5 ms, a client that calls plain `IAudioClient::Initialize` (call apps: Discord, Teams) ran
  240-frame packets instead of 480: ring-to-recorder latency p50 9.8 → 4.6 ms.
- **It takes effect only when the endpoint is created at install.** Each of these left the period at 10 ms:
  - writing the value into the endpoint's MMDevices property store (Administrators hold SetValue there), then restarting
    Audiosrv;
  - writing it into the interface key (`...\#Microphone0\Device Parameters\EP\0`, where the INF puts it), then disabling and
    enabling the device;
  - the same write, then restarting AudioEndpointBuilder.

  Only uninstalling and reinstalling the driver applied it.
- **With it set, IAudioClient3's small periods disappear.** `GetSharedModeEnginePeriod` went from 128-480 frames (from the
  driver's `DEVPKEY_KsAudio_PacketSize_Constraints2`) to 240 only, and the exclusive minimum from 2.67 to 3 ms. So one
  endpoint can't offer both a smaller default for plain clients and the IAudioClient3 range for clients that ask.
- **Creating or re-creating a device needs Administrator access**, as `SwDeviceCreate`'s documentation states, and so does
  changing a device's driver. A per-user or per-app choice therefore needs a privileged helper, or two endpoints.

## Questions for Microsoft
1. Is there a supported way to change an endpoint's default engine period at runtime (a property, an API, a notification
   that makes the engine re-read it)?
2. Is it intended that `PKEY_AudioEngine_OEMPeriod` replaces the IAudioClient3 range rather than only the default?
3. Could the shared engine give plain-Initialize clients a smaller default (the low-latency stack's own) while keeping the
   IAudioClient3 range, for virtual and hardware devices alike?

## Where to report (to choose)
- Feedback Hub, Developer Platform → Audio (reaches the audio team; needs a Microsoft account).
- An issue on MicrosoftDocs/windows-driver-docs, for the documentation side: the OEMPeriod page dates from Windows 7 and says
  nothing about IAudioClient3 or when the value is read.
- microsoft/low-latency-audio discussions, the team behind the ACX low-latency work.

## Draft (for the owner to post; check the figures again first)
Title: "PKEY_AudioEngine_OEMPeriod is only read when an endpoint is created, and it removes the IAudioClient3 period range"

> We develop a virtual microphone (an ACX driver based on the AudioCodec sample) that call apps record from. Those apps open
> the mic with plain `IAudioClient::Initialize` and run at the engine's default 10 ms period. Setting
> `PKEY_AudioEngine_OEMPeriod` to 5 ms in the INF (under the capture interface's `EP\0`) halves their latency, but:
>
> 1. The value is read only when the endpoint is created. Writing it to the endpoint's property store or the interface key
>    and restarting Audiosrv, AudioEndpointBuilder or the device has no effect; only reinstalling the driver applies it.
> 2. With it set, `IAudioClient3::GetSharedModeEnginePeriod` reports only the OEM period (240 frames), not the 128-480
>    range our `DEVPKEY_KsAudio_PacketSize_Constraints2` gives. So we can't offer call apps a smaller default and
>    low-latency apps a smaller period on one endpoint.
>
> Is there a supported way to change the default period at runtime, and is (2) intended? Repro: Windows 11 24H2 / Server
> 2025 (26100), an ACX capture endpoint with packet-size constraints; set the OEM period in the INF, install, compare
> `GetDevicePeriod` and `GetSharedModeEnginePeriod`, then change the value without reinstalling.
