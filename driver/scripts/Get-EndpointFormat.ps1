<#
.SYNOPSIS
    The device format Windows stored for each audio endpoint whose name matches (PKEY_AudioEngine_DeviceFormat): the format
    the shared-mode engine runs the device at.

.EXAMPLE
    .\Get-EndpointFormat.ps1 -Match 'AudioCodec'
#>
param([string]$Match = 'AudioCodec')

$ErrorActionPreference = 'Stop'
$nameKey = '{b3f8fa53-0004-438e-9003-51a46e139bfc},6'      # the device's name (e.g. 'AudioCodec Device')
$formatKey = '{f19f064d-082c-4e27-bc73-6882a1bb8e4c},0'    # PKEY_AudioEngine_DeviceFormat
$float = [guid]'00000003-0000-0010-8000-00aa00389b71'
$pcm = [guid]'00000001-0000-0010-8000-00aa00389b71'
foreach ($flow in 'Render', 'Capture') {
    $root = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\$flow"
    foreach ($ep in Get-ChildItem $root -ErrorAction SilentlyContinue) {
        $props = Get-ItemProperty (Join-Path $ep.PSPath 'Properties') -ErrorAction SilentlyContinue
        if (-not $props) { continue }
        $name = $props.$nameKey
        if ($name -notmatch $Match) { continue }
        $state = (Get-ItemProperty $ep.PSPath).DeviceState
        $b = $props.$formatKey
        if (-not $b) { Write-Host "$flow  $name  state ${state}: no stored format"; continue }
        $tag = [BitConverter]::ToUInt16($b, 8); $ch = [BitConverter]::ToUInt16($b, 10)
        $rate = [BitConverter]::ToUInt32($b, 12); $bits = [BitConverter]::ToUInt16($b, 22)
        $kind = "tag 0x{0:X4}" -f $tag
        if ($tag -eq 0xFFFE -and $b.Length -ge 8 + 40) {
            $valid = [BitConverter]::ToUInt16($b, 8 + 18)
            $sub = [guid][byte[]]$b[(8 + 24)..(8 + 39)]
            $kind = if ($sub -eq $float) { 'IEEE float' } elseif ($sub -eq $pcm) { "PCM ($valid valid bits)" } else { "subformat $sub" }
        } elseif ($tag -eq 3) { $kind = 'IEEE float' } elseif ($tag -eq 1) { $kind = 'PCM' }
        Write-Host "$flow  $name  state $state : $rate Hz, $ch ch, $bits-bit $kind"
    }
}
