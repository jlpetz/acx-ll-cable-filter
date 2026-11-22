<#
.SYNOPSIS
    Installs the built, test-signed package: a root-enumerated device with our driver on it.

.DESCRIPTION
    A new device node is made with devcon (pnputil can't make root-enumerated ones); one that's there already gets the
    driver updated through pnputil. Needs test signing active (Setup-TestSigning.ps1, then a reboot) and an elevated shell.

    -Variant picks the hardware ID (common.ps1): both microphones (the default), the call one, or the low-latency one. A device
    of another variant is removed first: Windows makes the call microphone's endpoint, with its default period, only then.

.EXAMPLE
    .\Install-Driver.ps1
.EXAMPLE
    .\Install-Driver.ps1 -Variant call
#>
param(
    [ValidateSet('Release', 'Debug')][string]$Configuration = 'Release',
    [ValidateSet('both', 'call', 'low-latency')][string]$Variant = 'both'
)

. (Join-Path $PSScriptRoot 'common.ps1')
Assert-Admin
if (-not (Get-TestSigningActive)) { throw 'Test signing is not active: run Setup-TestSigning.ps1, then reboot.' }
$cert = Get-TestCert
if (-not $cert -or -not (Test-CertTrusted $cert)) { throw 'The test certificate is missing or untrusted: run Setup-TestSigning.ps1.' }

$inf = Join-Path $DriverRoot "out\$Configuration\$PackageName\$InfName"
if (-not (Test-Path $inf)) { throw "No package: run Build-Driver.ps1 first ($inf)." }

$HardwareId = $HardwareIds[$Variant]
foreach ($other in $HardwareIds.Values | Where-Object { $_ -ne $HardwareId }) {
    if (@(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains $other }).Count -gt 0) {
        Write-Step "Remove the other variant's device, $other"
        & (Find-WdkTool 'devcon.exe') remove $other
    }
}
$existing = @(Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains $HardwareId })
if ($existing.Count -gt 0) {
    Write-Step "Update the driver on $HardwareId"
    pnputil /add-driver $inf /install
} else {
    Write-Step "Make $HardwareId ($Variant) and install"
    & (Find-WdkTool 'devcon.exe') install $inf $HardwareId
}
if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 3010) { throw "install failed ($LASTEXITCODE)" }
if ($LASTEXITCODE -eq 3010) { Write-Note 'Windows wants a reboot to finish.' }

& (Join-Path $PSScriptRoot 'Get-DriverStatus.ps1')
