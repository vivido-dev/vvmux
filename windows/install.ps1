[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$source = Join-Path $PSScriptRoot 'vvmux.exe'
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
    throw "vvmux.exe is missing from the release archive"
}

$installRoot = Join-Path $env:LOCALAPPDATA 'Programs\vvmux'
$bin = Join-Path $installRoot 'bin'
$destination = Join-Path $bin 'vvmux.exe'
New-Item -ItemType Directory -Path $bin -Force | Out-Null
Copy-Item -LiteralPath $source -Destination $destination -Force

$environmentKey = 'HKCU:\Environment'
$current = (Get-ItemProperty -Path $environmentKey -Name Path -ErrorAction SilentlyContinue).Path
$entries = @($current -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
$normalizedBin = $bin.TrimEnd('\')
$present = $entries | Where-Object { $_.TrimEnd('\').Equals($normalizedBin, [StringComparison]::OrdinalIgnoreCase) }
if (-not $present) {
    $updated = (@($entries) + $bin) -join ';'
    New-ItemProperty -Path $environmentKey -Name Path -Value $updated -PropertyType ExpandString -Force | Out-Null
}

Write-Host "Installed vvmux to $destination"
Write-Host 'Open a new terminal for the user PATH update to take effect.'
