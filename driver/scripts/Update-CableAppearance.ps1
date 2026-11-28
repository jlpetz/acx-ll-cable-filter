<#
.SYNOPSIS
    Applies the current Windows Sound labels and icons to one cable upgraded from the old Speakers/Microphone names.
.DESCRIPTION
    Windows can carry old endpoint appearance settings across a driver update. Run this optional helper from elevated
    PowerShell after updating. It compares each name and icon separately with the old factory value, so a custom name
    or icon is preserved. Windows cannot distinguish an explicitly chosen stock value from the same factory value.
    Run it before deleting an older LLCable package to move references to its factory icons to the current package.
    If the previous package is gone or its ownership cannot be verified, the helper leaves that icon alone.
    Uses the undocumented Windows Sound IPolicyConfig property interface. No cable is recreated. A partial run can be retried.
.EXAMPLE
    .\Update-CableAppearance.ps1 -Label B -WhatIf
.EXAMPLE
    .\Update-CableAppearance.ps1 -Label B
#>
[CmdletBinding(SupportsShouldProcess)]
param([Parameter(Mandatory)][ValidatePattern('^[a-zA-Z0-9]{1,8}$')][string]$Label)

. (Join-Path $PSScriptRoot 'common.ps1')
if (-not $WhatIfPreference) { Assert-Admin }
$tool = Get-CableTool
$Label = $Label.ToUpperInvariant()
$adapter = "ACX-LL-Cable $Label"
$nodes = @(Get-PnpDevice -Class MEDIA -PresentOnly | Where-Object { $_.InstanceId -like "ROOT\MEDIA\ACXLLCABLE_${Label}_*" })
if ($nodes.Count -ne 1) { throw "Expected one installed $adapter, found $($nodes.Count)." }
$node = $nodes[0]
# Windows PowerShell 5.1 propagates -WhatIf into the CIM method behind this read and skips it unless explicitly requested.
$hw = (Get-PnpDeviceProperty -InstanceId $node.InstanceId -KeyName DEVPKEY_Device_HardwareIds -WhatIf:$false).Data
$variants = @($HardwareIds.Keys | Where-Object { $hw -contains $HardwareIds[$_] })
$service = (Get-PnpDeviceProperty -InstanceId $node.InstanceId -KeyName DEVPKEY_Device_Service -WhatIf:$false).Data
if ($variants.Count -ne 1 -or $service -ne $PackageName) { throw "$adapter is not an installed $PackageName cable." }
$image = (Get-ItemProperty -LiteralPath "HKLM:\SYSTEM\CurrentControlSet\Services\$service" -Name ImagePath).ImagePath
if ($image -notlike '\SystemRoot\*') { throw "Unexpected driver service path: $image" }
$sys = (Get-Item -LiteralPath (Join-Path $env:windir $image.Substring('\SystemRoot\'.Length))).FullName
$store = Join-Path $env:windir 'System32\DriverStore\FileRepository'
$currentSignature = Get-AuthenticodeSignature -FilePath $sys
if ($sys -notlike "$store\audiocodec.inf_*\AudioCodec.sys" -or $currentSignature.Status -ne 'Valid') {
    throw "Expected the signed $PackageName driver in the Driver Store."
}

if (-not ('NfSoundAppearance' -as [type])) { Add-Type @'
using System;
using System.Runtime.InteropServices;

[StructLayout(LayoutKind.Sequential)]
public struct NfSoundKey {
    public Guid Format;
    public uint Id;
    public NfSoundKey(string format, uint id) { Format = new Guid(format); Id = id; }
}
[StructLayout(LayoutKind.Explicit, Size=24)]
public struct NfSoundValue {
    [FieldOffset(0)] public ushort Type;
    [FieldOffset(8)] public IntPtr Text;
}
[ComImport, Guid("f8679f50-850a-41cf-9c72-430f290290c8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface INfSoundPolicy {
    void GetMixFormat(); void GetDeviceFormat(); void ResetDeviceFormat(); void SetDeviceFormat(); void GetProcessingPeriod();
    void SetProcessingPeriod(); void GetShareMode(); void SetShareMode();
    void GetPropertyValue([MarshalAs(UnmanagedType.LPWStr)] string id, int fxStore, ref NfSoundKey key, out NfSoundValue value);
    void SetPropertyValue([MarshalAs(UnmanagedType.LPWStr)] string id, int fxStore, ref NfSoundKey key, ref NfSoundValue value);
}
[ComImport, Guid("870af99c-171d-4f9e-af0d-e63df40c2bc9")] class NfSoundPolicyClient { }
public static class NfSoundAppearance {
    static readonly INfSoundPolicy Policy = (INfSoundPolicy)new NfSoundPolicyClient();
    public static readonly NfSoundKey Name = new NfSoundKey("a45c254e-df1c-4efd-8020-67d146a850e0",2);
    public static readonly NfSoundKey Icon = new NfSoundKey("259abffc-50a7-47ce-af08-68c9a7d73366",12);
    [DllImport("ole32.dll")] static extern int PropVariantClear(ref NfSoundValue value);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern uint PrivateExtractIcons(string file, int index, int width,
        int height, out IntPtr icon, out uint resource, uint count, uint flags);
    [DllImport("user32.dll")] static extern bool DestroyIcon(IntPtr icon);
    public static bool HasIcon(string path, int id) {
        IntPtr icon; uint resource;
        uint count = PrivateExtractIcons(path, -id, 32, 32, out icon, out resource, 1, 0);
        if (icon == IntPtr.Zero) return false;
        DestroyIcon(icon);
        return count == 1;
    }
    public static string Read(string id, NfSoundKey key) {
        NfSoundValue value; Policy.GetPropertyValue(id, 0, ref key, out value);
        try {
            if (value.Type == 0) return null;
            if (value.Type != 31) throw new InvalidOperationException("Unexpected Sound property type: " + value.Type);
            return Marshal.PtrToStringUni(value.Text);
        } finally { PropVariantClear(ref value); }
    }
    public static void SetIfUnchanged(string id, NfSoundKey key, string previous, string text) {
        if (Read(id,key) != previous) throw new InvalidOperationException("Sound property changed during the refresh; try again.");
        var value = new NfSoundValue { Type = 31, Text = Marshal.StringToCoTaskMemUni(text) };
        try { Policy.SetPropertyValue(id, 0, ref key, ref value); } finally { PropVariantClear(ref value); }
        if (Read(id,key) != text) throw new InvalidOperationException("Windows did not retain the requested Sound property.");
    }
}
'@ }

function Get-RoleEndpoint([string]$role) {
    $line = @(& $tool endpoint $Label $role)
    if ($LASTEXITCODE -ne 0 -or $line.Count -ne 1 -or $line[0] -notmatch '^(\{[^ ]+\}) active "(.*)"$') {
        throw "$adapter has no active $role endpoint. Enable or repair the cable first."
    }
    [pscustomobject]@{ Id = $Matches[1]; FullName = $Matches[2] }
}

function Test-OldFactoryIcon([string]$Reference, [string]$Current, [string]$StoreRoot, [int]$Resource, [int]$StockResource) {
    if (-not $Reference) { return $false }
    $expanded = [Environment]::ExpandEnvironmentVariables($Reference)
    if ($expanded -eq "$env:windir\system32\mmres.dll,-$StockResource") { return $true }
    if ($expanded -eq "$Current,-$Resource") { return $false }
    # Only an actual previous Driver Store package can establish that the old icon belonged to this product.
    $pathPattern = '^(' + [regex]::Escape($StoreRoot) + '\\audiocodec\.inf_(amd64|arm64)_[0-9a-f]{16}\\AudioCodec\.sys),-' + $Resource + '$'
    if ($expanded -notmatch $pathPattern) { return $false }
    $previous = $Matches[1]
    $inf = Join-Path (Split-Path -Parent $previous) 'AudioCodec.inf'
    if (!(Test-Path -LiteralPath $previous -PathType Leaf) -or !(Test-Path -LiteralPath $inf -PathType Leaf)) {
        Write-Warning "Leaving the old icon in place: its package can no longer be verified ($previous)."
        return $false
    }
    $old = Get-AuthenticodeSignature -FilePath $previous
    if ($old.Status -ne 'Valid' -or $old.SignerCertificate.Thumbprint -ne $currentSignature.SignerCertificate.Thumbprint) {
        Write-Warning "Leaving the old icon in place: its package signer does not match the current driver."
        return $false
    }
    $lines = @(Get-Content -LiteralPath $inf)
    $model = '^\s*%AcxLlCable\.DeviceDesc%\s*=\s*Audio_Device\s*,\s*ROOT\\AcxLlCable\s*$'
    $factory = '^\s*HKR,EP\\0,%PKEY_DeviceClass_IconPath%,,"%13%\\AudioCodec\.sys,-' + $Resource + '"\s*$'
    return [bool](($lines -match $model) -and ($lines -match $factory) -and [NfSoundAppearance]::HasIcon($previous,$Resource))
}

# These are the original circuit reference strings, also used by the setup tool to identify each role. Verify the
# installed interface's provisioning data: an older package can have a valid signature but lack the appearance update.
$iface = "HKLM:\SYSTEM\CurrentControlSet\Control\DeviceClasses\{6994ad04-93ef-11d0-a3cc-00a0c9223196}"
$nodeKey = '##?#' + $node.InstanceId.Replace('\','#') + '#{6994ad04-93ef-11d0-a3cc-00a0c9223196}'
$keyName = '{259abffc-50a7-47ce-af08-68c9a7d73366},12'
$roles = @(,@('speakers','Speaker0','Speakers','In',101,3010))
if ($variants[0] -ne 'call') { $roles += ,@('microphone','Microphone0','Microphone','Out',102,3014) }
if ($variants[0] -ne 'low-latency') { $roles += ,@('call-microphone','Microphone1','Call Microphone','Out-5ms',102,3014) }
$plan = @(foreach ($row in $roles) {
    $role,$pin,$oldName,$suffix,$resource,$oldResource = $row
    $icon = "$sys,-$resource"
    $provisioned = (Get-Item -LiteralPath "$iface\$nodeKey\#$pin\Device Parameters\EP\0").GetValue($keyName)
    if ($provisioned -ne $icon -or -not [NfSoundAppearance]::HasIcon($sys,$resource)) {
        throw "Update the driver first. $adapter/$pin does not supply the expected icon."
    }
    $ep = Get-RoleEndpoint $role
    $beforeName = [NfSoundAppearance]::Read($ep.Id,[NfSoundAppearance]::Name)
    $beforeIcon = [NfSoundAppearance]::Read($ep.Id,[NfSoundAppearance]::Icon)
    if ($beforeName -ceq $oldName -and $ep.FullName -ceq "$oldName ($adapter)") {
        [pscustomobject]@{ Role=$role; Id=$ep.Id; Key='Name'; Before=$beforeName; After="LLCable-$Label $suffix"; Full=$ep.FullName }
    }
    if (Test-OldFactoryIcon $beforeIcon $sys $store $resource $oldResource) {
        [pscustomobject]@{ Role=$role; Id=$ep.Id; Key='Icon'; Before=$beforeIcon; After=$icon; Full=$null }
    }
})
if (-not $plan.Count) { Write-Output "$adapter has no old factory appearance values to update."; return }
foreach ($change in $plan) {
    if (-not $PSCmdlet.ShouldProcess("$adapter/$($change.Role)", "Update $($change.Key) to $($change.After)")) { continue }
    $ep = Get-RoleEndpoint $change.Role
    if ($ep.Id -ne $change.Id -or ($change.Full -and $ep.FullName -cne $change.Full)) { throw 'Endpoint changed. Run again.' }
    $key = if ($change.Key -eq 'Name') { [NfSoundAppearance]::Name } else { [NfSoundAppearance]::Icon }
    [NfSoundAppearance]::SetIfUnchanged($ep.Id,$key,$change.Before,$change.After)
    Write-Output "$adapter/$($change.Role): updated $($change.Key)."
}
