/*++

Copyright (c) Microsoft Corporation.  All rights reserved.

    THIS CODE AND INFORMATION IS PROVIDED "AS IS" WITHOUT WARRANTY OF ANY
    KIND, EITHER EXPRESSED OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE
    IMPLIED WARRANTIES OF MERCHANTABILITY AND/OR FITNESS FOR A PARTICULAR
    PURPOSE.

Module Name:

    Public.h

Abstract:

    Contains structure definitions and function prototypes for the driver
    that are public. 

Environment:

    Kernel mode

--*/

/* make prototypes usable from C++ */
#ifdef __cplusplus
extern "C" {
#endif

#include <initguid.h>
#include <ntddk.h>
#include <ntstrsafe.h>
#include <ntintsafe.h>
#include "Trace.h"

#include <wdf.h>
#include <acx.h>

// noise-filter (TODO 19): the protocol's types, for the event logs below (NF_VMIC_EVENT). Guarded: AppRing.h includes it too.
#include "nf_vmic.h"

#define PAGED_CODE_SEG __declspec(code_seg("PAGE"))
#define INIT_CODE_SEG __declspec(code_seg("INIT"))

// Number of msec for idle timeout.
#define IDLE_POWER_TIMEOUT 5000

extern RECORDER_LOG g_AudioDspLog;
extern RECORDER_LOG g_AudioDspMcLog;

//
// noise-filter (TODO 19, spike check 7): the render-to-capture loopback ring.
//
// One per device, in the device context (WDF contexts are non-paged), so it outlives either stream. Positions are frames on
// one QPC time line shared by both streams: ring frame n holds the audio presented at n / SampleRate seconds of QPC time,
// stored at byte (n % RingFrames) * BlockAlign. Every field is guarded by Lock, which both streams' timer DPCs and their
// PASSIVE_LEVEL state changes, creation and destruction take.
//
#define LOOPBACK_RING_BYTES (48000 * 8 * 2)     // 768,000 bytes: whole frames for block aligns 1-4, 6, 8, 12, 16 and 24

typedef struct _LOOPBACK_FORMAT {
    BOOLEAN         Valid;                  // byte clock == rate * block align; ring and packets hold whole frames
    USHORT          Channels;
    ULONG           SampleRate;
    ULONG           BitsPerSample;
    ULONG           ValidBitsPerSample;
    ULONG           BlockAlign;
    GUID            SubFormat;
} LOOPBACK_FORMAT, *PLOOPBACK_FORMAT;

// The streams open on one circuit (EvtCircuitCreateStream to the stream engine's destruction) in one format. Each circuit
// lists two formats, so two slots per circuit cover every format it can have open at once.
#define LOOPBACK_OPEN_FORMATS 2

typedef struct _LOOPBACK_OPEN {
    ULONG           Count;                  // 0: the slot is free
    LOOPBACK_FORMAT Format;                 // Valid unused (it needs the packet size, known only at Run)
} LOOPBACK_OPEN, *PLOOPBACK_OPEN;

//
// noise-filter (TODO 19): one of a device's event logs (nf_vmic.h, "The event logs"): event n (Sequence n, from 1) in slot
// (n - 1) % Entries of Slots. Its own spin lock, the last in every lock order (Cable.h): taken inside the app ring's and the
// loopback's, and nothing is taken inside it. Init in Codec_EvtBusDeviceAdd; Slots are its LOOPBACK_RING's, which never moves.
//
typedef struct _NF_EVENT_LOG {
    KSPIN_LOCK      Lock;
    ULONG           Entries;
    ULONGLONG       Logged;                 // events logged: the newest's Sequence
    PNF_VMIC_EVENT  Slots;
} NF_EVENT_LOG, *PNF_EVENT_LOG;

typedef struct _LOOPBACK_RING {
    KSPIN_LOCK      Lock;
    PUCHAR          Buffer;                 // LOOPBACK_RING_BYTES, non-paged: Codec_EvtBusDeviceAdd to Codec_EvtDeviceContextCleanup

    // Written by the render stream that owns the ring (the first one to run).
    PVOID           Owner;                  // that CRenderStreamEngine, or NULL
    LOOPBACK_FORMAT RenderFormat;           // the last owner's format (kept after it stops, so its tail can still be read)
    BOOLEAN         RenderActive;           // the owner is running and has written in this run
    ULONG           RenderLeadFrames;       // 0: packets copied as released (a period early); else a packet: copied as they complete.
                                            // Set by a pass that copied; a capture stream takes a change at once, a fall as a
                                            // counted one-time skip (LoopbackCapturePass, LeadSkips)
    ULONGLONG       ValidFrom;              // [ValidFrom, WritePos) is the current run's audio (only its last RingFrames held)
    ULONGLONG       WritePos;

    // Written at stream creation and destruction (LoopbackStreamOpen, LoopbackStreamClose): the formats open on each circuit,
    // so a stream in a format other than one open on the other circuit is refused (STATUS_NOT_SUPPORTED). The capture
    // circuit's go through CaptureStreamOpen and CaptureStreamClose, which hold the same cable's app ring's lock around this
    // one's (Cable.h has the order).
    LOOPBACK_OPEN   RenderOpen[LOOPBACK_OPEN_FORMATS];
    LOOPBACK_OPEN   CaptureOpen[LOOPBACK_OPEN_FORMATS];

    // Counters for the spike, read with the debugger.
    ULONG           IdlePackets;            // capture packets zeroed (all or the end) with no render stream running
    ULONG           MismatchPackets;        // capture packets zeroed because the render and capture formats differ
    ULONG           RefusedOpens;           // stream creations refused (LoopbackStreamOpen): another format open on the other circuit,
                                            // or (not expected) no free format slot on this one
    ULONG           UnderrunPackets;        // capture packets zeroed at the end because the render audio was not there yet
    ULONGLONG       UnderrunFrames;
    ULONG           OverrunPackets;         // capture packets zeroed at the start because the ring had already overwritten them
    ULONG           CaptureAnchors;         // capture read position (re)anchored: Run, glitch adjustment, render lead change
    ULONG           RenderRestarts;         // render runs started (claim, or a time line jump too long to zero), so it numbers them
    ULONG           RenderGapFills;         // render time line moved by a short gap (resume, glitch adjustment), zeroed
    ULONG           RenderLatePackets;      // render packets copied as they completed: the client released them too late
    ULONG           RenderLostPackets;      // render packets gone from the buffer before they were copied (zeroed; not expected)
    ULONG           RenderNotOwner;         // passes of a second render stream (not looped back)
    ULONG           LeadSkips;              // capture reads moved on at once by a fall in the render lead: a one-time skip
    ULONGLONG       LeadSkippedFrames;      // the ring's audio such skips passed over (the difference in leads at most)

    // This device's event logs (NF_EVENT_LOG: their own locks, not Lock): its loopback's and its cable's app ring's
    // (NF_APP_RING.Loopback is this ring; the copy's jump totals are the app ring's, Cable.h). STATS copies them.
    NF_EVENT_LOG    SkipLog;
    NF_EVENT_LOG    UnderrunLog;
    NF_VMIC_EVENT   SkipSlots[NF_VMIC_SKIP_LOG_ENTRIES];
    NF_VMIC_EVENT   UnderrunSlots[NF_VMIC_UNDERRUN_LOG_ENTRIES];
} LOOPBACK_RING, *PLOOPBACK_RING;

// noise-filter (TODO 19, the per-cable design): a cable's app ring, names and statistics (NF_CABLE).
#include "Cable.h"

//
// Define CODEC device context.
//
typedef struct _CODEC_DEVICE_CONTEXT {
    ACXCIRCUIT      Render;
    ACXCIRCUIT      Capture;
    ACXCIRCUIT      Capture2;               // noise-filter (TODO 19): the call microphone (DriverSettings.h)
    WDF_TRI_STATE   ExcludeD3Cold;
    LOOPBACK_RING   Loopback;               // noise-filter (TODO 19)
    NF_CABLE        Cable;                  // noise-filter (TODO 19, the per-cable design): this device's cable (Cable.h)
    BOOLEAN         StartCounted;           // noise-filter (TODO 19, the boot crash breaker, Device.cpp): this binding's start
                                            // is counted in the hardware key and the count not removed yet. PASSIVE_LEVEL only.
    BOOLEAN         StartClearUnflushed;    // noise-filter (TODO 19, the boot crash breaker): the count removed, its removal not
                                            // known to be on disk (its ZwFlushKey failed): flushed again by the next clear, the
                                            // context cleanup at the latest. PASSIVE_LEVEL only.
} CODEC_DEVICE_CONTEXT, * PCODEC_DEVICE_CONTEXT;

//
// Codec driver prototypes.
//
EVT_WDF_DRIVER_DEVICE_ADD           Codec_EvtBusDeviceAdd;
DRIVER_INITIALIZE                   DriverEntry;
EVT_WDF_DRIVER_UNLOAD               AudioCodecDriverUnload;

//
// Codec device callbacks.
//
EVT_WDF_DEVICE_PREPARE_HARDWARE     Codec_EvtDevicePrepareHardware;
EVT_WDF_DEVICE_RELEASE_HARDWARE     Codec_EvtDeviceReleaseHardware;
EVT_WDF_DEVICE_D0_ENTRY             Codec_EvtDeviceD0Entry;
EVT_WDF_DEVICE_D0_EXIT              Codec_EvtDeviceD0Exit;
EVT_WDF_DEVICE_SELF_MANAGED_IO_INIT Codec_EvtDeviceSelfManagedIoInit;     // noise-filter (TODO 19, the boot crash breaker)
EVT_WDF_DEVICE_CONTEXT_CLEANUP      Codec_EvtDeviceContextCleanup;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(CODEC_DEVICE_CONTEXT, GetCodecDeviceContext)

//
// Define DSP device context.
//
typedef struct _DSP_DEVICE_CONTEXT {
    ACXCIRCUIT      Speaker;
    ACXCIRCUIT      MicArray;
    ACXCIRCUIT      SpeakerHp;
    ACXCIRCUIT      MicrophoneHp;
    ACXCIRCUIT      HDMI;
    WDF_TRI_STATE   ExcludeD3Cold;
} DSP_DEVICE_CONTEXT, * PDSP_DEVICE_CONTEXT;

//
// Dsp driver prototypes.
//
EVT_WDF_DRIVER_DEVICE_ADD           Dsp_EvtBusDeviceAdd;
EVT_WDF_DRIVER_UNLOAD               AudioDspDriverUnload;

//
// Dsp device callbacks.
//
EVT_WDF_DEVICE_PREPARE_HARDWARE     Dsp_EvtDevicePrepareHardware;
EVT_WDF_DEVICE_RELEASE_HARDWARE     Dsp_EvtDeviceReleaseHardware;
EVT_WDF_DEVICE_D0_ENTRY             Dsp_EvtDeviceD0Entry;
EVT_WDF_DEVICE_D0_EXIT              Dsp_EvtDeviceD0Exit;
EVT_WDF_DEVICE_CONTEXT_CLEANUP      Dsp_EvtDeviceContextCleanup;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DSP_DEVICE_CONTEXT, GetDspDeviceContext)

//
// Define CODECMC (multicircuit codec) device context.
//
typedef struct _CODECMC_DEVICE_CONTEXT
{
    ACXCIRCUIT Render;
    ACXCIRCUIT Capture;
    ACXCOMPOSITETEMPLATE Composite[2];
    ULONG refComposite[2];
    WDF_TRI_STATE ExcludeD3Cold;
} CODECMC_DEVICE_CONTEXT, *PCODECMC_DEVICE_CONTEXT;

//
// Multicircuit codec driver prototypes.
//
EVT_WDF_DRIVER_DEVICE_ADD CodecMc_EvtBusDeviceAdd;
DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_UNLOAD AudioCodecMcDriverUnload;

//
// Multicircuit codec device callbacks.
//
EVT_WDF_DEVICE_PREPARE_HARDWARE CodecMc_EvtDevicePrepareHardware;
EVT_WDF_DEVICE_RELEASE_HARDWARE CodecMc_EvtDeviceReleaseHardware;
EVT_WDF_DEVICE_D0_ENTRY CodecMc_EvtDeviceD0Entry;
EVT_WDF_DEVICE_D0_EXIT CodecMc_EvtDeviceD0Exit;
EVT_WDF_DEVICE_CONTEXT_CLEANUP CodecMc_EvtDeviceContextCleanup;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(CODECMC_DEVICE_CONTEXT, GetCodecMcDeviceContext)

//
// Composite type definition used in multicircuit codec.
//
typedef enum
{
    CompositeType_RENDER,
    CompositeType_CAPTURE
} CompositeType;

//
// Define DSPMC (multicircuit dsp) device context.
//
typedef struct _DSPMC_DEVICE_CONTEXT
{
    ACXCIRCUIT Render;
    ACXCIRCUIT Capture;
    WDF_TRI_STATE ExcludeD3Cold;
} DSPMC_DEVICE_CONTEXT, *PDSPMC_DEVICE_CONTEXT;

//
// Multicircuit dsp driver prototypes.
//
EVT_WDF_DRIVER_DEVICE_ADD DspMc_EvtBusDeviceAdd;
EVT_WDF_DRIVER_UNLOAD AudioDspMcDriverUnload;

//
// Multicircuit dsp device callbacks.
//
EVT_WDF_DEVICE_PREPARE_HARDWARE DspMc_EvtDevicePrepareHardware;
EVT_WDF_DEVICE_RELEASE_HARDWARE DspMc_EvtDeviceReleaseHardware;
EVT_WDF_DEVICE_D0_ENTRY DspMc_EvtDeviceD0Entry;
EVT_WDF_DEVICE_D0_EXIT DspMc_EvtDeviceD0Exit;
EVT_WDF_DEVICE_CONTEXT_CLEANUP DspMc_EvtDeviceContextCleanup;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DSPMC_DEVICE_CONTEXT, GetDspMcDeviceContext)

// Factory circuit callbacks for multicircuit dsp. 
EVT_ACX_FACTORY_CIRCUIT_CREATE_CIRCUITDEVICE RenderMCDsp_EvtAcxFactoryCircuitCreateCircuitDevice;
EVT_ACX_FACTORY_CIRCUIT_CREATE_CIRCUIT RenderMCDsp_EvtAcxFactoryCircuitCreateCircuit;

EVT_ACX_FACTORY_CIRCUIT_CREATE_CIRCUITDEVICE CaptureMCDsp_EvtAcxFactoryCircuitCreateCircuitDevice;
EVT_ACX_FACTORY_CIRCUIT_CREATE_CIRCUIT CaptureMCDsp_EvtAcxFactoryCircuitCreateCircuit;


/* make internal prototypes usable from C++ */
#ifdef __cplusplus
}
#endif

// Used to store the registry settings path for the driver:
extern UNICODE_STRING g_RegistryPath;

// Driver tag name:
extern ULONG DeviceDriverTag;

// The idle timeout in msec for power policy structure:
extern ULONG IdleTimeoutMsec;

#define DECLARE_CONST_ACXOBJECTBAG_MULTICIRCUIT_SAMPLE_PROPERTY_NAME(name) \
    DECLARE_CONST_UNICODE_STRING(name, L"mc_" #name)

__drv_requiresIRQL(PASSIVE_LEVEL)
PAGED_CODE_SEG
NTSTATUS
CopyRegistrySettingsPath(
    _In_ PUNICODE_STRING RegistryPath
);

PAGED_CODE_SEG
NTSTATUS
Codec_SetPowerPolicy(
    _In_ WDFDEVICE      Device
);

PAGED_CODE_SEG
NTSTATUS
Dsp_SetPowerPolicy(
    _In_ WDFDEVICE      Device
);

PAGED_CODE_SEG
NTSTATUS
CodecMc_SetPowerPolicy(
    _In_ WDFDEVICE      Device
);

PAGED_CODE_SEG
NTSTATUS
DspMc_SetPowerPolicy(
    _In_ WDFDEVICE      Device
);

PAGED_CODE_SEG
NTSTATUS
CodecR_AddStaticRender(
    _In_ WDFDEVICE              Device,
    _In_ const GUID *           ComponentGuid,
    _In_ const UNICODE_STRING * CircuitName
);

PAGED_CODE_SEG
NTSTATUS
CodecC_AddStaticCapture(
    _In_ WDFDEVICE              Device,
    _In_ const GUID *           ComponentGuid,
    _In_ const GUID  *          MicCustomName,
    _In_ const UNICODE_STRING * CircuitName,
    _In_ PCWSTR                 EndpointName    // noise-filter (TODO 19): the endpoint's name (a literal, kept)
);

PAGED_CODE_SEG
NTSTATUS
Speaker_AddStaticRender(
    _In_ WDFDEVICE        Device,
    _In_ GUID             ComponentGuid,
    _In_ UNICODE_STRING   CircuitName,
    _In_ BOOLEAN          IsHeadphones
);

PAGED_CODE_SEG
NTSTATUS
MicArray_AddStaticCapture(
    _In_ WDFDEVICE        Device,
    _In_ GUID             ComponentGuid,
    _In_ GUID             MicCustomName,
    _In_ UNICODE_STRING   CircuitName
);

PAGED_CODE_SEG
NTSTATUS
MicrophoneHp_AddStaticCapture(
    _In_ WDFDEVICE        Device,
    _In_ GUID             ComponentGuid,
    _In_ GUID             MicCustomName,
    _In_ UNICODE_STRING   CircuitName
);

PAGED_CODE_SEG
NTSTATUS
SpeakerHp_AddStaticRender(
    _In_ WDFDEVICE        Device,
    _In_ GUID             ComponentGuid,
    _In_ UNICODE_STRING   CircuitName
);

PAGED_CODE_SEG
NTSTATUS
HDMI_AddStaticRender(
    _In_ WDFDEVICE        Device,
    _In_ GUID             ComponentGuid,
    _In_ UNICODE_STRING   CircuitName
);

PAGED_CODE_SEG
NTSTATUS
RenderMC_AddStaticRender(
    _In_ WDFDEVICE              Device,
    _In_ const GUID *           ComponentGuid,
    _In_ const UNICODE_STRING * CircuitName,
    _In_ const UNICODE_STRING * Uri
);

PAGED_CODE_SEG
NTSTATUS
CaptureMC_AddStaticCapture(
    _In_ WDFDEVICE              Device,
    _In_ const GUID *           ComponentGuid,
    _In_ const GUID  *          MicCustomName,
    _In_ const UNICODE_STRING * CircuitName,
    _In_ const UNICODE_STRING * Uri
);

PAGED_CODE_SEG
NTSTATUS
MicArray_CircuitCleanup(
    _In_ ACXCIRCUIT Device
);

PAGED_CODE_SEG
NTSTATUS
CaptureMCDsp_CircuitCleanup(
    _In_ ACXCIRCUIT Device
);

PAGED_CODE_SEG
NTSTATUS
DspMc_AddFactoryCircuit(
    _In_ WDFDRIVER      Driver,
    _In_ WDFDEVICE      Device
);

NTSTATUS 
CodecMc_AddRenderComposites(_In_ WDFDEVICE Device);

PAGED_CODE_SEG
NTSTATUS 
CodecMc_AddCaptureComposites(_In_ WDFDEVICE Device);

PAGED_CODE_SEG
NTSTATUS 
CodecMc_AddComposites(_In_ WDFDEVICE Device, _In_ CompositeType compositeType);

NTSTATUS
CodecMc_RemoveComposites(_In_ WDFDEVICE Device);
