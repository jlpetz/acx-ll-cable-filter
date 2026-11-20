<#
.SYNOPSIS
    Removes the device and our driver package from the driver store.

.EXAMPLE
    .\Uninstall-Driver.ps1
#>
. (Join-Path $PSScriptRoot 'common.ps1')
Assert-Admin

Write-Step "Remove $HardwareId"
& (Find-WdkTool 'devcon.exe') remove $HardwareId

Write-Step "Delete $InfName from the driver store"
$published = @(Get-WindowsDriver -Online | Where-Object { (Split-Path -Leaf $_.OriginalFileName) -eq $InfName.ToLower() -or
        (Split-Path -Leaf $_.OriginalFileName) -eq $InfName })
if ($published.Count -eq 0) { Write-Ok 'none there' }
foreach ($d in $published) {
    pnputil /delete-driver $d.Driver /uninstall /force
    if ($LASTEXITCODE -eq 0) { Write-Ok "deleted $($d.Driver)" } else { Write-Note "pnputil couldn't delete $($d.Driver) ($LASTEXITCODE)" }
}
