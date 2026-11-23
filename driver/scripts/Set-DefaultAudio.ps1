<#
.SYNOPSIS
    Sets Windows' default playback and recording devices (all three roles), by endpoint id.

.DESCRIPTION
    Installing an audio driver can make its new endpoints the defaults (the spike's AudioCodec install did). This puts them
    back. Windows has no command for it; this uses IPolicyConfig, the undocumented COM interface the Sound control panel
    itself uses (user mode, Windows 7 and later). Ids are IMMDevice ids, as `noise-filter devices` lists them.

.EXAMPLE
    .\Set-DefaultAudio.ps1 -Playback '{0.0.0.00000000}.{01ad0454-a603-4666-96e8-e7f218bc234a}' `
        -Recording '{0.0.1.00000000}.{914e9eee-db76-4727-9b4b-bf757c2222e8}'
#>
param([string]$Playback, [string]$Recording)

$ErrorActionPreference = 'Stop'
if (-not ('NfPolicyConfig' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

[ComImport, Guid("f8679f50-850a-41cf-9c72-430f290290c8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IPolicyConfig
{
    int GetMixFormat(); int GetDeviceFormat(); int ResetDeviceFormat(); int SetDeviceFormat(); int GetProcessingPeriod();
    int SetProcessingPeriod(); int GetShareMode(); int SetShareMode(); int GetPropertyValue(); int SetPropertyValue();
    [PreserveSig] int SetDefaultEndpoint([MarshalAs(UnmanagedType.LPWStr)] string deviceId, int role);
    int SetEndpointVisibility();
}

[ComImport, Guid("870af99c-171d-4f9e-af0d-e63df40c2bc9")]
class CPolicyConfigClient { }

[ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IEndpointEnumerator {
    int EnumAudioEndpoints();
    [PreserveSig] int GetDefaultAudioEndpoint(int flow, int role, out IEndpoint endpoint);
}
[ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IEndpoint {
    int Activate(); int OpenPropertyStore();
    [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
}
[ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
class EndpointEnumerator { }

public static class NfPolicyConfig
{
    public static string GetRole(int flow, int role)
    {
        var enumerator = (IEndpointEnumerator)new EndpointEnumerator();
        IEndpoint endpoint = null;
        try {
            int hr = enumerator.GetDefaultAudioEndpoint(flow, role, out endpoint);
            if (hr == unchecked((int)0x80070490)) return null;
            Marshal.ThrowExceptionForHR(hr);
            string id;
            Marshal.ThrowExceptionForHR(endpoint.GetId(out id));
            return id;
        } finally {
            if (endpoint != null) Marshal.ReleaseComObject(endpoint);
            Marshal.ReleaseComObject(enumerator);
        }
    }
    public static void SetRole(string id, int role)
    {
        var config = (IPolicyConfig)new CPolicyConfigClient();
        try { Marshal.ThrowExceptionForHR(config.SetDefaultEndpoint(id, role)); }
        finally { Marshal.ReleaseComObject(config); }
    }
    // eConsole, eMultimedia, eCommunications.
    public static void SetDefault(string id)
    {
        for (int role = 0; role < 3; role++)
        {
            SetRole(id, role);
        }
    }
}
'@
}
foreach ($id in @($Playback, $Recording) | Where-Object { $_ }) {
    [NfPolicyConfig]::SetDefault($id)
    Write-Host "default (all roles): $id" -ForegroundColor Green
}
