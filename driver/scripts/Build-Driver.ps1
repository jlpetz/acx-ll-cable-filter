<#
.SYNOPSIS
    Builds the driver and makes its test-signed package in driver\out\<Configuration>\<package>.

.DESCRIPTION
    1. MSBuild (x64) with the WDK's own signing off, and two of its checks off: on VS 2026's 64-bit MSBuild, WDK 28000's
       InfVerif task looks for x86\InfVerif.dll and its ApiValidator's aitstatic exits 193. InfVerif runs here instead,
       from the WDK's x64 tools (reported, not fatal).
    2. The package: the stamped INF and the .sys, the .sys signed with our test certificate, the catalog made by inf2cat
       for Windows 11 24H2 and Server 2025 (KMDF 1.35's floor), and signed.
    -NoSign skips the signing (a build check only).

.EXAMPLE
    .\Build-Driver.ps1
#>
param(
    [ValidateSet('Release', 'Debug')][string]$Configuration = 'Release',
    [switch]$NoSign
)

. (Join-Path $PSScriptRoot 'common.ps1')

Write-Step "Build ($Configuration, x64)"
$msbuild = Find-MSBuild
& $msbuild $Solution "-p:Configuration=$Configuration" '-p:Platform=x64' '-p:SignMode=Off' '-p:SkipPackageVerification=true' `
    '-p:ApiValidator_Enable=false' '-m' '-nologo' '-v:minimal'
if ($LASTEXITCODE -ne 0) { throw 'MSBuild failed.' }

$built = Join-Path $ProjectDir "x64\$Configuration"
$pkg = Join-Path $DriverRoot "out\$Configuration\$PackageName"
Write-Step "Package: $pkg"
if (Test-Path $pkg) { Remove-Item -Recurse -Force $pkg }
New-Item -ItemType Directory -Force -Path $pkg | Out-Null
foreach ($f in "$PackageName.sys", $InfName) {
    $src = Join-Path $built $f
    if (-not (Test-Path $src)) { throw "missing build output: $src" }
    Copy-Item $src $pkg
}
$pdb = Join-Path $built "$PackageName.pdb"
if (Test-Path $pdb) { Copy-Item $pdb (Join-Path $DriverRoot "out\$Configuration") }

Write-Step 'InfVerif'
$infverif = Find-WdkTool 'infverif.exe'
$report = & $infverif /w (Join-Path $pkg $InfName) 2>&1 | Out-String
if ($LASTEXITCODE -eq 0) { Write-Ok 'clean' } else { Write-Note "reported (not fatal):`n$report" }

if ($NoSign) {
    Write-Note 'not signed (-NoSign)'
    return
}
Write-Step 'Sign'
Invoke-SignTool @((Join-Path $pkg "$PackageName.sys"))
$inf2cat = Find-WdkTool 'inf2cat.exe'
& $inf2cat "/driver:$pkg" "/os:$CatalogOs" /uselocaltime | Out-Null
if ($LASTEXITCODE -ne 0) { throw "inf2cat failed (run it by hand for the reason: $inf2cat /driver:$pkg /os:$CatalogOs /uselocaltime)" }
$cat = Get-ChildItem $pkg -Filter '*.cat' | Select-Object -First 1
if (-not $cat) { throw 'inf2cat made no catalog.' }
Invoke-SignTool @($cat.FullName)
Write-Ok "package ready: $pkg"
