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
    -PacketFrames 96 builds the 96-frame trial (TODO 19): NF_PACKET_FRAMES 96 and a 1 ms floor (PacketSize.cpp), passed to
    cl.exe through the CL environment variable, which MSBuild doesn't track. out\<Configuration>\packet-frames.txt reads
    <size>-complete once a build has finished, and any other build rebuilds everything: one of the other size, one after a
    build that failed or was stopped (the marker then reads <size>-incomplete), one with no marker. The last package is removed
    before MSBuild runs, so a build that fails leaves none in out\ to be taken for the size it was asked for.

.EXAMPLE
    .\Build-Driver.ps1
.EXAMPLE
    .\Build-Driver.ps1 -PacketFrames 96
#>
param(
    [ValidateSet('Release', 'Debug')][string]$Configuration = 'Release',
    [ValidateSet(96, 128)][int]$PacketFrames = 128,
    [switch]$NoSign
)

. (Join-Path $PSScriptRoot 'common.ps1')

Write-Step "Build ($Configuration, x64, $PacketFrames-frame packets)"
$msbuild = Find-MSBuild
$out = Join-Path $DriverRoot "out\$Configuration"
$marker = Join-Path $out 'packet-frames.txt'
$pkg = Join-Path $out $PackageName
# The marker reads "<size>-complete" once a build of that size has finished (MSBuild, then the package made), and
# "<size>-incomplete" from before MSBuild starts until then, so a build that fails or is stopped part way leaves it incomplete
# and the next build rebuilds, whatever its size: objects the stopped build didn't get to are the last size's (all of them,
# should it stop before its Rebuild's Clean), and an incremental build wouldn't recompile them (MSBuild doesn't see the CL
# variable change). Anything else is unknown: rebuild. That covers a missing marker (out\ may have been cleared while the
# intermediates in src\ remain) and a bare size, which until 2026-11-23 the script wrote before MSBuild ran.
$last = if (Test-Path $marker) { (Get-Content $marker -Raw).Trim() } else { '' }
$target = if ($last -ne "$PacketFrames-complete") { 'Rebuild' } else { 'Build' }
if ($target -eq 'Rebuild') {
    $why = if ($last) { "the marker read $last" } else { 'no marker' }
    Write-Note "rebuilding everything ($why)"
}
New-Item -ItemType Directory -Force -Path $out | Out-Null
Set-Content -Path $marker -Value "$PacketFrames-incomplete" -NoNewline
# The last package goes before MSBuild runs, not after: a build that fails then leaves no package in out\ to be installed as
# this one (Install-Driver.ps1 says there is none).
if (Test-Path $pkg) { Remove-Item -Recurse -Force $pkg }
$savedCl = $env:CL
if ($PacketFrames -ne 128) { $env:CL = "/DNF_PACKET_FRAMES#$PacketFrames /DNF_MIN_PACKET_PERIOD_HNS#10000" } else { $env:CL = $null }
try {
    # The INF's DriverVer date in UTC: inf2cat refuses one later than UTC today, and stampinf's default is the local date.
    $infDate = (Get-Date).ToUniversalTime().ToString('MM/dd/yyyy', [Globalization.CultureInfo]::InvariantCulture)
    & $msbuild $Solution "-t:$target" "-p:Configuration=$Configuration" '-p:Platform=x64' '-p:SignMode=Off' `
        '-p:SkipPackageVerification=true' '-p:ApiValidator_Enable=false' "-p:NfInfDate=$infDate" '-m' '-nologo' '-v:minimal'
    if ($LASTEXITCODE -ne 0) { throw 'MSBuild failed.' }
} finally {
    $env:CL = $savedCl
}

$built = Join-Path $ProjectDir "x64\$Configuration"
Write-Step "Package: $pkg"
New-Item -ItemType Directory -Force -Path $pkg | Out-Null
foreach ($f in "$PackageName.sys", $InfName) {
    $src = Join-Path $built $f
    if (-not (Test-Path $src)) { throw "missing build output: $src" }
    Copy-Item $src $pkg
}
$pdb = Join-Path $built "$PackageName.pdb"
if (Test-Path $pdb) { Copy-Item $pdb $out }
# The build has finished: its objects and the package's .sys are of this size. (A signing that fails below leaves the package
# unsigned, of this size still; the next build of it just packages and signs again.)
Set-Content -Path $marker -Value "$PacketFrames-complete" -NoNewline

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
