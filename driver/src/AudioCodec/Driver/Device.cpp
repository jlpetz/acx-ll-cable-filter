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
// NfMicrophones (the INF sets it per hardware ID): 1 the low-latency one (Microphone0), 2 the call one (Microphone1), 3 both.
// Absent, 0 or out of range: both.
//
#define NF_MIC_LOW_LATENCY  0x1
#define NF_MIC_CALL         0x2

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
    WCHAR                               hardwareIds[128] = {};
    ULONG                               hardwareBytes;
    DECLARE_CONST_UNICODE_STRING(cableBId, L"ROOT\\NoiseFilterCableB");
    UNICODE_STRING                      firstId;

    PAGED_CODE();

    // Our INF matches exact root hardware IDs. Routing must be known even on first boot or when the optional cable loads first.
    RETURN_NTSTATUS_IF_FAILED(WdfFdoInitQueryProperty(DeviceInit, DevicePropertyHardwareID, sizeof(hardwareIds), hardwareIds, &hardwareBytes));
    if (hardwareBytes < 2 * sizeof(WCHAR) || hardwareBytes > sizeof(hardwareIds) || hardwareIds[hardwareBytes / sizeof(WCHAR) - 1] != 0)
    {
        return STATUS_INVALID_PARAMETER;
    }
    RtlInitUnicodeString(&firstId, hardwareIds);
    const BOOLEAN cableB = RtlEqualUnicodeString(&firstId, &cableBId, TRUE);

    //
    // The driver calls this DDI in its AddDevice callback before creating the PnP device.
    // ACX uses this call to add default/standard settings for the device to be created.
    //
    ACX_DEVICEINIT_CONFIG_INIT(&devInitCfg);
    RETURN_IF_FAILED(AcxDeviceInitInitialize(DeviceInit, &devInitCfg));

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
    WdfDeviceInitSetPnpPowerEventCallbacks(DeviceInit, &pnpPowerCallbacks);

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, CODEC_DEVICE_CONTEXT);
    attributes.EvtCleanupCallback = Codec_EvtDeviceContextCleanup;

    RETURN_NTSTATUS_IF_FAILED(WdfDeviceCreate(&DeviceInit, &attributes, &device));

    //
    // Init Codec's device context.
    //
    devCtx = GetCodecDeviceContext(device);
    ASSERT(devCtx != nullptr);

    devCtx->Render = nullptr;
    devCtx->Capture = nullptr;
    devCtx->Capture2 = nullptr;
    devCtx->ExcludeD3Cold = WdfFalse;
    devCtx->VmicRegistered = FALSE;

    //
    // noise-filter (TODO 19): the loopback ring, allocated once for the device's life so that no stream allocates at
    // DISPATCH_LEVEL; freed in Codec_EvtDeviceContextCleanup. ExAllocatePool2 zeroes it.
    //
    RtlZeroMemory(&devCtx->Loopback, sizeof(devCtx->Loopback));
    KeInitializeSpinLock(&devCtx->Loopback.Lock);
    devCtx->Loopback.Buffer = (PUCHAR)ExAllocatePool2(POOL_FLAG_NON_PAGED, LOOPBACK_RING_BYTES, DeviceDriverTag);
    if (devCtx->Loopback.Buffer == nullptr)
    {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    //
    // noise-filter (TODO 19, spike check 4): register with the app feed: the first device makes its control device
    // (NoiseFilterVmic), and the last one's context cleanup deletes it. A failure there costs only the app feed; it is
    // reported and the device goes on. From here on this device's cleanup must unregister, whatever fails later.
    //
    if (!cableB)
    {
        RETURN_NTSTATUS_IF_FAILED(NfVmic_DeviceAdd(Driver, device, &devCtx->Loopback));
        devCtx->Loopback.AppFeed = TRUE;
        devCtx->VmicRegistered = TRUE;
    }

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
    RETURN_NTSTATUS_IF_FAILED(CodecR_AddStaticRender(device, cableB ? &CABLE_B_RENDER_COMPONENT_GUID : &CODEC_RENDER_COMPONENT_GUID,
                                                    &renderCircuitName));

    // noise-filter (TODO 19): the microphones this hardware ID asks for (NfReadMicrophones); the call one is a second capture
    // circuit (DriverSettings.h). The first made goes in devCtx->Capture, a second in devCtx->Capture2.
    ULONG microphones = cableB ? NF_MIC_LOW_LATENCY : NfReadMicrophones(device);
    if (microphones & NF_MIC_LOW_LATENCY)
    {
        RETURN_NTSTATUS_IF_FAILED(CodecC_AddStaticCapture(device, cableB ? &CABLE_B_CAPTURE_COMPONENT_GUID : &CODEC_CAPTURE_COMPONENT_GUID,
                                                          &MIC_CUSTOM_NAME, &captureCircuitName, L"Microphone"));
    }
    if (microphones & NF_MIC_CALL)
    {
        RETURN_NTSTATUS_IF_FAILED(CodecC_AddStaticCapture(device, &CODEC_CAPTURE2_COMPONENT_GUID, &MIC2_CUSTOM_NAME, &capture2CircuitName,
                                                          L"Call Microphone"));
    }

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
    // noise-filter (TODO 19, spike check 4): first, while the loopback ring that STATS reads from is still here: stop STATS
    // reading it and, with the last device, end any app ring and delete the control device. Learn: a driver with both kinds
    // deletes its control device itself, at PASSIVE_LEVEL, after the framework has deleted its PnP devices, which this
    // callback is how it learns (wdf/using-control-device-objects).
    //
    if (devCtx->VmicRegistered)
    {
        NfVmic_DeviceCleanup(device);
        devCtx->VmicRegistered = FALSE;
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
}
