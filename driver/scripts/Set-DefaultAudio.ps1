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

public static class NfPolicyConfig
{
    // eConsole, eMultimedia, eCommunications.
    public static void SetDefault(string id)
    {
        var config = (IPolicyConfig)new CPolicyConfigClient();
        for (int role = 0; role < 3; role++)
        {
            int hr = config.SetDefaultEndpoint(id, role);
            if (hr != 0) { Marshal.ThrowExceptionForHR(hr); }
        }
    }
}
'@
}
foreach ($id in @($Playback, $Recording) | Where-Object { $_ }) {
    [NfPolicyConfig]::SetDefault($id)
    Write-Host "default (all roles): $id" -ForegroundColor Green
}
