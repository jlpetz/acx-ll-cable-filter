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

## A second finding we could add (our proposal, not yet the owner's)
ACX capture timestamps (TODO 19, 2026-11-23): ACX never calls `EvtAcxStreamGetCapturePacket` (0 calls, shared and
exclusive, every mode and period tried) and hands clients the QPC passed to `AcxRtStreamNotifyPacketComplete` as the
packet's first frame's time (GetBuffer's `pu64QPCPosition`, to the tick). Its documentation calls that QPC "the packet
completion time", and the AudioCodec sample passes the completion time, so a driver that follows both stamps every capture
packet one packet late. We now pass the packet's start. Question: which is intended, and should the docs and the sample say so?

### A first packet with a different timestamp after Stop/Start

On the dev box (Server 2025, build 26100, 2026-11-23), open the low-latency microphone in exclusive mode at 48 kHz
stereo float32, 480 frames per packet. Read and release a packet on its event, wait 3–8 ms **before Stop**, then
restart the same client after a pause, without Reset. Record the first two nonempty `GetBuffer` results, their flags,
frame positions and timestamps. Varying how long the stream is stopped tests a different part of the timing.

The original probe saw a backward step on 8/8 restarts. A follow-up also sampled the driver's last notification
and timer-pass count. On 13/16 restarts the first two packets matched successive notifications 10.000 ms apart.
On the other three (two DEFAULT, one RAW), the first 480-frame packet arrived within 0.54 ms of Start, while the
driver still showed its pre-Start notification and pass count. The next packet matched the new notification,
with its timestamp 0.82–9.89 ms **earlier** than the first. Frame positions advanced by 480 and Windows reported
zero flags. In each of these three cases one notification had occurred between the last client read and Stop completing.

As a comparison, VB-CABLE accepted exclusive DEFAULT at the same rate and period in PCM16. All eight timestamp
steps were 10.000 ms and the first packet arrived 11.74–12.32 ms after Start. VB-CABLE refused float32.
The ACX driver supplies the first-frame time to its completion notification as described above. When an unread
packet spans Stop/Start, which component supplies its first-frame timestamp? How should an ACX driver ensure
that the timestamps a client sees on consecutive packets keep advancing? Physical Windows 11 confirmation is pending.

(Ours, the same evening, for the owner to fold in or leave: `nf-vmic-test stamps` now waits 30-170% of a packet before
each Stop, so a packet is left unread on purpose. Exclusive, 480 and 128 frames, 16 restarts each: every restart that left
one (4 of 16 in each run) handed it out about 0.3 ms after Start stamped about then, and none of the others did. In 7 of
those 8 the next packet, the driver's, was stamped 1.0-9.1 ms earlier; in the eighth the client's next read came five
packet positions on, 11.05 ms later (corrected by Astra's review, 2026-11-24; Astra's own rerun under Verifier, 8 of 8 with
the step back). In shared mode the engine does the same with 3-10 restarts in 16, and at a 5 ms period
it also hands out, after Start, packets completed before Stop with the driver's own stamps.)
[WASAPI capture packet timestamps](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiocaptureclient-getbuffer)
and [ACX packet completion timestamps](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/acxstreams/nf-acxstreams-acxrtstreamnotifypacketcomplete).

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
