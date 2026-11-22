<#
.SYNOPSIS
    Where the test-signing set-up and the driver stand: test signing, the certificate, the device, its endpoints.

.EXAMPLE
    .\Get-DriverStatus.ps1
#>
. (Join-Path $PSScriptRoot 'common.ps1')

Write-Step 'Test signing'
Write-Host "   boot entry: $(if (Get-TestSigning) { 'on' } else { 'off' }); running kernel: $(if (Get-TestSigningActive) { 'on' } else { 'off' })"

Write-Step 'Test certificate'
$cert = Get-TestCert
if ($cert) {
    Write-Host "   $($cert.Thumbprint), until $($cert.NotAfter.ToString('yyyy-MM-dd')); trusted: $(Test-CertTrusted $cert)"
} else {
    Write-Note 'none (Setup-TestSigning.ps1 makes it)'
}

Write-Step "Device (variants: $(($HardwareIds.GetEnumerator() | ForEach-Object { "$($_.Key) $($_.Value)" }) -join ', '))"
$devices = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $d = $_; @($HardwareIds.Values | Where-Object { $d.HardwareID -contains $_ }).Count -gt 0 })
if ($devices.Count -eq 0) { Write-Note 'not installed' }
foreach ($d in $devices) {
    $problem = (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_ProblemCode' -ErrorAction SilentlyContinue).Data
    $variant = ($HardwareIds.GetEnumerator() | Where-Object { $d.HardwareID -contains $_.Value } | Select-Object -First 1).Key
    Write-Host "   $($d.InstanceId): $($d.Status)$(if ($problem) { ", problem code $problem" }) (variant: $variant)"
    $children = @(Get-PnpDevice -Class AudioEndpoint -ErrorAction SilentlyContinue | Where-Object {
            (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_Parent' -ErrorAction SilentlyContinue).Data -eq $d.InstanceId -or
            $_.FriendlyName -match [regex]::Escape($PackageName) })
    foreach ($c in $children) { Write-Host "     endpoint: $($c.FriendlyName) ($($c.Status))" }
}
