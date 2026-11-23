<#
.SYNOPSIS
    List, add or remove the independent Noise Filter Cable B playback/recording pair.
.DESCRIPTION
    Add uses the installed primary cable's package and requires the WDK's devcon on this development checkout.
    Cable B works as a plain cable with no app. Choose its Speakers as the app's WASAPI output to filter into it.
#>
param([ValidateSet('list', 'add', 'remove')][string]$Action = 'list')
. (Join-Path $PSScriptRoot 'common.ps1')
$cableHardware = $CableBHardwareId
function Find-CableB {
    @(Get-PnpDevice -Class Media -PresentOnly | Where-Object { $_.HardwareID -contains $cableHardware })
}
function Get-CableBEndpoints {
    @(Get-PnpDevice -Class AudioEndpoint -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.FriendlyName -like '*Noise Filter Cable B*' })
}
function Show-CableB {
    $present = @(Find-CableB)
    if ($present.Count -eq 0) { Write-Host 'Cable B is absent.'; return }
    $present | Select-Object Status, FriendlyName, InstanceId
    Get-CableBEndpoints | Select-Object Status, FriendlyName
}
if ($Action -eq 'list') { Show-CableB; return }

Assert-Admin
# Serialize concurrent setup processes. The protected directory prevents a standard user from taking the setup lock.
$lockPath = Join-Path $env:SystemRoot 'System32\config\NoiseFilter-CableB.lock'
$lock = [IO.FileStream]::new($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None, 1,
    [IO.FileOptions]::DeleteOnClose)
try {
    $existing = @(Find-CableB)
    if ($Action -eq 'remove') {
        foreach ($dev in $existing) {
            pnputil /remove-device $dev.InstanceId
            if ($LASTEXITCODE -ne 0) { throw "Cable B removal returned $LASTEXITCODE." }
        }
        Show-CableB
        return
    }
    if ($existing.Count -ne 0) {
        if ($existing.Count -ne 1 -or $existing[0].Status -ne 'OK' -or @(Get-CableBEndpoints).Count -ne 2) {
            throw 'Cable B is present but incomplete. Remove it, then add it again.'
        }
        Show-CableB
        return
    }
    $primary = @(Get-PnpDevice -PresentOnly -Class Media | Where-Object {
        $dev = $_; @($HardwareIds.Values | Where-Object { $dev.HardwareID -contains $_ }).Count -gt 0
    })
    if ($primary.Count -ne 1 -or $primary[0].Status -ne 'OK') { throw 'Install and start the primary cable first.' }
    $published = (Get-PnpDeviceProperty -InstanceId $primary[0].InstanceId -KeyName DEVPKEY_Device_DriverInfPath).Data
    $inf = Join-Path "$env:SystemRoot\INF" $published
    if (-not (Select-String -LiteralPath $inf -Pattern $cableHardware -SimpleMatch -Quiet)) {
        throw 'Update the primary cable to a driver package that offers Cable B first.'
    }
    . (Join-Path $PSScriptRoot 'Set-DefaultAudio.ps1')
    $defaults = @{}
    foreach ($flow in 0, 1) { foreach ($role in 0..2) { $defaults["$flow/$role"] = [NfPolicyConfig]::GetRole($flow, $role) } }
    try {
        & (Find-WdkTool 'devcon.exe') install $inf $cableHardware
        if ($LASTEXITCODE -ne 0) { throw "Cable B setup returned $LASTEXITCODE." }
        $until = [datetime]::UtcNow.AddSeconds(20)
        do {
            $endpoints = @(Get-CableBEndpoints)
            if ($endpoints.Count -eq 2 -and @($endpoints | Where-Object Status -ne 'OK').Count -eq 0) { break }
            Start-Sleep -Milliseconds 200
        } while ([datetime]::UtcNow -lt $until)
        if ($endpoints.Count -ne 2 -or @($endpoints | Where-Object Status -ne 'OK').Count -ne 0) {
            throw 'The Cable B speaker and microphone did not both start.'
        }
    } finally {
        # Windows may promote a newly installed endpoint. Restore only roles moved to B, preserving independent role choices.
        $ids = @(Get-CableBEndpoints | ForEach-Object { $_.InstanceId -replace '^SWD\\MMDEVAPI\\', '' })
        foreach ($flow in 0, 1) { foreach ($role in 0..2) {
            $old = $defaults["$flow/$role"]
            $now = [NfPolicyConfig]::GetRole($flow, $role)
            if ($old -and $now -ne $old -and $ids -contains $now) { [NfPolicyConfig]::SetRole($old, $role) }
        } }
    }
    Show-CableB
} finally {
    $lock.Dispose()
}
