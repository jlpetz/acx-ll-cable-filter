<#
.SYNOPSIS
    Sets this machine up to load our test-signed driver: a test certificate, trusted, and test signing on.

.DESCRIPTION
    Ported from TMR-MD's build.ps1 (Create-Kernel-Certificate, Install-Certificate), which worked on Windows 11; the same
    steps apply on Server 2025. Idempotent: what's already in place is left alone.
    1. A self-signed code-signing certificate, "CN=Noise Filter Test Driver", in LocalMachine\My (made once).
    2. It, trusted: in LocalMachine\Root and LocalMachine\TrustedPublisher.
    3. Test signing on for the current boot entry (bcdedit /set testsigning on). Needs Secure Boot off, and takes a reboot.
    With -Ems, also Windows' Emergency Management Services on COM1, for the EC2 serial console (a driver that bugchecks at
    boot can still be reached; the account must allow serial console access too).
    It never reboots: it says when one is needed.

.EXAMPLE
    .\Setup-TestSigning.ps1 -Ems
#>
param([switch]$Ems)

. (Join-Path $PSScriptRoot 'common.ps1')
Assert-Admin

Write-Step 'Secure Boot'
try { $secure = Confirm-SecureBootUEFI } catch { $secure = $false }
if ($secure) { throw 'Secure Boot is on: test signing would be refused. Turn it off first.' }
Write-Ok 'off (test signing can take effect)'

Write-Step 'Test certificate'
New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
$cert = Get-TestCert
if ($cert) {
    Write-Ok "present: $($cert.Thumbprint), until $($cert.NotAfter.ToString('yyyy-MM-dd'))"
} else {
    $cert = New-SelfSignedCertificate -Subject $CertSubject -Type CodeSigningCert -CertStoreLocation 'Cert:\LocalMachine\My' `
        -HashAlgorithm SHA256 -KeyAlgorithm RSA -KeyLength 2048 -KeyExportPolicy Exportable -NotAfter (Get-Date).AddYears(5) `
        -TextExtension @('2.5.29.19={text}CA=FALSE')
    Write-Ok "made: $($cert.Thumbprint)"
}
Export-Certificate -Cert $cert -FilePath $CertFile -Force | Out-Null
foreach ($store in 'Root', 'TrustedPublisher') {
    if (Test-Path "Cert:\LocalMachine\$store\$($cert.Thumbprint)") {
        Write-Ok "already trusted in LocalMachine\$store"
    } else {
        Import-Certificate -FilePath $CertFile -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
        Write-Ok "added to LocalMachine\$store"
    }
}

Write-Step 'Test signing'
$reboot = $false
if (Get-TestSigning) {
    Write-Ok 'set in the boot entry'
} else {
    bcdedit /set testsigning on | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'bcdedit /set testsigning on failed.' }
    Write-Ok 'set in the boot entry now'
    $reboot = $true
}
if (Get-TestSigningActive) { Write-Ok 'active in the running kernel' } else { $reboot = $true }

if ($Ems) {
    Write-Step 'Emergency Management Services (EC2 serial console)'
    bcdedit /ems '{current}' on | Out-Null
    bcdedit /emssettings EMSPORT:1 EMSBAUDRATE:115200 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'bcdedit /emssettings failed.' }
    Write-Ok 'on, COM1 at 115200 (takes effect at the next boot)'
}

if ($reboot) {
    Write-Note 'A reboot is needed before a test-signed driver loads. This script does not reboot.'
} else {
    Write-Ok 'Ready: test-signed drivers load on this machine.'
}
