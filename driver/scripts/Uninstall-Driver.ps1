<#
.SYNOPSIS
    Removes every cable (and the old driver's devices) and our driver packages from the driver store.

.DESCRIPTION
    TODO 19, the per-cable design, section 9: acx-ll-cable remove --all --forget (every device of ours, the old driver's
    hardware IDs included), then each package whose INF was AudioCodec.inf deleted from the driver store (the old driver's
    and the per-cable driver's share the name). -KeepIdentities keeps the setup's store, so cables added again keep their
    identities and endpoints. -BuildTool builds the setup tool first.

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
$code = Invoke-CableTool $tool $arguments
if ($code -eq 7) { Write-Note 'Windows wants a restart to finish removing a device: restart, then run this again to delete the packages.'; exit 7 }
if ($code -ne 0) { throw "the setup tool failed ($code): its steps are above" }

Write-Step "Delete $InfName from the driver store"
$published = @(Get-WindowsDriver -Online | Where-Object { (Split-Path -Leaf $_.OriginalFileName) -eq $InfName.ToLower() -or
        (Split-Path -Leaf $_.OriginalFileName) -eq $InfName })
if ($published.Count -eq 0) { Write-Ok 'none there' }
foreach ($d in $published) {
    pnputil /delete-driver $d.Driver /uninstall /force
    if ($LASTEXITCODE -eq 0) { Write-Ok "deleted $($d.Driver)" } else { Write-Note "pnputil couldn't delete $($d.Driver) ($LASTEXITCODE)" }
}
