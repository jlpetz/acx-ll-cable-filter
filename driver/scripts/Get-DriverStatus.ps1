<#
.SYNOPSIS
    Where the test-signing set-up and the driver stand: test signing, the certificate, the cables, their endpoints.

.DESCRIPTION
    The cables come from the setup tool's list (acx-ll-cable list: any number of cables, the old driver's devices, the
    remembered absent ones and setup errors). Without the tool built, a short list from Get-PnpDevice stands in.

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

Write-Step "Cables (variants: $(($HardwareIds.GetEnumerator() | ForEach-Object { "$($_.Key) $($_.Value)" }) -join ', '))"
$tool = $null
try { $tool = Get-CableTool } catch { Write-Note "$($_.Exception.Message) A short list instead:" }
if ($tool) {
    $null = Invoke-CableTool $tool @('list')
} else {
    $ids = @($HardwareIds.Values) + $LegacyHardwareIds
    $devices = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object { $d = $_; @($ids | Where-Object { $d.HardwareID -contains $_ }).Count -gt 0 })
    if ($devices.Count -eq 0) { Write-Note 'none present' }
    foreach ($d in $devices) {
        $problem = (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName 'DEVPKEY_Device_ProblemCode' -ErrorAction SilentlyContinue).Data
        Write-Host "   $($d.InstanceId): $($d.Status)$(if ($problem) { ", problem code $problem" }) ($($d.HardwareID -join ', '))"
    }
}
