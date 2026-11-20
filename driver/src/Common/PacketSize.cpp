// Copyright (c) Microsoft Corporation.
// Copyright (c) Yamaha Corporation.
// Licensed under the MIT License
// ============================================================================
// This is part of the Microsoft Low-Latency Audio driver project.
// Further information: https://aka.ms/asio
// ============================================================================
//
// Ported into noise-filter's virtual audio driver (TODO 19) from microsoft/low-latency-audio at
// dc76402009e2766a96f77f5f858d2999a41d69a7, src/uac2-driver/CircuitHelper.cpp: the packet-size constraints struct and
// property table (L49-82) and AddPropertyToCircuitInterface (L942-996). Changes (2026-11-20): our values (a 2 ms floor,
// 128-frame RAW and DEFAULT packets, no alignment), a NULL-link check, and the sample's scope_exit and WPP macros in place
// of wil's. The MIT licence text is in driver/LICENSE-MIT-low-latency-audio.txt.

/*++

Module Name:

    PacketSize.cpp

Abstract:

    Declares the endpoints' WaveRT packet sizes (DEVPKEY_KsAudio_PacketSize_Constraints2), so that IAudioClient3
    shared-mode clients are offered periods under 10 ms.

Environment:

    Kernel mode

--*/

// First, as in low-latency-audio's CircuitHelper.cpp: with INITGUID defined before wdm.h pulls in devpropdef.h,
// ksmedia.h's DEFINE_DEVPROPKEY defines DEVPKEY_KsAudio_PacketSize_Constraints2 (selectany) in this file. private.h
// includes ksmedia.h before its own <initguid.h>, so through private.h alone the key is only declared.
#include <initguid.h>
#include "private.h"
#include "public.h"
#include "PacketSize.h"

#ifndef __INTELLISENSE__
#include "PacketSize.tmh"
#endif

#ifndef HNSTIME_PER_MILLISECOND
#define HNSTIME_PER_MILLISECOND 10000
#endif

//
// The values (TODO 19, spike check 2).
//
// NF_MIN_PACKET_PERIOD_HNS: the absolute floor, 2 ms (96 frames at 48 kHz). Learn (audio/low-latency-audio): a mode's
// constraint must be higher than the driver's minimum or the audio stack ignores it, so the floor stays strictly below
// one NF_PACKET_FRAMES packet. (96 frames is exactly 2 ms: with a 2 ms floor it would be ignored.)
//
// NF_PACKET_FRAMES: the packet, which is one engine period (two packets per WaveRT buffer), offered to RAW and DEFAULT
// streams. 128 frames: 2.667 ms at 48 kHz, 2.902 ms at 44.1 kHz. It is the inbox HDAudio driver's minimum.
//
// Both can be set at build time for a trial (Build-Driver.ps1 -PacketFrames 96 defines NF_PACKET_FRAMES 96 and a 1 ms floor,
// NF_MIN_PACKET_PERIOD_HNS 10000, through the CL environment variable); the C_ASSERTs below keep the floor under the packet.
//
#ifndef NF_MIN_PACKET_PERIOD_HNS
#define NF_MIN_PACKET_PERIOD_HNS    (2 * HNSTIME_PER_MILLISECOND)
#endif
#ifndef NF_PACKET_FRAMES
#define NF_PACKET_FRAMES            128
#endif

C_ASSERT((ULONGLONG)NF_PACKET_FRAMES * 10000000ULL / 48000 > (ULONGLONG)NF_MIN_PACKET_PERIOD_HNS);
C_ASSERT((ULONGLONG)NF_PACKET_FRAMES * 10000000ULL / 44100 > (ULONGLONG)NF_MIN_PACKET_PERIOD_HNS);

typedef struct _NF_PACKETSIZE_CONSTRAINTS
{
    KSAUDIO_PACKETSIZE_CONSTRAINTS2              TransportPacketConstraints;          // holds ProcessingModeConstraints[0]
    KSAUDIO_PACKETSIZE_PROCESSINGMODE_CONSTRAINT AdditionalProcessingConstraints[1];  // [1]
} NF_PACKETSIZE_CONSTRAINTS;

// The value is read as one variable-length KSAUDIO_PACKETSIZE_CONSTRAINTS2, so there must be no padding between the parts.
C_ASSERT(sizeof(NF_PACKETSIZE_CONSTRAINTS) ==
         FIELD_OFFSET(KSAUDIO_PACKETSIZE_CONSTRAINTS2, ProcessingModeConstraints) +
         2 * sizeof(KSAUDIO_PACKETSIZE_PROCESSINGMODE_CONSTRAINT));

static const NF_PACKETSIZE_CONSTRAINTS s_PacketSizeConstraints = {
    {
        NF_MIN_PACKET_PERIOD_HNS,                       // MinPacketPeriodInHns: 20000 hns (2 ms) by default
        FILE_BYTE_ALIGNMENT,                            // PacketSizeFileAlignment: none (no DMA; the CPU copies packets)
        0,                                              // MaxPacketSizeInBytes: no maximum (AllocateRtPackets takes any size)
        2,                                              // NumProcessingModeConstraints
        {
            STATIC_AUDIO_SIGNALPROCESSINGMODE_RAW,      // every stream on our pins is RAW today
            NF_PACKET_FRAMES,                           // SamplesPerProcessingPacket
            0,                                          // ProcessingPacketDurationInHns: ignored, samples are nonzero
        },
    },
    {
        {
            STATIC_AUDIO_SIGNALPROCESSINGMODE_DEFAULT,  // for when the pins gain a DEFAULT format list
            NF_PACKET_FRAMES,                           // SamplesPerProcessingPacket
            0,                                          // ProcessingPacketDurationInHns
        },
    },
};

static const DSP_DEVPROPERTY c_PacketSizeProperties[] = {
    {
        &DEVPKEY_KsAudio_PacketSize_Constraints2,                           // Key
        DEVPROP_TYPE_BINARY,                                                // Type
        sizeof(s_PacketSizeConstraints),                                    // BufferSize (64 bytes)
        const_cast<NF_PACKETSIZE_CONSTRAINTS *>(&s_PacketSizeConstraints),  // Buffer (the I/O manager keeps a copy)
    },
};

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
AddPropertyToCircuitInterface(
    ACXCIRCUIT              Circuit,
    ULONG                   PropertyCount,
    const DSP_DEVPROPERTY * Properties
)
{
    PAGED_CODE();

    NTSTATUS       status;
    UNICODE_STRING acxLink{};
    UNICODE_STRING audioLink{};

    //
    // Learn (AcxCircuitGetSymbolicLinkName, ACX 1.1, PASSIVE_LEVEL): for a single-circuit endpoint the link is valid
    // after the driver adds the circuit to the device.
    //
    WDFSTRING wdfLink = AcxCircuitGetSymbolicLinkName(Circuit);
    RETURN_NTSTATUS_IF_TRUE(wdfLink == nullptr, STATUS_INVALID_DEVICE_STATE);

    WdfStringGetUnicodeString(wdfLink, &acxLink);
    RETURN_NTSTATUS_IF_TRUE(acxLink.Length == 0 || acxLink.Buffer == nullptr, STATUS_INVALID_DEVICE_STATE);

    //
    // The KSCATEGORY_AUDIO interface with the same reference string: the circuit name, which the INF's AddInterface
    // lines registered at install. PASSIVE_LEVEL, system thread.
    //
    status = IoGetDeviceInterfaceAlias(&acxLink, &KSCATEGORY_AUDIO, &audioLink);
    RETURN_NTSTATUS_IF_FAILED(status);

    auto audioLinkScope = scope_exit([&audioLink]() {
        RtlFreeUnicodeString(&audioLink);
        });

    //
    // Set the properties on the circuit's KSCATEGORY_AUDIO interface. PASSIVE_LEVEL (the WDK 28000 header), system thread.
    // Persistent, so
    // the value is already there on later boots; it is rewritten on every start.
    //
    for (ULONG i = 0; i < PropertyCount; ++i)
    {
        status = IoSetDeviceInterfacePropertyData(&audioLink,
                                                  Properties[i].PropertyKey,
                                                  LOCALE_NEUTRAL,
                                                  PLUGPLAY_PROPERTY_PERSISTENT,
                                                  Properties[i].Type,
                                                  Properties[i].BufferSize,
                                                  Properties[i].Buffer);
        RETURN_NTSTATUS_IF_FAILED(status);
    }

    return STATUS_SUCCESS;
}

_Use_decl_annotations_
PAGED_CODE_SEG
NTSTATUS
PublishPacketSizeConstraints(
    ACXCIRCUIT Circuit
)
{
    PAGED_CODE();

    return AddPropertyToCircuitInterface(Circuit, (ULONG)RTL_NUMBER_OF(c_PacketSizeProperties), c_PacketSizeProperties);
}
