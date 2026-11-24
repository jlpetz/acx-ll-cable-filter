<#
.SYNOPSIS
    Lists, adds, removes or switches the variant of an ACX-LL-Cable (any number of cables: A, B, C...).

.DESCRIPTION
    TODO 19, the per-cable design (doc/todo/19-per-cable-design.md, section 9). Replaces Cable-B.ps1. Every action runs the
    setup tool, acx-ll-cable (crates\nf-vmic), which holds the setup's lock and does the work; this script only adds the
    restore of Windows' default playback and recording roles around add and variant (Windows may make a new endpoint the
    default; only roles moved to an endpoint that wasn't there before are put back).

    A cable is added on the driver package the other cables run. Only Install-Driver.ps1 (the tool's update) changes the
    package, and moves every cable together.

    -Build builds the tool first (cargo build --release -p nf-vmic --bin acx-ll-cable); without it, a missing tool is an
    error, so no run rebuilds anything unasked.

.EXAMPLE
    .\Cable.ps1 list
.EXAMPLE
    .\Cable.ps1 add B
.EXAMPLE
    .\Cable.ps1 add D -Mics call
.EXAMPLE
    .\Cable.ps1 variant B low-latency
.EXAMPLE
    .\Cable.ps1 remove B -Forget
.EXAMPLE
    .\Cable.ps1 -Build list
#>
param(
    [Parameter(Position = 0)][ValidateSet('list', 'add', 'remove', 'variant')][string]$Action = 'list',
    [Parameter(Position = 1)][string]$Label,
    [Parameter(Position = 2)][ValidateSet('both', 'call', 'low-latency')][string]$To,
    [ValidateSet('both', 'call', 'low-latency')][string]$Mics,
    [string]$Adopt,
    [switch]$Forget,
    [switch]$Build
)

. (Join-Path $PSScriptRoot 'common.ps1')
$tool = Get-CableTool -Build:$Build

if ($Action -eq 'list') {
    exit (Invoke-CableTool $tool @('list'))
}

Assert-Admin
if (-not $Label) { throw "'$Action' takes a cable's label: Cable.ps1 $Action B" }
$arguments = switch ($Action) {
    'add' {
        $a = @('add', $Label)
        if ($Mics) { $a += @('--mics', $Mics) }
        if ($Adopt) { $a += @('--adopt', $Adopt) }
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
}

if ($Action -eq 'remove') {
    exit (Invoke-CableTool $tool $arguments)
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
exit $code
