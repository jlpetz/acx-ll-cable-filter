/*++

Copyright (c) Microsoft Corporation.  All rights reserved.

Module Name:

    Device.cpp - Device handling events for example driver.

Abstract:

   This file contains the device entry points and callbacks.

Environment:

    Kernel-mode Driver Framework

--*/

#include <wdm.h>
#include <windef.h>
#include "public.h"
// noise-filter (TODO 19, the per-cable design): DEVPKEY_Device_InstanceId and DEVPKEY_Device_FriendlyName, defined here
// (selectany): public.h includes <initguid.h> first.
#include <devpkey.h>
#include <devguid.h>
#include <ks.h>
#include <mmsystem.h>
#include <ksmedia.h>
#include "streamengine.h"
#include "DriverSettings.h"
#include "PacketSize.h"
#include "AppRing.h"

#ifndef __INTELLISENSE__
#include "device.tmh"
#endif

UNICODE_STRING g_RegistryPath = { 0 };      // This is used to store the registry settings path for the driver

ULONG DeviceDriverTag = DRIVER_TAG;

ULONG IdleTimeoutMsec = IDLE_TIMEOUT_MSEC;

__drv_requiresIRQL(PASSIVE_LEVEL)
PAGED_CODE_SEG
NTSTATUS
CopyRegistrySettingsPath(
    _In_ PUNICODE_STRING RegistryPath
)
/*++

Routine Description:

Copies the following registry path to a global variable.

\REGISTRY\MACHINE\SYSTEM\ControlSetxxx\Services\<driver>\Parameters

Arguments:

RegistryPath - Registry path passed to DriverEntry

Returns:

NTSTATUS - SUCCESS if able to configure the framework

--*/

{
    PAGED_CODE();

    //
    // Initializing the unicode string, so that if it is not allocated it will not be deallocated too.
    //
    RtlInitUnicodeString(&g_RegistryPath, nullptr);

    g_RegistryPath.MaximumLength = RegistryPath->Length + sizeof(WCHAR);

    g_RegistryPath.Buffer = (PWCH)ExAllocatePool2(POOL_FLAG_PAGED, g_RegistryPath.MaximumLength, DRIVER_TAG);

    if (g_RegistryPath.Buffer == nullptr)
    {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    RtlAppendUnicodeToString(&g_RegistryPath, RegistryPath->Buffer);

    return STATUS_SUCCESS;
}

//
// noise-filter (TODO 19, the owner's options A and D, 2026-11-23): which microphones this device makes, from its hardware key's
// NfMicrophones (the INF sets it per hardware ID): 1 the low-latency one (Microphone0), 2 the call one (Microphone1), 3 both
// (NF_MIC_LOW_LATENCY and NF_MIC_CALL, Cable.h). Absent, 0 or out of range: both.
//
PAGED_CODE_SEG
static
ULONG
NfReadMicrophones(
    _In_ WDFDEVICE Device
)
{
    DECLARE_CONST_UNICODE_STRING(valueName, L"NfMicrophones");
    WDFKEY  key;
    ULONG   value = 0;

    PAGED_CODE();

    if (NT_SUCCESS(WdfDeviceOpenRegistryKey(Device, PLUGPLAY_REGKEY_DEVICE, KEY_READ, WDF_NO_OBJECT_ATTRIBUTES, &key)))
    {
        if (!NT_SUCCESS(WdfRegistryQueryULong(key, &valueName, &value)))
        {
            value = 0;
        }
        WdfRegistryClose(key);
    }

    value &= (NF_MIC_LOW_LATENCY | NF_MIC_CALL);
    return (value != 0) ? value : (NF_MIC_LOW_LATENCY | NF_MIC_CALL);
}

//
// noise-filter (TODO 19, the per-cable design, section 4): a cable's identity and label, carried in its instance ID, which the
// setup tool fixes when it creates the device node: ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n> (Cable.h). Nothing the driver reads
// here is written after the node is registered, so Plug and Play can't start a node before its identity is there.
//

PAGED_CODE_SEG
static
WCHAR
NfUpcase(
    _In_ WCHAR C
)
{
    PAGED_CODE();

    return (C >= L'a' && C <= L'z') ? (WCHAR)(C - L'a' + L'A') : C;
}

// A hex digit's value in *Value; FALSE if C isn't one.
PAGED_CODE_SEG
static
BOOLEAN
NfHexDigit(
    _In_    WCHAR   C,
    _Out_   PUCHAR  Value
)
{
    PAGED_CODE();

    C = NfUpcase(C);
    if (C >= L'0' && C <= L'9')
    {
        *Value = (UCHAR)(C - L'0');
        return TRUE;
    }
    if (C >= L'A' && C <= L'F')
    {
        *Value = (UCHAR)(C - L'A' + 10);
        return TRUE;
    }
    *Value = 0;
    return FALSE;
}

//
// A GUID's 36 characters without braces, xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx in either case: RtlGUIDFromString's form less the
// braces, checked character by character (so nothing is read past the 36).
//
PAGED_CODE_SEG
static
BOOLEAN
NfParseGuid36(
    _In_reads_(NF_CABLE_ID_CHARS)   PCWSTR  Text,
    _Out_                           GUID*   Guid
)
{
    UCHAR   bytes[16] = {};
    ULONG   nibble = 0;
    ULONG   i;

    PAGED_CODE();

    RtlZeroMemory(Guid, sizeof(*Guid));
    for (i = 0; i < NF_CABLE_ID_CHARS; i++)
    {
        UCHAR value;

        if (i == 8 || i == 13 || i == 18 || i == 23)
        {
            if (Text[i] != L'-')
            {
                return FALSE;
            }
            continue;
        }
        if (!NfHexDigit(Text[i], &value))
        {
            return FALSE;
        }
        bytes[nibble / 2] = (UCHAR)((bytes[nibble / 2] << 4) | value);
        nibble++;
    }

    // The text's order: Data1, Data2 and Data3 most significant first, then Data4's eight bytes.
    Guid->Data1 = ((ULONG)bytes[0] << 24) | ((ULONG)bytes[1] << 16) | ((ULONG)bytes[2] << 8) | bytes[3];
    Guid->Data2 = (USHORT)((bytes[4] << 8) | bytes[5]);
    Guid->Data3 = (USHORT)((bytes[6] << 8) | bytes[7]);
    RtlCopyMemory(Guid->Data4, &bytes[8], sizeof(Guid->Data4));
    return TRUE;
}

//
// What follows the instance ID's last backslash: ACXLLCABLE_<label>_<id>_<n>, without case. The label one to eight of A-Z and
// 0-9 (returned upper case), the id a GUID's 36 characters, n a decimal from 1 without leading zeros (so one cable has one
// instance ID for each n). STATUS_DEVICE_CONFIGURATION_ERROR for anything else: a node made outside the setup tool (devcon,
// Device Manager's Add legacy hardware: ROOT\MEDIA\NNNN) has no identity.
//
PAGED_CODE_SEG
static
NTSTATUS
NfParseCableInstanceId(
    _In_reads_(Chars)                               PCWSTR  InstanceId,
    _In_                                            size_t  Chars,
    _Out_                                           GUID*   Id,
    _Out_writes_(NF_CABLE_LABEL_MAX_CHARS + 1)      PWCHAR  Label,
    _Out_                                           PULONG  InstanceNumber
)
{
    static const WCHAR  prefix[] = NF_CABLE_INSTANCE_PREFIX;
    const size_t        prefixChars = ARRAYSIZE(prefix) - 1;
    PCWSTR              p = InstanceId;
    size_t              rest = Chars;
    size_t              labelChars = 0;
    ULONGLONG           n = 0;
    size_t              i;

    PAGED_CODE();

    RtlZeroMemory(Id, sizeof(*Id));
    Label[0] = L'\0';
    *InstanceNumber = 0;

    for (i = 0; i < Chars; i++)
    {
        if (InstanceId[i] == L'\\')
        {
            p = InstanceId + i + 1;
            rest = Chars - i - 1;
        }
    }

    if (rest < prefixChars)
    {
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    for (i = 0; i < prefixChars; i++)
    {
        if (NfUpcase(p[i]) != prefix[i])
        {
            return STATUS_DEVICE_CONFIGURATION_ERROR;
        }
    }
    p += prefixChars;
    rest -= prefixChars;

    // The label, up to the next underscore.
    while (labelChars < rest && p[labelChars] != L'_')
    {
        WCHAR c = NfUpcase(p[labelChars]);

        if (labelChars == NF_CABLE_LABEL_MAX_CHARS || !((c >= L'A' && c <= L'Z') || (c >= L'0' && c <= L'9')))
        {
            Label[0] = L'\0';
            return STATUS_DEVICE_CONFIGURATION_ERROR;
        }
        Label[labelChars++] = c;
    }
    Label[labelChars] = L'\0';
    if (labelChars == 0 || labelChars == rest)
    {
        Label[0] = L'\0';
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    p += labelChars + 1;
    rest -= labelChars + 1;

    // The identity, then an underscore and at least one digit.
    if (rest < NF_CABLE_ID_CHARS + 2 || p[NF_CABLE_ID_CHARS] != L'_' || !NfParseGuid36(p, Id))
    {
        Label[0] = L'\0';
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    p += NF_CABLE_ID_CHARS + 1;
    rest -= NF_CABLE_ID_CHARS + 1;

    // n: 1 to 4294967295, decimal, no leading zero, and nothing after it.
    if (rest == 0 || rest > 10 || p[0] == L'0')
    {
        Label[0] = L'\0';
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    for (i = 0; i < rest; i++)
    {
        if (p[i] < L'0' || p[i] > L'9')
        {
            Label[0] = L'\0';
            return STATUS_DEVICE_CONFIGURATION_ERROR;
        }
        n = n * 10 + (ULONGLONG)(p[i] - L'0');
    }
    if (n == 0 || n > MAXULONG)
    {
        Label[0] = L'\0';
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }

    *InstanceNumber = (ULONG)n;
    return STATUS_SUCCESS;
}

// TRUE if Chars characters of Text are Upper's (upper case), without case.
PAGED_CODE_SEG
static
BOOLEAN
NfMatchUpper(
    _In_reads_(Chars)   PCWSTR  Text,
    _In_reads_(Chars)   PCWSTR  Upper,
    _In_                size_t  Chars
)
{
    size_t i;

    PAGED_CODE();

    for (i = 0; i < Chars; i++)
    {
        if (NfUpcase(Text[i]) != Upper[i])
        {
            return FALSE;
        }
    }
    return TRUE;
}

//
// The instance ID from the name of the device's hardware key (PLUGPLAY_REGKEY_DEVICE):
// \REGISTRY\MACHINE\SYSTEM\<control set>\Enum\<instance ID>\Device Parameters. NfQueryInstanceId's fallback, for a kernel whose
// property store doesn't answer DEVPKEY_Device_InstanceId (unconfirmed until S0a: nothing yet shows it does). The name has that
// form or STATUS_DEVICE_CONFIGURATION_ERROR; every read is bounded by what ZwQueryKey returned. PASSIVE_LEVEL (wdffdo.h,
// ZwQueryKey), before WdfDeviceCreate.
//
// KeyNameInformation's answer: ntddk.h's KEY_NAME_INFORMATION, which this file doesn't see (wdm.h, included first, defines
// _NTDDK_, so ntddk.h's body is skipped).
typedef struct _NF_KEY_NAME_INFORMATION {
    ULONG   NameLength;                     // bytes
    WCHAR   Name[1];
} NF_KEY_NAME_INFORMATION, *PNF_KEY_NAME_INFORMATION;

PAGED_CODE_SEG
static
NTSTATUS
NfInstanceIdFromHardwareKey(
    _In_                                        PWDFDEVICE_INIT DeviceInit,
    _Out_writes_(NF_CABLE_INSTANCE_ID_CHARS)    PWCHAR          Buffer,
    _Out_                                       size_t*         Chars
)
{
    static const WCHAR          enumPart[] = L"\\ENUM\\";
    static const WCHAR          leaf[] = L"\\DEVICE PARAMETERS";
    const size_t                enumChars = ARRAYSIZE(enumPart) - 1;
    const size_t                leafChars = ARRAYSIZE(leaf) - 1;
    // The control set's path is about 40 characters, the instance ID at most 199: room to spare.
    const ULONG                 infoBytes = FIELD_OFFSET(NF_KEY_NAME_INFORMATION, Name) + 512 * sizeof(WCHAR);
    PNF_KEY_NAME_INFORMATION    info;
    WDFKEY                      key = NULL;
    ULONG                       returned = 0;
    size_t                      nameChars;
    size_t                      end;
    size_t                      start = 0;
    BOOLEAN                     found = FALSE;
    size_t                      i;
    NTSTATUS                    status;

    PAGED_CODE();

    *Chars = 0;
    RtlZeroMemory(Buffer, NF_CABLE_INSTANCE_ID_CHARS * sizeof(WCHAR));

    info = (PNF_KEY_NAME_INFORMATION)ExAllocatePool2(POOL_FLAG_PAGED, infoBytes, DeviceDriverTag);
    if (info == nullptr)
    {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    status = WdfFdoInitOpenRegistryKey(DeviceInit, PLUGPLAY_REGKEY_DEVICE, KEY_READ, WDF_NO_OBJECT_ATTRIBUTES, &key);
    if (NT_SUCCESS(status))
    {
        status = ZwQueryKey(WdfRegistryWdmGetHandle(key), KeyNameInformation, info, infoBytes, &returned);
        WdfRegistryClose(key);
    }
    if (NT_SUCCESS(status))
    {
        status = STATUS_DEVICE_CONFIGURATION_ERROR;
        nameChars = info->NameLength / sizeof(WCHAR);
        if (nameChars > (infoBytes - FIELD_OFFSET(NF_KEY_NAME_INFORMATION, Name)) / sizeof(WCHAR))
        {
            nameChars = 0;
        }

        // The name ends in \Device Parameters (without case), and the instance ID is what lies between it and the first \Enum\.
        if (nameChars > leafChars && NfMatchUpper(&info->Name[nameChars - leafChars], leaf, leafChars))
        {
            end = nameChars - leafChars;
            for (i = 0; i + enumChars <= end; i++)
            {
                if (NfMatchUpper(&info->Name[i], enumPart, enumChars))
                {
                    start = i + enumChars;
                    found = TRUE;
                    break;
                }
            }
            if (found && start < end && end - start < NF_CABLE_INSTANCE_ID_CHARS &&
                NT_SUCCESS(RtlStringCchCopyNW(Buffer, NF_CABLE_INSTANCE_ID_CHARS, &info->Name[start], end - start)))
            {
                status = RtlStringCchLengthW(Buffer, NF_CABLE_INSTANCE_ID_CHARS, Chars);
            }
        }
    }

    ExFreePoolWithTag(info, DeviceDriverTag);
    return status;
}

//
// The device's instance ID, NUL-terminated in Buffer, and its length in characters: DEVPKEY_Device_InstanceId, or, if the
// property store doesn't answer it, the hardware key's name (NfInstanceIdFromHardwareKey; the debug message says so, and S0a
// should look for it). PASSIVE_LEVEL (wdffdo.h), before WdfDeviceCreate.
//
PAGED_CODE_SEG
static
NTSTATUS
NfQueryInstanceId(
    _In_                                        PWDFDEVICE_INIT DeviceInit,
    _Out_writes_(NF_CABLE_INSTANCE_ID_CHARS)    PWCHAR          Buffer,
    _Out_                                       size_t*         Chars
)
{
    WDF_DEVICE_PROPERTY_DATA    property;
    ULONG                       bytes = 0;
    DEVPROPTYPE                 type = DEVPROP_TYPE_EMPTY;
    NTSTATUS                    status;

    PAGED_CODE();

    *Chars = 0;
    RtlZeroMemory(Buffer, NF_CABLE_INSTANCE_ID_CHARS * sizeof(WCHAR));

    WDF_DEVICE_PROPERTY_DATA_INIT(&property, &DEVPKEY_Device_InstanceId);
    property.Lcid = LOCALE_NEUTRAL;
    status = WdfFdoInitQueryPropertyEx(DeviceInit, &property, NF_CABLE_INSTANCE_ID_CHARS * sizeof(WCHAR), Buffer, &bytes, &type);
    if (status == STATUS_BUFFER_TOO_SMALL)
    {
        // Longer than MAX_DEVICE_ID_LEN, so not ours either.
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    if (NT_SUCCESS(status) && type == DEVPROP_TYPE_STRING && bytes >= 2 * sizeof(WCHAR) &&
        bytes <= NF_CABLE_INSTANCE_ID_CHARS * sizeof(WCHAR))
    {
        // Whatever the property held, the string ends inside the buffer.
        Buffer[NF_CABLE_INSTANCE_ID_CHARS - 1] = L'\0';
        return RtlStringCchLengthW(Buffer, NF_CABLE_INSTANCE_ID_CHARS, Chars);
    }

    // Not answered as a string: the hardware key's name. An error-level message, so it shows without a debug filter.
    DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_ERROR_LEVEL,
               "AudioCodec: DEVPKEY_Device_InstanceId not answered (0x%08X, type 0x%X, %lu bytes): the instance ID from the "
               "hardware key's name\n", status, type, bytes);
    return NfInstanceIdFromHardwareKey(DeviceInit, Buffer, Chars);
}

// A circuit's component ID for this cable: the identity XOR the circuit's constant (DriverSettings.h), so each cable's are its own.
PAGED_CODE_SEG
static
VOID
NfDeriveComponentId(
    _In_    const GUID* Identity,
    _In_    const GUID* Constant,
    _Out_   GUID*       Out
)
{
    const UCHAR*    a = (const UCHAR*)Identity;
    const UCHAR*    b = (const UCHAR*)Constant;
    UCHAR*          o = (UCHAR*)Out;
    ULONG           i;

    PAGED_CODE();

    for (i = 0; i < sizeof(GUID); i++)
    {
        o[i] = (UCHAR)(a[i] ^ b[i]);
    }
}

//
// The device's FriendlyName, "ACX-LL-Cable <label>", which names its endpoints' adapter part: "Microphone (ACX-LL-Cable D)"
// (the per-cable design, section 8). Set at every binding, before the circuits' interfaces are enabled at start and the
// endpoints are made. The setup tool sets the same name before it registers the node, a second layer: a failure here is
// reported and costs nothing else. WdfDeviceAssignProperty: up to APC_LEVEL (wdfdevice.h).
//
PAGED_CODE_SEG
static
VOID
NfAssignFriendlyName(
    _In_ WDFDEVICE      Device,
    _In_ PNF_CABLE      Cable
)
{
    WDF_DEVICE_PROPERTY_DATA    property;
    WCHAR                       name[sizeof("ACX-LL-Cable ") + NF_CABLE_LABEL_MAX_CHARS] = {};
    size_t                      chars = 0;
    NTSTATUS                    status;

    PAGED_CODE();

    status = RtlStringCchPrintfW(name, ARRAYSIZE(name), L"ACX-LL-Cable %ws", Cable->Label);
    if (NT_SUCCESS(status))
    {
        status = RtlStringCchLengthW(name, ARRAYSIZE(name), &chars);
    }
    if (NT_SUCCESS(status))
    {
        WDF_DEVICE_PROPERTY_DATA_INIT(&property, &DEVPKEY_Device_FriendlyName);
        property.Lcid = LOCALE_NEUTRAL;
        property.Flags = PLUGPLAY_PROPERTY_PERSISTENT;
        status = WdfDeviceAssignProperty(Device, &property, DEVPROP_TYPE_STRING, (ULONG)((chars + 1) * sizeof(WCHAR)), name);
    }
    if (!NT_SUCCESS(status))
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_WARNING_LEVEL, "AudioCodec: cable %ws's friendly name not set (0x%08X)\n",
                   Cable->Label, status);
    }
}

//
// noise-filter (TODO 19): the boot crash breaker (ours, for the owner's go; removable with this block, its calls in
// Codec_EvtBusDeviceAdd and Codec_EvtDeviceContextCleanup, Codec_EvtDeviceSelfManagedIoInit and CODEC_DEVICE_CONTEXT's
// StartCounted). Driver Verifier stays on at boot on the dev box, so a deterministic bugcheck in a cable's start would come
// back at every boot. AddDevice counts the device's start attempts in its hardware key (NfStartAttempts, Cable.h), flushed
// before it goes on, and the count is cleared once the start has completed (EvtDeviceSelfManagedIoInit, the last of the
// start's callbacks) or the binding has ended without a crash (a failed AddDevice or start, a removal: a clean failure doesn't
// loop the box). A device whose last start never completed, so that nothing cleared its count, is refused at AddDevice with
// NF_STATUS_START_BREAKER (Device Manager: code 31, the status in DEVPKEY_Device_ProblemStatus), a System event (source
// AudioCodec) and a debug message, both saying why, the second how to clear it. The setup tool deletes the count when it
// installs a cable. Everything here is PASSIVE_LEVEL (the registry calls, wdfregistry.h and wdm.h).
//
// Why at one start, not two: at boot the cables most likely start before the Session Manager makes the SYSTEM hive writable
// (kernel Plug and Play starts the root-enumerated devices of installed drivers before smss.exe runs; to confirm on the box:
// after a normal boot, a cable's Device Parameters key's last write time, this count's clear, against smss.exe's start time).
// Registry writes before then stay in memory, and a crash loses them, so no boot's count reaches disk. A crash in start first
// shows in a session (an install, an update, an enable), whose count of 1 is on disk; at a limit of two every boot after would
// read 1, count 2 in memory only and crash again, for good. At one, that first boot refuses the device. A false refusal needs a
// crash of anything, or a power loss, within a cable's start in a session, a fraction of a second; the setup tool's add clears it.
//
// What it can't catch:
// - a crash that only happens at boot, if the cables start before the hive is writable (above): its count never reaches disk;
// - a crash before the count is on disk: in DriverEntry, or in AddDevice before the write (the instance ID's query and parse,
//   the key's open);
// - a crash after the start completed: in a stream (Audiosrv's first opens after boot among them), an IOCTL, a removal;
// - a crash in the framework's or ACX's start work after our last start callback, or in the clear itself.
// A crash in one cable's start leaves a count on every cable whose start was in flight then (an update starts them together),
// and each is refused. A hang in start leaves its count too (no callback clears it): the next boot refuses the device.
//
DECLARE_CONST_UNICODE_STRING(g_NfStartAttemptsValue, NF_CABLE_START_ATTEMPTS_VALUE);

// Count into the key (0: the value removed), flushed so it is on disk before the caller goes on.
PAGED_CODE_SEG
static
NTSTATUS
NfBreakerStore(
    _In_ WDFKEY Key,
    _In_ ULONG  Count
)
{
    NTSTATUS status;

    PAGED_CODE();

    if (Count == 0)
    {
        status = WdfRegistryRemoveValue(Key, &g_NfStartAttemptsValue);
        if (status == STATUS_OBJECT_NAME_NOT_FOUND)
        {
            // Nothing counted: nothing to write.
            return STATUS_SUCCESS;
        }
    }
    else
    {
        status = WdfRegistryAssignULong(Key, &g_NfStartAttemptsValue, Count);
    }
    if (NT_SUCCESS(status))
    {
        status = ZwFlushKey(WdfRegistryWdmGetHandle(Key));
    }
    return status;
}

//
// A refusal's System event, so an unattended boot shows why without a debugger: the I/O manager's error log, its source the
// driver's service name (AudioCodec), its code NF_STATUS_START_BREAKER (an error, event ID 1), its one string naming the cable
// and the count. No message file is registered for the source, so Event Viewer shows the string under "the following
// information was included with the event". Best effort: without an entry, no event. IoAllocateErrorLogEntry and
// IoWriteErrorLogEntry: up to DISPATCH_LEVEL (wdm.h).
//
#define NF_BREAKER_EVENT_CHARS      96u         // the most an entry holds after its header: ERROR_LOG_MAXIMUM_SIZE (240 bytes)
C_ASSERT(sizeof(IO_ERROR_LOG_PACKET) + NF_BREAKER_EVENT_CHARS * sizeof(WCHAR) <= ERROR_LOG_MAXIMUM_SIZE);

PAGED_CODE_SEG
static
VOID
NfBreakerReportRefusal(
    _In_ WDFDRIVER  Driver,
    _In_ PCWSTR     Label,
    _In_ ULONG      Attempts
)
{
    WCHAR                   text[NF_BREAKER_EVENT_CHARS] = {};
    size_t                  chars = 0;
    ULONG                   textBytes;
    PIO_ERROR_LOG_PACKET    entry;

    PAGED_CODE();

    // At most 90 characters with an eight-character label and the largest count.
    if (!NT_SUCCESS(RtlStringCchPrintfW(text, ARRAYSIZE(text),
                                        L"ACX-LL-Cable %ws refused: its last start never completed (NfStartAttempts %lu)", Label,
                                        Attempts)) ||
        !NT_SUCCESS(RtlStringCchLengthW(text, ARRAYSIZE(text), &chars)))
    {
        return;
    }
    textBytes = (ULONG)((chars + 1) * sizeof(WCHAR));

    entry = (PIO_ERROR_LOG_PACKET)IoAllocateErrorLogEntry(WdfDriverWdmGetDriverObject(Driver),
                                                          (UCHAR)(sizeof(IO_ERROR_LOG_PACKET) + textBytes));
    if (entry == NULL)
    {
        return;
    }
    RtlZeroMemory(entry, sizeof(IO_ERROR_LOG_PACKET));
    entry->ErrorCode = NF_STATUS_START_BREAKER;
    entry->FinalStatus = NF_STATUS_START_BREAKER;
    entry->UniqueErrorValue = Attempts;
    entry->NumberOfStrings = 1;
    entry->StringOffset = (USHORT)sizeof(IO_ERROR_LOG_PACKET);
    RtlCopyMemory((PUCHAR)entry + sizeof(IO_ERROR_LOG_PACKET), text, textBytes);
    IoWriteErrorLogEntry(entry);
}

//
// AddDevice's part, before anything is made: refuses the device at the limit, else counts this attempt. *Key: the hardware
// key, open, once the attempt is counted (the caller clears it with NfBreakerClose if AddDevice fails before the framework
// device exists, as no cleanup callback can then); NULL if the key can't be opened, and the device goes on without the breaker
// (it guards against a crash loop and shouldn't stop a cable by itself).
//
PAGED_CODE_SEG
static
NTSTATUS
NfBreakerEnter(
    _In_    WDFDRIVER       Driver,
    _In_    PWDFDEVICE_INIT DeviceInit,
    _In_    PCWSTR          InstanceId,
    _In_    PCWSTR          Label,
    _Out_   WDFKEY*         Key
)
{
    WDFKEY      key = NULL;
    ULONG       attempts = 0;
    NTSTATUS    status;

    PAGED_CODE();

    *Key = NULL;

    status = WdfFdoInitOpenRegistryKey(DeviceInit, PLUGPLAY_REGKEY_DEVICE, KEY_QUERY_VALUE | KEY_SET_VALUE, WDF_NO_OBJECT_ATTRIBUTES,
                                       &key);
    if (!NT_SUCCESS(status))
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_WARNING_LEVEL,
                   "AudioCodec: %ws starts without the crash breaker (its hardware key: 0x%08X)\n", InstanceId, status);
        return STATUS_SUCCESS;
    }

    // Absent, or not a DWORD: none counted.
    if (!NT_SUCCESS(WdfRegistryQueryULong(key, &g_NfStartAttemptsValue, &attempts)))
    {
        attempts = 0;
    }
    if (attempts >= NF_CABLE_START_ATTEMPTS_LIMIT)
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_ERROR_LEVEL,
                   // Under DbgPrint's 512 bytes with both instance IDs at their longest (about 80 characters).
                   "AudioCodec: %ws refused by the crash breaker (0x%08X): its last start never completed (a crash in start?), "
                   "NfStartAttempts %lu. To clear: the setup tool's add or update, or delete NfStartAttempts under "
                   "HKLM\\SYSTEM\\CurrentControlSet\\Enum\\%ws\\Device Parameters; then disable and enable it.\n",
                   InstanceId, NF_STATUS_START_BREAKER, attempts, InstanceId);
        NfBreakerReportRefusal(Driver, Label, attempts);
        WdfRegistryClose(key);
        return NF_STATUS_START_BREAKER;
    }

    status = NfBreakerStore(key, attempts + 1);
    if (!NT_SUCCESS(status))
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_WARNING_LEVEL,
                   "AudioCodec: %ws: its start count may not be on disk (0x%08X): the crash breaker may not see this start\n",
                   InstanceId, status);
    }

    // Counted (or tried): cleared once the start completes or the binding ends cleanly.
    *Key = key;
    return STATUS_SUCCESS;
}

// Closes AddDevice's key, clearing the count first if Clear (AddDevice failed before the framework device existed).
PAGED_CODE_SEG
static
VOID
NfBreakerClose(
    _In_opt_    WDFKEY  Key,
    _In_        BOOLEAN Clear
)
{
    PAGED_CODE();

    if (Key != NULL)
    {
        if (Clear)
        {
            (void)NfBreakerStore(Key, 0);
        }
        WdfRegistryClose(Key);
    }
}

// Clears the device's count, once: its start completed, or its binding ended without a crash. PASSIVE_LEVEL.
PAGED_CODE_SEG
static
VOID
NfBreakerClear(
    _In_ WDFDEVICE              Device,
    _In_ PCODEC_DEVICE_CONTEXT  DevCtx
)
{
    WDFKEY      key;
    NTSTATUS    status;

    PAGED_CODE();

    if (!DevCtx->StartCounted)
    {
        return;
    }
    DevCtx->StartCounted = FALSE;

    status = WdfDeviceOpenRegistryKey(Device, PLUGPLAY_REGKEY_DEVICE, KEY_SET_VALUE, WDF_NO_OBJECT_ATTRIBUTES, &key);
    if (NT_SUCCESS(status))
    {
        status = NfBreakerStore(key, 0);
        WdfRegistryClose(key);
    }
    if (!NT_SUCCESS(status))
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_WARNING_LEVEL, "AudioCodec: cable %ws's start count not cleared (0x%08X)\n",
                   DevCtx->Cable.Label, status);
    }
}

PAGED_CODE_SEG
NTSTATUS
Codec_EvtDeviceSelfManagedIoInit(
    _In_ WDFDEVICE Device
)
{
    PAGED_CODE();

    // noise-filter (TODO 19, the boot crash breaker): the start has completed. Called once, after the first D0 entry.
    NfBreakerClear(Device, GetCodecDeviceContext(Device));

    return STATUS_SUCCESS;
}

PAGED_CODE_SEG
NTSTATUS
Codec_EvtBusDeviceAdd(
    _In_    WDFDRIVER        Driver,
    _Inout_ PWDFDEVICE_INIT  DeviceInit
)
/*++

Routine Description:

    EvtDeviceAdd is called by the framework in response to AddDevice
    call from the PnP manager. We create and initialize a device object to
    represent a new instance of the device. All the software resources
    should be allocated in this callback.

Arguments:
    Driver - Handle to a framework driver object created in DriverEntry

    DeviceInit - Pointer to a framework-allocated WDFDEVICE_INIT structure.

Return Value:

    NTSTATUS

--*/
{
    NTSTATUS                            status = STATUS_SUCCESS;
    WDF_OBJECT_ATTRIBUTES               attributes;
    WDF_DEVICE_PNP_CAPABILITIES         pnpCaps;
    ACX_DEVICEINIT_CONFIG               devInitCfg;
    ACX_DEVICE_CONFIG                   devCfg;
    WDFDEVICE                           device = nullptr;
    PCODEC_DEVICE_CONTEXT               devCtx;
    WDF_PNPPOWER_EVENT_CALLBACKS        pnpPowerCallbacks;
    WCHAR                               instanceId[NF_CABLE_INSTANCE_ID_CHARS];
    size_t                              instanceChars = 0;
    GUID                                cableId;
    WCHAR                               label[NF_CABLE_LABEL_MAX_CHARS + 1];
    ULONG                               instanceNumber = 0;
    PNF_CABLE                           cable;
    LARGE_INTEGER                       now;
    WDFKEY                              breakerKey = NULL;
    BOOLEAN                             startCounted = FALSE;

    PAGED_CODE();

    //
    // noise-filter (TODO 19, the per-cable design, section 4): the cable's identity and label, from its instance ID, before
    // anything is made. Every device of ours is a cable, and every cable has one: a node without one (not made by the setup
    // tool) is refused with an error status (STATUS_DEVICE_CONFIGURATION_ERROR: Device Manager shows code 31, CM_PROB_FAILED_ADD,
    // with the status in DEVPKEY_Device_ProblemStatus), so discovery, the control device's name and the setup never meet an
    // unnamed cable. A query that fails (the property, then the hardware key's name: NfQueryInstanceId) returns its own status,
    // for the same property to tell.
    //
    status = NfQueryInstanceId(DeviceInit, instanceId, &instanceChars);
    if (!NT_SUCCESS(status))
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_ERROR_LEVEL, "AudioCodec: no instance ID (0x%08X): the device is refused\n", status);
        return status;
    }
    status = NfParseCableInstanceId(instanceId, instanceChars, &cableId, label, &instanceNumber);
    if (!NT_SUCCESS(status))
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_ERROR_LEVEL,
                   "AudioCodec: %ws is not an ACX-LL-Cable instance (ROOT\\MEDIA\\ACXLLCABLE_<label>_<id>_<n>): refused (0x%08X); "
                   "make cables with the setup tool\n", instanceId, status);
        return status;
    }

    // noise-filter (TODO 19, the boot crash breaker, above): refused if its last start never completed, else this attempt
    // counted and on disk (in a session) before anything is made.
    status = NfBreakerEnter(Driver, DeviceInit, instanceId, label, &breakerKey);
    if (!NT_SUCCESS(status))
    {
        return status;
    }
    startCounted = (breakerKey != NULL) ? TRUE : FALSE;

    //
    // The driver calls this DDI in its AddDevice callback before creating the PnP device.
    // ACX uses this call to add default/standard settings for the device to be created.
    //
    ACX_DEVICEINIT_CONFIG_INIT(&devInitCfg);
    status = AcxDeviceInitInitialize(DeviceInit, &devInitCfg);
    if (!NT_SUCCESS(status))
    {
        // noise-filter (TODO 19, the boot crash breaker): a clean failure, with no framework device to clear the count later.
        NfBreakerClose(breakerKey, TRUE);
        RETURN_IF_FAILED(status);
    }

    //
    // Initialize the pnpPowerCallbacks structure.  Callback events for PNP
    // and Power are specified here.  If you don't supply any callbacks,
    // the Framework will take appropriate default actions based on whether
    // DeviceInit is initialized to be an FDO, a PDO or a filter device
    // object.
    //
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnpPowerCallbacks);
    pnpPowerCallbacks.EvtDevicePrepareHardware = Codec_EvtDevicePrepareHardware;
    pnpPowerCallbacks.EvtDeviceReleaseHardware = Codec_EvtDeviceReleaseHardware;
    pnpPowerCallbacks.EvtDeviceD0Entry = Codec_EvtDeviceD0Entry;
    pnpPowerCallbacks.EvtDeviceD0Exit = Codec_EvtDeviceD0Exit;
    // noise-filter (TODO 19, the boot crash breaker): the start has completed.
    pnpPowerCallbacks.EvtDeviceSelfManagedIoInit = Codec_EvtDeviceSelfManagedIoInit;
    WdfDeviceInitSetPnpPowerEventCallbacks(DeviceInit, &pnpPowerCallbacks);

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, CODEC_DEVICE_CONTEXT);
    attributes.EvtCleanupCallback = Codec_EvtDeviceContextCleanup;

    // noise-filter (TODO 19, the boot crash breaker): from here the device's own callbacks clear the count (StartCounted);
    // before, a failure clears it now.
    status = WdfDeviceCreate(&DeviceInit, &attributes, &device);
    NfBreakerClose(breakerKey, NT_SUCCESS(status) ? FALSE : TRUE);
    RETURN_NTSTATUS_IF_FAILED(status);

    //
    // Init Codec's device context.
    //
    devCtx = GetCodecDeviceContext(device);
    ASSERT(devCtx != nullptr);

    devCtx->Render = nullptr;
    devCtx->Capture = nullptr;
    devCtx->Capture2 = nullptr;
    devCtx->ExcludeD3Cold = WdfFalse;
    devCtx->StartCounted = startCounted;

    //
    // noise-filter (TODO 19, the per-cable design, sections 4 and 5): the cable, first, so the context cleanup (which runs if
    // anything below fails) always finds its lock and its loopback ring. The framework zeroed the context: every counter and log
    // of this binding starts at zero, and the attachment count from 1 at the first ATTACH.
    //
    cable = &devCtx->Cable;
    RtlZeroMemory(&devCtx->Loopback, sizeof(devCtx->Loopback));
    KeInitializeSpinLock(&devCtx->Loopback.Lock);
    KeInitializeSpinLock(&cable->Ring.Lock);
    cable->Ring.Loopback = &devCtx->Loopback;
    cable->ControlStatus = STATUS_DEVICE_NOT_READY;

    cable->Id = cableId;
    cable->InstanceNumber = instanceNumber;
    RETURN_NTSTATUS_IF_FAILED(RtlStringCchCopyW(cable->Label, ARRAYSIZE(cable->Label), label));
    RETURN_NTSTATUS_IF_FAILED(RtlStringCchCopyW(cable->InstanceId, ARRAYSIZE(cable->InstanceId), instanceId));
    RETURN_NTSTATUS_IF_FAILED(RtlStringCchPrintfW(cable->IdText, ARRAYSIZE(cable->IdText),
                                                  L"%08lx-%04hx-%04hx-%02x%02x-%02x%02x%02x%02x%02x%02x",
                                                  cableId.Data1, cableId.Data2, cableId.Data3, cableId.Data4[0], cableId.Data4[1],
                                                  cableId.Data4[2], cableId.Data4[3], cableId.Data4[4], cableId.Data4[5],
                                                  cableId.Data4[6], cableId.Data4[7]));
    // The control device's names (Control.cpp, nf_vmic.h): \Device\AcxLlCable_<id> and \DosDevices\AcxLlCable_<id>.
    RETURN_NTSTATUS_IF_FAILED(RtlStringCchPrintfW(cable->DeviceName, ARRAYSIZE(cable->DeviceName), L"%ws%ws", NF_VMIC_DEVICE_PREFIX,
                                                  cable->IdText));
    RETURN_NTSTATUS_IF_FAILED(RtlStringCchPrintfW(cable->LinkName, ARRAYSIZE(cable->LinkName), L"%ws%ws", NF_VMIC_SYMLINK_PREFIX,
                                                  cable->IdText));
    // The generation: the binding's start, UTC in 100 ns units, unique per binding; readers compare it for equality only.
    KeQuerySystemTimePrecise(&now);
    cable->Generation = (ULONGLONG)now.QuadPart;
    cable->BindQpc = (ULONGLONG)KeQueryPerformanceCounter(NULL).QuadPart;
    NfDeriveComponentId(&cableId, &CODEC_RENDER_COMPONENT_GUID, &cable->RenderComponentId);
    NfDeriveComponentId(&cableId, &CODEC_CAPTURE_COMPONENT_GUID, &cable->CaptureComponentId);
    NfDeriveComponentId(&cableId, &CODEC_CAPTURE2_COMPONENT_GUID, &cable->Capture2ComponentId);

    //
    // noise-filter (TODO 19): the loopback ring, allocated once for the device's life so that no stream allocates at
    // DISPATCH_LEVEL; freed in Codec_EvtDeviceContextCleanup. ExAllocatePool2 zeroes it.
    //
    // Its event logs (Public.h, NF_EVENT_LOG), zeroed above: empty, numbering from 1.
    KeInitializeSpinLock(&devCtx->Loopback.SkipLog.Lock);
    devCtx->Loopback.SkipLog.Entries = NF_VMIC_SKIP_LOG_ENTRIES;
    devCtx->Loopback.SkipLog.Slots = devCtx->Loopback.SkipSlots;
    KeInitializeSpinLock(&devCtx->Loopback.UnderrunLog.Lock);
    devCtx->Loopback.UnderrunLog.Entries = NF_VMIC_UNDERRUN_LOG_ENTRIES;
    devCtx->Loopback.UnderrunLog.Slots = devCtx->Loopback.UnderrunSlots;
    devCtx->Loopback.Buffer = (PUCHAR)ExAllocatePool2(POOL_FLAG_NON_PAGED, LOOPBACK_RING_BYTES, DeviceDriverTag);
    if (devCtx->Loopback.Buffer == nullptr)
    {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    // noise-filter (TODO 19, the per-cable design, section 5): the cable's copy of what its reader delivered, 256 KiB, as the
    // loopback ring is (it was a static of the image's); freed in the context cleanup.
    cable->Ring.History = (PUCHAR)ExAllocatePool2(POOL_FLAG_NON_PAGED, NF_APP_HISTORY_BYTES, DeviceDriverTag);
    if (cable->Ring.History == nullptr)
    {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    // noise-filter (TODO 19, the per-cable design, section 8): the endpoints' adapter name, "ACX-LL-Cable D". Best effort.
    NfAssignFriendlyName(device, cable);

    //
    // noise-filter (TODO 19, spike check 4; the per-cable design, section 3): the cable's own control device,
    // \Device\AcxLlCable_<id>, which its context cleanup deletes. A failure there costs this cable's feed only (a name collision:
    // another node carries the identity); it is reported (ControlStatus) and the cable plays on as a plain cable.
    //
    (void)NfVmic_CableAdd(Driver, cable);

    //
    // The driver calls this DDI in its AddDevice callback after creating the PnP
    // device. ACX uses this call to apply any post device settings.
    //
    ACX_DEVICE_CONFIG_INIT(&devCfg);
    RETURN_NTSTATUS_IF_FAILED(AcxDeviceInitialize(device, &devCfg));

    //
    // Tell the framework to set the SurpriseRemovalOK in the DeviceCaps so
    // that you don't get the popup in usermode (on Win2K) when you surprise
    // remove the device.
    //
    WDF_DEVICE_PNP_CAPABILITIES_INIT(&pnpCaps);
    pnpCaps.SurpriseRemovalOK = WdfTrue;
    WdfDeviceSetPnpCapabilities(device, &pnpCaps);

    //
    // Create a render circuit and capture circuit and add them to the current
    // device context. These circuits will be added to the device when the
    // prepare hardware callback is called. 
    //
    // noise-filter (TODO 19, the per-cable design, section 8): each circuit's component ID is the cable's own (NfDeriveComponentId).
    RETURN_NTSTATUS_IF_FAILED(CodecR_AddStaticRender(device, &cable->RenderComponentId, &renderCircuitName));

    // noise-filter (TODO 19): the microphones this hardware ID asks for (NfReadMicrophones), the same rule for every cable; the
    // call one is a second capture circuit (DriverSettings.h). The first made goes in devCtx->Capture, a second in
    // devCtx->Capture2.
    cable->Microphones = NfReadMicrophones(device);
    if (cable->Microphones & NF_MIC_LOW_LATENCY)
    {
        RETURN_NTSTATUS_IF_FAILED(CodecC_AddStaticCapture(device, &cable->CaptureComponentId, &MIC_CUSTOM_NAME, &captureCircuitName,
                                                          L"Microphone"));
    }
    if (cable->Microphones & NF_MIC_CALL)
    {
        RETURN_NTSTATUS_IF_FAILED(CodecC_AddStaticCapture(device, &cable->Capture2ComponentId, &MIC2_CUSTOM_NAME, &capture2CircuitName,
                                                          L"Call Microphone"));
    }

    DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_INFO_LEVEL, "AudioCodec: cable %ws added (instance %lu, microphones %lu, feed 0x%08X)\n",
               cable->Label, cable->InstanceNumber, cable->Microphones, cable->ControlStatus);

    return status;
}

PAGED_CODE_SEG
NTSTATUS
Codec_EvtDevicePrepareHardware(
    _In_ WDFDEVICE      Device,
    _In_ WDFCMRESLIST   ResourceList,
    _In_ WDFCMRESLIST   ResourceListTranslated
)
/*++

Routine Description:

    In this callback, the driver does whatever is necessary to make the
    hardware ready to use.

Arguments:

    Device - handle to a device

Return Value:

    NT status value

--*/
{
    NTSTATUS                status = STATUS_SUCCESS;
    PCODEC_DEVICE_CONTEXT   devCtx;

    UNREFERENCED_PARAMETER(ResourceList);
    UNREFERENCED_PARAMETER(ResourceListTranslated);

    PAGED_CODE();

    devCtx = GetCodecDeviceContext(Device);
    ASSERT(devCtx != nullptr);

    // NOTE: Download firmware here.

    // NOTE: Register streaming h/w resources here.

    //
    // Set power policy data.
    //
    RETURN_NTSTATUS_IF_FAILED(Codec_SetPowerPolicy(Device));

    // Both paths copy in memory. The sample's unused, driver-global WAV file work items cannot belong to two PnP devices.

    //
    // The driver uses this DDI to associate a circuit to a device. After
    // this call the circuit is not visible until the device goes in D0.
    // For a real driver there should be a check here to make sure the
    // circuit has not been added already (there could be a situation where
    // prepareHardware is called multiple times and releaseHardware is only
    // called once). 
    //

    ASSERT(devCtx->Render);
    RETURN_NTSTATUS_IF_FAILED(AcxDeviceAddCircuit(Device, devCtx->Render));

    //
    // noise-filter (TODO 19, check 2): publish the packet-size constraints on each circuit's KSCATEGORY_AUDIO
    // interface here. The circuit's symbolic link is valid once the circuit is added (single-circuit endpoint), and the
    // circuit is not visible until D0, so the property is in place before the interface is enabled.
    //
    // A failure costs only the small periods (the engine stays at its 10 ms default), so it is reported and the start
    // goes on; failing here would also leave the capture circuit unadded for EvtDeviceReleaseHardware to remove.
    //
    NTSTATUS publish = PublishPacketSizeConstraints(devCtx->Render);
    if (!NT_SUCCESS(publish))
    {
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_WARNING_LEVEL, "AudioCodec: render packet-size constraints not set (0x%08X)\n", publish);
    }

    // The microphones this variant made (NfReadMicrophones): one or two capture circuits. The call microphone takes the same
    // constraints (its INF's default period replaces their range there; TODO 22).
    ASSERT(devCtx->Capture != nullptr);
    if (devCtx->Capture != nullptr)
    {
        RETURN_NTSTATUS_IF_FAILED(AcxDeviceAddCircuit(Device, devCtx->Capture));
        publish = PublishPacketSizeConstraints(devCtx->Capture);
        if (!NT_SUCCESS(publish))
        {
            DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_WARNING_LEVEL, "AudioCodec: capture packet-size constraints not set (0x%08X)\n", publish);
        }
    }
    if (devCtx->Capture2 != nullptr)
    {
        RETURN_NTSTATUS_IF_FAILED(AcxDeviceAddCircuit(Device, devCtx->Capture2));
        publish = PublishPacketSizeConstraints(devCtx->Capture2);
        if (!NT_SUCCESS(publish))
        {
            DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_WARNING_LEVEL, "AudioCodec: second microphone packet-size constraints not set (0x%08X)\n",
                       publish);
        }
    }

    return status;
}

PAGED_CODE_SEG
NTSTATUS
Codec_EvtDeviceReleaseHardware(
    _In_ WDFDEVICE      Device,
    _In_ WDFCMRESLIST   ResourceListTranslated
)
/*++

Routine Description:

    In this callback, the driver releases the h/w resources allocated in the
    prepare h/w callback.

Arguments:

    Device - handle to a device

Return Value:

    NT status value

--*/
{
    NTSTATUS                status;
    PCODEC_DEVICE_CONTEXT   devCtx;

    UNREFERENCED_PARAMETER(Device);
    UNREFERENCED_PARAMETER(ResourceListTranslated);

    PAGED_CODE();

    devCtx = GetCodecDeviceContext(Device);
    ASSERT(devCtx != nullptr);

    //
    // The driver uses this DDI to delete a circuit from the current device. 
    //
    RETURN_NTSTATUS_IF_FAILED(AcxDeviceRemoveCircuit(Device, devCtx->Render));
    if (devCtx->Capture != nullptr)
    {
        RETURN_NTSTATUS_IF_FAILED(AcxDeviceRemoveCircuit(Device, devCtx->Capture));
    }
    if (devCtx->Capture2 != nullptr)
    {
        RETURN_NTSTATUS_IF_FAILED(AcxDeviceRemoveCircuit(Device, devCtx->Capture2));
    }

    // NOTE: Release streaming h/w resources here.

    status = STATUS_SUCCESS;

    return status;
}

PAGED_CODE_SEG
NTSTATUS
Codec_EvtDeviceD0Entry(
    _In_  WDFDEVICE              Device,
    _In_  WDF_POWER_DEVICE_STATE PreviousState
)
{
    UNREFERENCED_PARAMETER(Device);
    UNREFERENCED_PARAMETER(PreviousState);

    PAGED_CODE();

    return STATUS_SUCCESS;
}

NTSTATUS
Codec_EvtDeviceD0Exit(
    _In_  WDFDEVICE              Device,
    _In_  WDF_POWER_DEVICE_STATE TargetState
)
{
    NTSTATUS        status = STATUS_SUCCESS;
    POWER_ACTION    powerAction;

    PAGED_CODE();

    powerAction = WdfDeviceGetSystemPowerAction(Device);

    // 
    // Update the power policy D3-cold info for Connected Standby.
    //
    if (TargetState == WdfPowerDeviceD3 && powerAction == PowerActionNone)
    {
        PCODEC_DEVICE_CONTEXT   devCtx;
        WDF_TRI_STATE           excludeD3Cold = WdfTrue;
        ACX_DX_EXIT_LATENCY     latency;

        devCtx = GetCodecDeviceContext(Device);
        ASSERT(devCtx != nullptr);

        //
        // Get the current exit latency.
        //
        latency = AcxDeviceGetCurrentDxExitLatency(Device,
            WdfDeviceGetSystemPowerAction(Device),
            TargetState);

        //
        // If the current exit latency for the ACX device is responsive
        // (not instant or fast) then D3-cold does not need to be excluded.
        // Otherwise, D3-cold should be excluded because if the hardware
        // goes into this state it will take too long to go back into D0
        // and respond. 
        //
        if (latency == AcxDxExitLatencyResponsive)
        {
            excludeD3Cold = WdfFalse;
        }

        if (devCtx->ExcludeD3Cold != excludeD3Cold)
        {
            devCtx->ExcludeD3Cold = excludeD3Cold;

            RETURN_NTSTATUS_IF_FAILED(Codec_SetPowerPolicy(Device));
        }
    }

    return status;
}

PAGED_CODE_SEG
NTSTATUS
Codec_SetPowerPolicy(
    _In_ WDFDEVICE      Device
)
{
    NTSTATUS                status = STATUS_SUCCESS;
    PCODEC_DEVICE_CONTEXT   devCtx;

    PAGED_CODE();

    devCtx = GetCodecDeviceContext(Device);
    ASSERT(devCtx != nullptr);

    //
    // Init the idle policy structure.
    //
    WDF_DEVICE_POWER_POLICY_IDLE_SETTINGS idleSettings;
    WDF_DEVICE_POWER_POLICY_IDLE_SETTINGS_INIT(&idleSettings, IdleCannotWakeFromS0);
    idleSettings.IdleTimeout = IDLE_POWER_TIMEOUT;
    idleSettings.IdleTimeoutType = SystemManagedIdleTimeoutWithHint;
    idleSettings.ExcludeD3Cold = devCtx->ExcludeD3Cold;

    RETURN_NTSTATUS_IF_FAILED(WdfDeviceAssignS0IdleSettings(Device, &idleSettings));

    return status;
}

VOID
Codec_EvtDeviceContextCleanup(
    _In_ WDFOBJECT      WdfDevice
)
/*++

Routine Description:

    In this callback, it cleans up device context.

Arguments:

    WdfDevice - WDF device object

Return Value:

    nullptr

--*/
{
    WDFDEVICE               device;
    PCODEC_DEVICE_CONTEXT   devCtx;

    device = (WDFDEVICE)WdfDevice;
    devCtx = GetCodecDeviceContext(device);
    ASSERT(devCtx != nullptr);

    //
    // noise-filter (TODO 19, spike check 4; the per-cable design, section 3): first, while the cable's ring and the loopback
    // ring that STATS reads from are still here: end the cable's app ring and delete its control device, in the design's order
    // (NfVmic_CableCleanup), whatever Codec_EvtBusDeviceAdd got to. PASSIVE_LEVEL (a device's cleanup callback). Then the
    // cable's copy: no ring can be published any more, so no stream writes or reads it.
    //
    NfVmic_CableCleanup(&devCtx->Cable);

    if (devCtx->Cable.Ring.History != nullptr)
    {
        KIRQL   oldIrql;
        PUCHAR  history;

        KeAcquireSpinLock(&devCtx->Cable.Ring.Lock, &oldIrql);
        history = devCtx->Cable.Ring.History;
        devCtx->Cable.Ring.History = nullptr;
        KeReleaseSpinLock(&devCtx->Cable.Ring.Lock, oldIrql);

        ExFreePoolWithTag(history, DeviceDriverTag);
    }

    //
    // noise-filter (TODO 19): free the loopback ring. The streams are gone by now (inference: they descend from the device
    // and are cleaned up first, and each stopped its timer in ReleaseHardware); the DPC paths check Buffer under the lock.
    //
    if (devCtx->Loopback.Buffer != nullptr)
    {
        KIRQL   oldIrql;
        PUCHAR  buffer;

        KeAcquireSpinLock(&devCtx->Loopback.Lock, &oldIrql);
        buffer = devCtx->Loopback.Buffer;
        devCtx->Loopback.Buffer = nullptr;
        devCtx->Loopback.Owner = nullptr;
        devCtx->Loopback.RenderActive = FALSE;
        devCtx->Loopback.RenderFormat.Valid = FALSE;
        KeReleaseSpinLock(&devCtx->Loopback.Lock, oldIrql);

        ExFreePoolWithTag(buffer, DeviceDriverTag);
    }

    //
    // noise-filter (TODO 19, spike check 6): the sample called CodecC_CircuitCleanup(devCtx->Capture) here, which only looked
    // up the circuit's context. The circuit is a child of the device, so the framework has cleaned it up before this callback
    // runs and its handle is stale: under Driver Verifier the lookup faulted on device removal (bugcheck 0x7E in
    // Wdf01000!FxIFR, from WdfObjectGetTypedContext, 2026-11-22). Nothing is lost without it.
    //

    // noise-filter (TODO 19, the boot crash breaker): the binding ended without a crash (a failed AddDevice or start, or a
    // removal before the start completed): its count goes. PASSIVE_LEVEL (a device's cleanup callback).
    NfBreakerClear(device, devCtx);
}
