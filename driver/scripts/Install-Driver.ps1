<#
.SYNOPSIS
    Installs the built, test-signed package: cable A the first time, or every cable moved to the new build.

.DESCRIPTION
    TODO 19, the per-cable design (doc/todo/19-per-cable-design.md, section 9). The cables' device nodes are made only by the
    setup tool, acx-ll-cable (crates\nf-vmic): the driver refuses a node devcon makes (Code 31). In order:
      1. The checks: an elevated shell, test signing active, the test certificate trusted, a built package.
      2. The package added to the driver store only (pnputil /add-driver, no /install).
      3. One of:
         - the old driver's devices present (ROOT\AudioCodec*, ROOT\NoiseFilterCableB), or cables an earlier migration
           recorded and didn't make: the migration (acx-ll-cable migrate). Nothing changes unless the package makes A and B;
           then A and B are recorded in the setup's store, every device of ours is disabled until the old image unloads, the
           old devices are removed, and A (its variant, or -Variant if given) and B (if the old B was there; -CableBMics) are
           made on the new package. A run that stops part way (a failure, a restart Windows needs) is finished by running this
           again;
         - cables present: acx-ll-cable update, which moves every cable to the package together (each stops for a few
           seconds) and puts every cable back if the old image doesn't unload (an app holds a cable's control device). With
           -Variant, cable A is then switched to it (acx-ll-cable variant A), as -Variant switched the device before cables;
         - none: acx-ll-cable add A --mics <Variant>.
      4. The status.

    -Variant keeps its meaning, for cable A: both microphones (the default), the call one, or the low-latency one. Other
    cables come with Cable.ps1 add. -BuildTool builds the setup tool first.

.EXAMPLE
    .\Install-Driver.ps1
.EXAMPLE
    .\Install-Driver.ps1 -Variant call -BuildTool
#>
param(
    [ValidateSet('Release', 'Debug')][string]$Configuration = 'Release',
    [ValidateSet('both', 'call', 'low-latency')][string]$Variant = 'both',
    [ValidateSet('both', 'call', 'low-latency')][string]$CableBMics = 'both',
    [switch]$BuildTool
)

. (Join-Path $PSScriptRoot 'common.ps1')
Assert-Admin
if (-not (Get-TestSigningActive)) { throw 'Test signing is not active: run Setup-TestSigning.ps1, then reboot.' }
$cert = Get-TestCert
if (-not $cert -or -not (Test-CertTrusted $cert)) { throw 'The test certificate is missing or untrusted: run Setup-TestSigning.ps1.' }

$package = Join-Path $DriverRoot "out\$Configuration\$PackageName"
$inf = Join-Path $package $InfName
if (-not (Test-Path $inf)) { throw "No package: run Build-Driver.ps1 first ($inf)." }
$tool = Get-CableTool -Build:$BuildTool

Write-Step "Add the package to the driver store (pnputil /add-driver, no device installed)"
pnputil /add-driver $inf
if ($LASTEXITCODE -ne 0) { throw "pnputil /add-driver failed ($LASTEXITCODE)" }

function Test-Present([string[]]$Ids) {
    @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object { $d = $_; @($Ids | Where-Object { $d.HardwareID -contains $_ }).Count -gt 0 }).Count -gt 0
}

$pending = @(Get-PendingCables)
$switchA = $false
if ((Test-Present $LegacyHardwareIds) -or $pending.Count -gt 0) {
    if ($pending.Count -gt 0) { Write-Note "an earlier migration left these cables to make: $($pending -join ', ')" }
    Write-Step 'Migrate the old driver''s devices to cables (acx-ll-cable migrate)'
    $arguments = @('migrate', $package)
    if ($PSBoundParameters.ContainsKey('Variant')) { $arguments += @('--a-mics', $Variant) }
    if ($PSBoundParameters.ContainsKey('CableBMics')) { $arguments += @('--b-mics', $CableBMics) }
    $code = Invoke-CableTool $tool $arguments
} elseif (Test-Present @($HardwareIds.Values)) {
    Write-Step 'Move every cable to the new package (acx-ll-cable update)'
    $code = Invoke-CableTool $tool @('update', $package)
    $switchA = $PSBoundParameters.ContainsKey('Variant')
} else {
    Write-Step "Make cable A ($Variant)"
    $code = Invoke-CableTool $tool @('add', 'A', '--mics', $Variant)
}
if ($code -eq 7) { Write-Note 'Windows wants a restart to finish: restart, then run this again if the steps above say so.' }
elseif ($code -ne 0) { throw "the setup tool failed ($code): its steps are above" }

if ($switchA -and $code -eq 0) {
    # -Variant switched the one device before cables; with cables, it is cable A's (nothing if A has it already). Like the
    # switch here before, this doesn't restore default roles Windows moves; Cable.ps1 variant A does.
    Write-Step "Cable A's variant: $Variant (acx-ll-cable variant A $Variant)"
    $code = Invoke-CableTool $tool @('variant', 'A', $Variant)
    if ($code -eq 7) { Write-Note 'Windows wants a restart to finish the switch.' }
    elseif ($code -ne 0) { throw "the update is done, but cable A's variant switch failed ($code): its steps are above" }
}

& (Join-Path $PSScriptRoot 'Get-DriverStatus.ps1')
