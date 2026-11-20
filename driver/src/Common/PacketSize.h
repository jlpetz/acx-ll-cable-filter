// Copyright (c) Microsoft Corporation.
// Copyright (c) Yamaha Corporation.
// Licensed under the MIT License
// ============================================================================
// This is part of the Microsoft Low-Latency Audio driver project.
// Further information: https://aka.ms/asio
// ============================================================================
//
// Ported into noise-filter's virtual audio driver (TODO 19) from microsoft/low-latency-audio at
// dc76402009e2766a96f77f5f858d2999a41d69a7, src/uac2-driver/CircuitHelper.h (DSP_DEVPROPERTY, AddPropertyToCircuitInterface).
// Changes (2026-11-20): our packet-size wrapper added, SAL updated, the pointer typedef fixed. The MIT licence text is in
// driver/LICENSE-MIT-low-latency-audio.txt.

/*++

Module Name:

    PacketSize.h

Abstract:

    Publishes DEVPKEY_KsAudio_PacketSize_Constraints2 on an ACX circuit's KSCATEGORY_AUDIO device interface.
    Include after private.h or public.h (needs DEVPROPKEY, DEVPROPTYPE, ACXCIRCUIT and PAGED_CODE_SEG).

Environment:

    Kernel mode

--*/

#pragma once

typedef struct _DSP_DEVPROPERTY
{
    const DEVPROPKEY *                       PropertyKey;
    DEVPROPTYPE                              Type;
    ULONG                                    BufferSize;
    _Field_size_bytes_opt_(BufferSize) PVOID Buffer;
} DSP_DEVPROPERTY, *PDSP_DEVPROPERTY;

//
// Sets each property on the KSCATEGORY_AUDIO alias of the circuit's symbolic link. Call after AcxDeviceAddCircuit and
// before the device enters D0 (the interface is enabled at D0). PASSIVE_LEVEL, system thread.
//
_IRQL_requires_max_(PASSIVE_LEVEL)
PAGED_CODE_SEG
NTSTATUS
AddPropertyToCircuitInterface(
    _In_                      ACXCIRCUIT              Circuit,
    _In_                      ULONG                   PropertyCount,
    _In_reads_(PropertyCount) const DSP_DEVPROPERTY * Properties
);

//
// Publishes our WaveRT packet-size constraints (PacketSize.cpp) on the circuit's KSCATEGORY_AUDIO interface.
//
_IRQL_requires_max_(PASSIVE_LEVEL)
PAGED_CODE_SEG
NTSTATUS
PublishPacketSizeConstraints(
    _In_ ACXCIRCUIT Circuit
);
