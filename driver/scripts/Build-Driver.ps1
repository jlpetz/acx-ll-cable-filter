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
    MSBuild as the properties NfPacketFrames and NfMinPacketPeriodHns, which SamplesCommon.vcxproj makes defines. MSBuild
    records each file's compiler command line and recompiles a file whose defines changed, so a build of the other size
    recompiles what it must, with a Visual Studio or bare msbuild build (no properties: 128 frames) in between too.
    On top of that, out\<Configuration>\packet-frames.txt reads <size>-complete once a build has finished, and any other
    build rebuilds everything: one of the other size, one after a build that failed or was stopped (the marker then reads
    <size>-incomplete), one with no marker or an empty one. The last package is removed before MSBuild runs, so a build that
    fails there leaves none in out\ to be taken for the size it was asked for. One that stops after the package is made (at
    InfVerif, if it can't run, or at the signing) leaves the package of the size asked for, unsigned, and the marker
    <size>-complete: the next build of that size builds incrementally (nothing to recompile), then packages and signs.
    The INF's DriverVer is the build's start in UTC, both parts: the date (inf2cat refuses one later than UTC today) and the
    time of day as the version, H.m.s.ms (each part within an INF's 0-65535). By date, then version, a later build ranks
    newer, which the driver store's choice of the newest package (Windows', and the mic variant switch's) relies on.

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
# "<size>-incomplete" from before MSBuild starts until then; a build other than one of the size it says complete rebuilds
# everything. MSBuild would recompile what a size change touches by itself (the size is a define on cl's command line, which
# it records for each file); the Rebuild is kept over that for intermediates built before 2026-11-23, when the size reached cl
# through the CL environment variable, unrecorded (a 96-frame object's recorded command line is a 128-frame one's), and for a
# build that failed or was stopped part way. Anything unknown rebuilds too: a missing marker (out\ may have been cleared while
# the intermediates in src\ remain), an empty one (Set-Content truncates, then writes: a build stopped in between leaves it
# empty) and a bare size, which until 2026-11-23 the script wrote before MSBuild ran.
$last = if (Test-Path $marker) { "$(Get-Content $marker -Raw)".Trim() } else { '' }
$target = if ($last -ne "$PacketFrames-complete") { 'Rebuild' } else { 'Build' }
if ($target -eq 'Rebuild') {
    $why = if ($last) { "the marker read $last" } elseif (Test-Path $marker) { 'an empty marker' } else { 'no marker' }
    Write-Note "rebuilding everything ($why)"
}
New-Item -ItemType Directory -Force -Path $out | Out-Null
Set-Content -Path $marker -Value "$PacketFrames-incomplete" -NoNewline
# The last package goes before MSBuild runs, not after: a build that fails then leaves no package in out\ to be installed as
# this one (Install-Driver.ps1 says there is none).
if (Test-Path $pkg) { Remove-Item -Recurse -Force $pkg }
# The packet size as MSBuild properties, which SamplesCommon.vcxproj makes defines. None for 128 frames, PacketSize.cpp's
# default, so a 128-frame build's objects are a Visual Studio build's. Always an array (@(...)): a single string would be
# splatted a character an argument.
$sizeProps = @(if ($PacketFrames -ne 128) { "-p:NfPacketFrames=$PacketFrames"; '-p:NfMinPacketPeriodHns=10000' })
# The INF's DriverVer, both parts from one reading of the UTC clock at the build's start. The date: inf2cat refuses one later
# than UTC today, and stampinf's default is the local date. The version: the time of day, H.m.s.ms (03:27:41.512 is
# 3.27.41.512), each part within an INF's 0-65535 and the four increasing through the day, so by date, then version, a later
# build ranks newer. Stampinf's default version is the local time of day: with the UTC date, a build after local midnight but
# before UTC midnight had the date of the evening's builds before it and a smaller version.
$now = [DateTime]::UtcNow
$infDate = $now.ToString('MM/dd/yyyy', [Globalization.CultureInfo]::InvariantCulture)
$infVersion = '{0}.{1}.{2}.{3}' -f $now.Hour, $now.Minute, $now.Second, $now.Millisecond
# CL and _CL_ reach cl.exe without MSBuild recording them: cleared for the build, so only the projects set the objects' defines.
$savedCl = $env:CL
$savedClAfter = $env:_CL_
$env:CL = $null
$env:_CL_ = $null
try {
    & $msbuild $Solution "-t:$target" "-p:Configuration=$Configuration" '-p:Platform=x64' '-p:SignMode=Off' `
        '-p:SkipPackageVerification=true' '-p:ApiValidator_Enable=false' "-p:NfInfDate=$infDate" "-p:NfInfVersion=$infVersion" `
        @sizeProps '-m' '-nologo' '-v:minimal'
    if ($LASTEXITCODE -ne 0) { throw 'MSBuild failed.' }
} finally {
    $env:CL = $savedCl
    $env:_CL_ = $savedClAfter
}

$built = Join-Path $ProjectDir "x64\$Configuration"
Write-Step "Package: $pkg (DriverVer $infDate,$infVersion)"
New-Item -ItemType Directory -Force -Path $pkg | Out-Null
foreach ($f in "$PackageName.sys", $InfName) {
    $src = Join-Path $built $f
    if (-not (Test-Path $src)) { throw "missing build output: $src" }
    Copy-Item $src $pkg
}
$pdb = Join-Path $built "$PackageName.pdb"
if (Test-Path $pdb) { Copy-Item $pdb $out }
# The build has finished: its objects and the package's .sys are of this size. (A build stopped below, at InfVerif or the
# signing, leaves the package unsigned, of this size still; the next build of it builds incrementally, then packages and signs.)
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
