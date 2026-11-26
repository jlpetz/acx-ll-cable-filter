<#
.SYNOPSIS
    Lists, adds, removes, disables, enables or switches the variant of an ACX-LL-Cable (any number of cables: A, B, C...), or
    finds a cable's endpoint by role.

.DESCRIPTION
    TODO 19, the per-cable design (doc/todo/19-per-cable-design.md, section 9). Replaces Cable-B.ps1. Every action runs the
    setup tool, acx-ll-cable (crates\nf-vmic), which holds the setup's lock and does the work; this script only adds the
    restore of Windows' default playback and recording roles around add and variant (Windows may make a new endpoint the
    default; only roles moved to an endpoint that wasn't there before are put back).

    A cable is added on the driver package the other cables run. Only Install-Driver.ps1 (the tool's update) changes the
    package, and moves every cable together.

    disable <label> keeps the cable installed but off, across restarts too, until enable <label>; its endpoints keep their ids
    (not present while it is off), and enable checks they came back as they were. No other cable is touched. A disable
    Windows can't finish now (an app has the cable in use) is made at the next restart (exit 7): until then the cable works,
    list says it is disabled at the next restart, and enable cancels it. variant refuses a disabled cable: enable it first.

    endpoint <label|id> speakers|microphone|call-microphone (any user) writes the cable's endpoint for that role as Windows has
    it now, found by the cable's identity and the endpoint's pin, never by name: one line, "<id> <state> "<name>"", to the
    output (so a caller can take it: $line = .\Cable.ps1 endpoint B call-microphone). None now: exit 3, why on stderr.

    -Build builds the tool first (cargo build --release -p nf-vmic --bin acx-ll-cable); without it, a missing tool is an
    error, so no run rebuilds anything unasked. add -EndpointMap <file>: which new endpoint takes each old one's place (a
    re-added or repaired cable's, an adopted device's), as JSON (acx-ll-cable's --endpoint-map).

    Exit codes: 0 done (or nothing to do); 7 Windows wants a restart to finish, with or without other errors beside it (stop
    there: restart, then run this again); 2 the setup tool's error (its steps above say what changed); 3 endpoint only: no
    endpoint for that role now; 1 a usage error or this script's own check (not elevated, no tool).

.EXAMPLE
    .\Cable.ps1 list
.EXAMPLE
    .\Cable.ps1 add B
.EXAMPLE
    .\Cable.ps1 add B -EndpointMap C:\temp\b-map.json
.EXAMPLE
    .\Cable.ps1 add D -Mics call
.EXAMPLE
    .\Cable.ps1 variant B low-latency
.EXAMPLE
    .\Cable.ps1 remove B -Forget
.EXAMPLE
    .\Cable.ps1 disable B
.EXAMPLE
    .\Cable.ps1 enable B
.EXAMPLE
    .\Cable.ps1 endpoint B call-microphone
.EXAMPLE
    .\Cable.ps1 -Build list
#>
param(
    [Parameter(Position = 0)][ValidateSet('list', 'add', 'remove', 'variant', 'disable', 'enable', 'endpoint')][string]$Action = 'list',
    [Parameter(Position = 1)][string]$Label,
    # variant's microphones (both, call, low-latency), or endpoint's role (speakers, microphone, call-microphone): the tool checks it
    [Parameter(Position = 2)][string]$To,
    [ValidateSet('both', 'call', 'low-latency')][string]$Mics,
    [string]$Adopt,
    [string]$EndpointMap,
    [switch]$Forget,
    [switch]$Build
)

. (Join-Path $PSScriptRoot 'common.ps1')
if ($EndpointMap -and $Action -ne 'add') { throw "-EndpointMap goes with add: Cable.ps1 add B -EndpointMap <file>" }
if ($To -and $Action -notin 'variant', 'endpoint') { throw "'$Action' takes no third argument ('$To')" }
if (($Mics -or $Adopt) -and $Action -ne 'add') { throw "-Mics and -Adopt go with add" }
if ($Forget -and $Action -ne 'remove') { throw "-Forget goes with remove" }
$tool = Get-CableTool -Build:$Build

if ($Action -eq 'list') {
    exit (Invoke-CableTool $tool @('list'))
}

if ($Action -eq 'endpoint') {
    # Any user. The tool's line goes to the output, for a caller to take; why there is none goes to stderr (exit 3). Not under
    # common.ps1's 'Stop': a caller that redirects this script's stderr ($line = .\Cable.ps1 endpoint B speakers 2>$null, or
    # 2>&1) makes Windows PowerShell 5.1 take the tool's "none: ..." as a NativeCommandError, which would end the script with 1
    if (-not $Label -or -not $To) { throw 'endpoint takes a cable and a role: Cable.ps1 endpoint B speakers|microphone|call-microphone' }
    $ErrorActionPreference = 'Continue'
    & $tool endpoint $Label $To
    exit $LASTEXITCODE
}

Assert-Admin
if (-not $Label) { throw "'$Action' takes a cable's label: Cable.ps1 $Action B" }
$arguments = switch ($Action) {
    'add' {
        $a = @('add', $Label)
        if ($Mics) { $a += @('--mics', $Mics) }
        if ($Adopt) { $a += @('--adopt', $Adopt) }
        if ($EndpointMap) { $a += @('--endpoint-map', $EndpointMap) }
        $a
    }
    'remove' {
        $a = @('remove', $Label)
        if ($Forget) { $a += '--forget' }
        $a
    }
    'variant' {
        if (-not $To) { throw 'variant takes the microphones: Cable.ps1 variant B both|call|low-latency' }
        @('variant', $Label, $To)
    }
    'disable' { @('disable', $Label) }
    'enable' { @('enable', $Label) }
}

if ($Action -in 'remove', 'disable', 'enable') {
    # No new endpoint, so no default role to put back (a disable moves Windows' defaults off the cable while it is off)
    $doing = @{ 'remove' = 'removing'; 'disable' = 'disabling'; 'enable' = 'enabling' }[$Action]
    Exit-ForToolCode (Invoke-CableTool $tool $arguments) "$doing cable $Label"
    exit 0
}

# add and variant make endpoints: note the default roles and the endpoints there now, and put back any role Windows moved to
# a new one (Cable-B.ps1's restore, for any cable)
. (Join-Path $PSScriptRoot 'Set-DefaultAudio.ps1')
$defaults = @{}
foreach ($flow in 0, 1) { foreach ($role in 0..2) { $defaults["$flow/$role"] = [NfPolicyConfig]::GetRole($flow, $role) } }
$before = @(Get-PnpDevice -Class AudioEndpoint -ErrorAction SilentlyContinue | ForEach-Object { $_.InstanceId -replace '^SWD\\MMDEVAPI\\', '' })
$code = 2
try {
    $code = Invoke-CableTool $tool $arguments
} finally {
    foreach ($flow in 0, 1) { foreach ($role in 0..2) {
        $old = $defaults["$flow/$role"]
        $now = [NfPolicyConfig]::GetRole($flow, $role)
        if ($old -and $now -and $now -ne $old -and $before -notcontains $now) {
            try {
                [NfPolicyConfig]::SetRole($old, $role)
                Write-Note "put the default role back: flow $flow, role $role"
            } catch {
                Write-Note "the default role (flow $flow, role $role) moved to a new endpoint and couldn't be put back: $($_.Exception.Message)"
            }
        }
    } }
}
Exit-ForToolCode $code "$Action $Label"
exit 0
