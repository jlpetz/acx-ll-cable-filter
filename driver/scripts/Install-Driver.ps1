<#
.SYNOPSIS
    Installs the built, test-signed package: cable A the first time, or every cable moved to the new build.

.DESCRIPTION
    TODO 19, the per-cable design (doc/todo/19-per-cable-design.md, section 9). The cables' device nodes are made only by the
    setup tool, acx-ll-cable (crates\nf-vmic): the driver refuses a node devcon makes (Code 31). In order:
      1. The checks: an elevated shell, test signing active, the test certificate trusted, a built package.
      2. The package added to the driver store only (pnputil /add-driver, no /install).
      3. One of:
         - the old driver's devices present (ROOT\AudioCodec*, ROOT\NoiseFilterCableB): the migration, once
           (acx-ll-cable migrate): every device of ours disabled until the old image unloads, the old devices removed, and
           cables A (its variant, or -Variant if given) and B (if the old B was there; -CableBMics) made on the new package;
         - cables present: acx-ll-cable update, which moves every cable to the package together (each stops for a few
           seconds) and puts every cable back if the old image doesn't unload (an app holds a cable's control device);
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

if (Test-Present $LegacyHardwareIds) {
    Write-Step 'Migrate the old driver''s devices to cables (acx-ll-cable migrate)'
    $arguments = @('migrate', $package, '--b-mics', $CableBMics)
    if ($PSBoundParameters.ContainsKey('Variant')) { $arguments += @('--a-mics', $Variant) }
    $code = Invoke-CableTool $tool $arguments
} elseif (Test-Present @($HardwareIds.Values)) {
    Write-Step 'Move every cable to the new package (acx-ll-cable update)'
    $code = Invoke-CableTool $tool @('update', $package)
} else {
    Write-Step "Make cable A ($Variant)"
    $code = Invoke-CableTool $tool @('add', 'A', '--mics', $Variant)
}
if ($code -eq 7) { Write-Note 'Windows wants a restart to finish.' }
elseif ($code -ne 0) { throw "the setup tool failed ($code): its steps are above" }

& (Join-Path $PSScriptRoot 'Get-DriverStatus.ps1')
