# Run in a fresh PowerShell process. All cable queries and Windows policy calls use in-memory fixtures.
class NfPolicyConfig {
    static [hashtable] $Defaults
    static [string] GetRole([int]$flow, [int]$role) { return [NfPolicyConfig]::Defaults["$flow/$role"] }
    static [void] SetRole([string]$id, [int]$role) {
        $flow = [int]$id.StartsWith('{c-')
        [NfPolicyConfig]::Defaults["$flow/$role"] = $id
    }
}
$ErrorActionPreference = 'Stop'
if (('NfPolicyConfig' -as [type]) -ne [NfPolicyConfig]) { throw 'Expected in-memory policy fixture.' }
. (Join-Path $PSScriptRoot '../driver/scripts/Update-DefaultAudio.ps1')

$a = [guid]::NewGuid().ToString()
$b = [guid]::NewGuid().ToString()
$initial = @{"$a/speakers"='{r-a}'; "$b/speakers"='{r-b}'; "$b/microphone"='{c-low}'; "$b/call-microphone"='{c-call}'}
$defaults = @{'0/0'='{r-b}'; '0/1'='{r-a}'; '0/2'='{r-other}'; '1/0'='{c-low}'; '1/1'='{c-call}'; '1/2'='{c-other}'}

function Read-FakeCable([string]$action, [string]$cableId, [string]$pin) {
    $global:LASTEXITCODE = 0
    if ($action -eq 'list') { "ACX-LL-Cable A (id $a)"; "ACX-LL-Cable B (id $b)"; return }
    $endpoint = $script:ports["$cableId/$pin"]
    if ($endpoint) { "$endpoint active fixture" } else { $global:LASTEXITCODE = 3 }
}

function Invoke-CableTool {
    $script:ports = $script:replacement
    # Windows has moved all six defaults while replacing the cable endpoints.
    foreach ($role in 0..2) {
        [NfPolicyConfig]::Defaults["0/$role"] = '{r-temp}'
        [NfPolicyConfig]::Defaults["1/$role"] = '{c-temp}'
    }
    return $script:toolExit
}

function Test-Change([string[]]$Command, [string]$removePin, [string]$lowDefault, [string]$callDefault, [int]$toolExit, [int]$wantExit) {
    $script:ports = $initial.Clone()
    $script:replacement = $initial.Clone()
    $script:replacement["$b/speakers"] = '{r-new}'
    $script:replacement["$b/microphone"] = '{c-low-new}'
    $script:replacement["$b/call-microphone"] = '{c-call-new}'
    if ($removePin) { $script:replacement.Remove("$b/$removePin") }
    $script:toolExit = $toolExit
    [NfPolicyConfig]::Defaults = $defaults.Clone()
    $rc = Invoke-CableEndpointChange 'Read-FakeCable' $Command
    if ($rc -ne $wantExit) { throw "$Command returned $rc, expected $wantExit." }
    $expected = $defaults.Clone()
    $expected['0/0'] = '{r-new}'
    $expected['1/0'] = $lowDefault
    $expected['1/1'] = $callDefault
    if ($Command[0] -eq 'update') { $expected['0/2'] = '{r-temp}'; $expected['1/2'] = '{c-temp}' }
    foreach ($key in $expected.Keys) {
        $actual = [NfPolicyConfig]::Defaults[$key]
        if ($actual -ne $expected[$key]) { throw "$Command left $key at $actual, expected $($expected[$key])." }
    }
}

Test-Change @('variant','b','call') 'microphone' '{c-call-new}' '{c-call-new}' 0 0
Test-Change @('variant','B','low-latency') 'call-microphone' '{c-low-new}' '{c-low-new}' 0 0
Test-Change @('variant','B','both') '' '{c-low-new}' '{c-call-new}' 0 0
# A disappearing role outside the requested switch must be reported; preserve the tool's restart code on partial failure.
Test-Change @('variant','A','call') 'microphone' '{c-temp}' '{c-call-new}' 7 7
Test-Change @('update','package') 'microphone' '{c-temp}' '{c-call-new}' 0 2
Write-Host 'Passed cable default restoration checks.'
