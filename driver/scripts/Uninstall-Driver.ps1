<#
.SYNOPSIS
    Removes every cable (and the old driver's devices) and our driver packages from the driver store.

.DESCRIPTION
    TODO 19, the per-cable design, section 9: acx-ll-cable remove --all --forget (every device of ours, the old driver's
    hardware IDs included), then each package whose INF was AudioCodec.inf deleted from the driver store (the old driver's
    and the per-cable driver's share the name). -KeepIdentities keeps the setup's store, so cables added again keep their
    identities and endpoints. -BuildTool builds the setup tool first.

    Exit codes: 0 done; 7 Windows wants a restart to finish (a device's removal, or a package's deletion: stop there,
    restart, then run this again); 2 the setup tool's error, or a package pnputil couldn't delete; 1 this script's own check
    (not elevated, no tool).

.EXAMPLE
    .\Uninstall-Driver.ps1
.EXAMPLE
    .\Uninstall-Driver.ps1 -KeepIdentities
#>
param([switch]$KeepIdentities, [switch]$BuildTool)

. (Join-Path $PSScriptRoot 'common.ps1')
Assert-Admin
$tool = Get-CableTool -Build:$BuildTool

Write-Step 'Remove every device of ours (acx-ll-cable remove --all)'
$arguments = @('remove', '--all')
if (-not $KeepIdentities) { $arguments += '--forget' }
# A removal Windows finishes at the next restart stops here: the packages are deleted on the run after it
Exit-ForToolCode (Invoke-CableTool $tool $arguments) 'removing the devices (the packages are deleted by the run after it)'

Write-Step "Delete $InfName from the driver store"
$published = @(Get-WindowsDriver -Online | Where-Object { (Split-Path -Leaf $_.OriginalFileName) -eq $InfName.ToLower() -or
        (Split-Path -Leaf $_.OriginalFileName) -eq $InfName })
if ($published.Count -eq 0) { Write-Ok 'none there' }
$failed = @()
$restart = @()
foreach ($d in $published) {
    pnputil /delete-driver $d.Driver /uninstall /force
    if ($LASTEXITCODE -eq 0) { Write-Ok "deleted $($d.Driver)" }
    elseif ($LASTEXITCODE -eq $RebootRequired) { Write-Note "deleted $($d.Driver); Windows wants a restart to finish"; $restart += $d.Driver }
    else { Write-Note "pnputil couldn't delete $($d.Driver) ($LASTEXITCODE)"; $failed += $d.Driver }
}
if ($failed.Count -gt 0) {
    Write-Host "   not deleted: $($failed -join ', '): run this again, or pnputil /delete-driver <oemNN.inf> /uninstall /force" -ForegroundColor Red
    exit 2
}
if ($restart.Count -gt 0) {
    Write-Note "Windows wants a restart to finish deleting $($restart -join ', '): restart, then run this again. Exit 7."
    exit 7
}
exit 0
