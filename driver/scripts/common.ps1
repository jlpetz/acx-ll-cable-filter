# Shared helpers for the driver scripts (dot-source this). Ported from TMR-MD's build.ps1 (its test certificate, trusted
# stores and signtool handling); the install side is new: an audio driver is a PnP device (an INF, a signed catalog and a
# root-enumerated device node), not an NT service.

$ErrorActionPreference = 'Stop'
# Windows PowerShell started from a shell whose PSModulePath lists PowerShell 7's modules first (Git Bash's does) loads 7's
# Security module, which gives 5.1 no Cert: drive. An ordinary PowerShell window is fine; from such a shell, start it with
# PSModulePath unset (`env -u PSModulePath powershell -File ...`).
if (-not (Get-PSDrive Cert -ErrorAction SilentlyContinue)) {
    throw 'No Cert: drive: PSModulePath points at PowerShell 7''s modules. Start Windows PowerShell with PSModulePath unset.'
}

# The driver being worked on. The spike starts from Microsoft's ACX AudioCodec sample, so these are its names for now.
$script:DriverRoot   = Split-Path -Parent $PSScriptRoot
$script:Solution     = Join-Path $DriverRoot 'src\AudioCodec\Driver\AudioCodec.sln'
$script:ProjectDir   = Join-Path $DriverRoot 'src\AudioCodec\Driver'
$script:PackageName  = 'AudioCodec'
$script:InfName      = 'AudioCodec.inf'
# The three variants (TODO 19, the owner's options A and D), shared by every cable (the per-cable design, D1's INF): one INF,
# a cable's microphones chosen by hardware ID. Each cable's device node is ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>, made by the
# setup tool (acx-ll-cable, crates\nf-vmic) and nothing else: the driver refuses a node devcon makes (Code 31).
$script:HardwareIds  = [ordered]@{
    'both'        = 'ROOT\AcxLlCable'              # the low-latency and the call microphone
    'call'        = 'ROOT\AcxLlCableCall'          # the call microphone (a 5 ms default period)
    'low-latency' = 'ROOT\AcxLlCableLowLatency'    # the low-latency microphone (IAudioClient3's small periods)
}
# The old driver's hardware IDs, before the per-cable driver: the migration (Install-Driver.ps1) removes their devices.
$script:LegacyHardwareIds = @('ROOT\AudioCodec', 'ROOT\AudioCodecCall', 'ROOT\AudioCodecLowLatency', 'ROOT\NoiseFilterCableB')
# KMDF 1.35 needs Windows 11 24H2 or Server 2025 (build 26100).
$script:CatalogOs    = '10_GE_X64,Server2025_X64'

# The test-signing certificate: one per machine, in LocalMachine\My, trusted through Root and TrustedPublisher.
$script:CertSubject  = 'CN=Noise Filter Test Driver'
$script:StateDir     = Join-Path $env:LOCALAPPDATA 'noise-filter\driver'
$script:CertFile     = Join-Path $StateDir 'NoiseFilterTestDriver.cer'

$script:WdkVersion   = '10.0.28000.0'
$script:Kits         = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'

function Write-Step($m) { Write-Host "== $m" -ForegroundColor Cyan }
function Write-Ok($m) { Write-Host "   $m" -ForegroundColor Green }
function Write-Note($m) { Write-Host "   $m" -ForegroundColor Yellow }

function Assert-Admin {
    $p = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'This needs an elevated PowerShell (Run as administrator).'
    }
}

# A WDK tool by name: the x64 build of it in the WDK's bin or Tools folder.
function Find-WdkTool([string]$Name) {
    $candidates = @(
        (Join-Path $Kits "bin\$WdkVersion\x64\$Name"),
        (Join-Path $Kits "Tools\$WdkVersion\x64\$Name"),
        (Join-Path $Kits "bin\$WdkVersion\x86\$Name")
    )
    foreach ($c in $candidates) { if (Test-Path $c) { return $c } }
    throw "$Name not found in the WDK $WdkVersion (looked in bin and Tools)."
}

# The cables' setup tool (TODO 19, the per-cable design, section 9): the release build of crates\nf-vmic's acx-ll-cable.
# -Build builds it first (cargo build --release -p nf-vmic --bin acx-ll-cable), so no run rebuilds anything unasked; without
# it, a missing tool is an error.
$script:RepoRoot = Split-Path -Parent $DriverRoot
function Get-CableTool([switch]$Build) {
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $RepoRoot 'target' }
    $tool = Join-Path $target 'release\acx-ll-cable.exe'
    if ($Build) {
        Write-Step 'Build the setup tool (cargo build --release -p nf-vmic --bin acx-ll-cable)'
        Push-Location $RepoRoot
        try { cargo build --release -p nf-vmic --bin acx-ll-cable } finally { Pop-Location }
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    }
    if (-not (Test-Path $tool)) { throw "The setup tool isn't built ($tool): run this with -Build, or Cable.ps1 -Build list." }
    return $tool
}

# Runs the setup tool with its arguments, its output shown as it comes; its exit code (0 done, 2 an error, 7 a restart).
function Invoke-CableTool([string]$Tool, [string[]]$Arguments) {
    & $Tool @Arguments | Out-Host
    return $LASTEXITCODE
}

function Find-MSBuild {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { throw 'vswhere.exe not found: is Visual Studio installed?' }
    $path = & $vswhere -latest -prerelease -requires Microsoft.Component.MSBuild -find 'MSBuild\**\Bin\MSBuild.exe' | Select-Object -First 1
    if (-not $path) { throw 'MSBuild.exe not found.' }
    return $path
}

function Get-TestCert {
    Get-ChildItem Cert:\LocalMachine\My | Where-Object { $_.Subject -eq $CertSubject } | Sort-Object NotAfter -Descending |
        Select-Object -First 1
}

function Test-CertTrusted($cert) {
    $root = Test-Path "Cert:\LocalMachine\Root\$($cert.Thumbprint)"
    $pub = Test-Path "Cert:\LocalMachine\TrustedPublisher\$($cert.Thumbprint)"
    return ($root -and $pub)
}

# Whether test signing is on for the boot entry in use (as of the last boot, or set for the next).
function Get-TestSigning {
    $line = bcdedit /enum '{current}' | Select-String -Pattern '^\s*testsigning\s+(\S+)'
    return ($line -and $line.Matches[0].Groups[1].Value -eq 'Yes')
}

# Test signing as the running kernel has it (set at boot): the code integrity options in the registry.
function Get-TestSigningActive {
    $opts = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control' -Name SystemStartOptions -ErrorAction SilentlyContinue).SystemStartOptions
    return ($opts -match 'TESTSIGNING')
}

function Invoke-SignTool([string[]]$Files) {
    $cert = Get-TestCert
    if (-not $cert) { throw "No test certificate ($CertSubject): run Setup-TestSigning.ps1 first." }
    $signtool = Find-WdkTool 'signtool.exe'
    foreach ($f in $Files) {
        # No timestamp: a test signature needs none, and the build shouldn't need the network.
        & $signtool sign /q /sm /s My /sha1 $cert.Thumbprint /fd SHA256 $f
        if ($LASTEXITCODE -ne 0) { throw "signtool failed on $f" }
        & $signtool verify /q /pa $f
        if ($LASTEXITCODE -ne 0) { throw "signtool verify failed on $f (is the certificate trusted? run Setup-TestSigning.ps1)" }
        Write-Ok "signed $(Split-Path -Leaf $f)"
    }
}
