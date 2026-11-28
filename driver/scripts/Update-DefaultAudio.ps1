# Package updates and variant switches can replace MMDevice IDs. Remember each cable default by identity and role.
. (Join-Path $PSScriptRoot 'Set-DefaultAudio.ps1')

function Get-CableAudioPorts([string]$Tool, [hashtable]$CableIds) {
    $ports = @{}
    $report = @(& $Tool list)
    if ($LASTEXITCODE) { throw "Could not list the installed cables (exit $LASTEXITCODE)." }
    foreach ($line in $report) {
        if ($line -notmatch '^ACX-LL-Cable ([A-Z0-9]+) \(id ([0-9a-fA-F-]{36})\)(?:$|: disabled)') { continue }
        $cableId = $Matches[2]
        if ($null -ne $CableIds) { $CableIds[$Matches[1]] = $cableId }
        foreach ($role in 'speakers','microphone','call-microphone') {
            # Exit 3 is expected for a one-output variant or a disabled cable. Discard its explanatory stderr.
            $ErrorActionPreference = 'Continue'
            $resolved = @(& $Tool endpoint $cableId $role 2>$null)
            $rc = $LASTEXITCODE
            $ErrorActionPreference = 'Stop'
            if ($rc -eq 3) { continue }
            if ($rc -ne 0) { throw "Could not resolve the $role endpoint of a cable (exit $rc)." }
            if ($resolved.Count -eq 1 -and $resolved[0] -match '^(\{\S+\})\s+active\s') {
                $ports["$cableId/$role"] = $Matches[1]
            }
        }
    }
    return $ports
}

function Invoke-CableEndpointChange([string]$Tool, [string[]]$Arguments) {
    $cableIds = @{}
    $before = Get-CableAudioPorts $Tool $cableIds
    # Only the explicitly removed capture pin may fall back to the sibling on the same cable. Missing endpoints
    # during a package update or an incomplete switch are still reported; they may be available after a retry.
    $removed = $null
    $survivor = $null
    $isVariant = $Arguments[0] -eq 'variant'
    if ($isVariant -and ($cableId = $cableIds[$Arguments[1]])) {
        switch ($Arguments[2]) {
            'call'        { $removed = "$cableId/microphone";      $survivor = "$cableId/call-microphone" }
            'low-latency' { $removed = "$cableId/call-microphone"; $survivor = "$cableId/microphone" }
        }
    }
    $byId = @{}
    foreach ($key in $before.Keys) { $byId[$before[$key]] = $key }
    $defaults = @(foreach ($flow in 0,1) {
        foreach ($role in 0,1,2) {
            $id = [NfPolicyConfig]::GetRole($flow, $role)
            if ($id) { [pscustomobject]@{ Flow=$flow; Role=$role; Id=$id; Port=$byId[$id] } }
        }
    })

    $rc = Invoke-CableTool $Tool $Arguments
    try {
        $after = Get-CableAudioPorts $Tool
        $missed = @()
        foreach ($saved in $defaults) {
            $current = [NfPolicyConfig]::GetRole($saved.Flow, $saved.Role)
            $wanted = if ($saved.Port) { $after[$saved.Port] } else { $saved.Id }
            if (!$wanted -and $removed -and $saved.Port -eq $removed) { $wanted = $after[$survivor] }
            if (!$wanted) { $missed += $saved; continue }
            if ($current -eq $wanted) { continue }
            # Package updates retain a concurrent selection between unrelated devices. Variant switches preserve all six defaults.
            if (!$isVariant -and !$saved.Port -and $current -and $current -notin $after.Values) { continue }
            try {
                [NfPolicyConfig]::SetRole($wanted, $saved.Role)
                if ([NfPolicyConfig]::GetRole($saved.Flow, $saved.Role) -ne $wanted) { $missed += $saved }
            } catch { $missed += $saved }
        }
        if ($missed.Count) {
            $which = ($missed | ForEach-Object { "$(@('playback','recording')[$_.Flow])/$(@('console','multimedia','communications')[$_.Role])" }) -join ', '
            throw "Windows could not restore these default audio assignments: $which."
        }
    } catch {
        Write-Warning $_.Exception.Message
        if (!$rc) { $rc = 2 }
    }
    return $rc
}
