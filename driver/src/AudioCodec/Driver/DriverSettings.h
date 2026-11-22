/*++

Copyright (c) Microsoft Corporation.  All rights reserved.

    THIS CODE AND INFORMATION IS PROVIDED "AS IS" WITHOUT WARRANTY OF ANY
    KIND, EITHER EXPRESSED OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE
    IMPLIED WARRANTIES OF MERCHANTABILITY AND/OR FITNESS FOR A PARTICULAR
    PURPOSE.

Module Name:

    DriverSettings.h

Abstract:

    Contains guid definitions and other definitions used by the render and capture circuits
    for this specific driver. Driver developers should replace these definitions with their
    own. 

Environment:

    Kernel mode

--*/

// Defining the component ID for the capture circuit. This ID uniquely identifies the circuit instance (vendor specific):
DEFINE_GUID(CODEC_CAPTURE_COMPONENT_GUID, 0xc3ee9ec6, 0x8e8c, 0x49e9, 0xaf, 0x4b, 0xa7, 0xfc, 0x28, 0xe9, 0xd2, 0xe7);

// Defines a custom name for the capture circuit bridge pin:
DEFINE_GUID(MIC_CUSTOM_NAME, 0xd5649dc4, 0x2fa2, 0x418b, 0xb2, 0x78, 0x39, 0x7, 0x64, 0x6b, 0x3, 0xe);

// Defining the component ID for the render circuit. This ID uniquely identifies the circuit instance (vendor specific):
DEFINE_GUID(CODEC_RENDER_COMPONENT_GUID, 0xd03deb75, 0xe5b2, 0x45f7, 0x91, 0xfa, 0xf7, 0xae, 0x42, 0xdd, 0xf, 0xe0);

// This is always the definition for the system container guid:
DEFINE_GUID(SYSTEM_CONTAINER_GUID, 0x00000000, 0x0000, 0x0000, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF);

// Driver developers should update this guid if the container is a device rather than a
// system. Otherwise, this GUID should stay the same:
DEFINE_GUID(DEVICE_CONTAINER_GUID, 0x00000000, 0x0000, 0x0000, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF);

// AudioCodec driver tag:
#define DRIVER_TAG (ULONG) 'CduA'

// The idle timeout in msec for power policy structure:
#define IDLE_TIMEOUT_MSEC (ULONG) 10000

// The WPP control GUID defined in Trace.h should also be updated to be unique.

// This string must match the string defined in AudioCodec.inf for the microphone name:
DECLARE_CONST_UNICODE_STRING(captureCircuitName, L"Microphone0");

//
// noise-filter (TODO 19, the owner's option A, 2026-11-23): a second capture circuit, the call microphone. Its INF interface
// sets a 5 ms default engine period (PKEY_AudioEngine_OEMPeriod) for clients that ask for no period (call apps); the first
// keeps IAudioClient3's small periods. Its own component ID; its endpoint is named by the bridge pin's name callback (CaptureCircuit.cpp).
//
DEFINE_GUID(CODEC_CAPTURE2_COMPONENT_GUID, 0xc5b359a3, 0x1341, 0x46a3, 0xa2, 0xbd, 0x98, 0x5f, 0x57, 0x06, 0xce, 0x1b);
DEFINE_GUID(MIC2_CUSTOM_NAME, 0x3edea053, 0x9f32, 0x4cba, 0xa3, 0xc6, 0xfe, 0xca, 0xe1, 0xe1, 0x70, 0x0f);
DECLARE_CONST_UNICODE_STRING(capture2CircuitName, L"Microphone1");

// This string must match the string defined in AudioCodec.inf for the speaker name:
DECLARE_CONST_UNICODE_STRING(renderCircuitName, L"Speaker0");
