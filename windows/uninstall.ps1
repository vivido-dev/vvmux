[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$installRoot = Join-Path $env:LOCALAPPDATA 'Programs\vvmux'
$bin = Join-Path $installRoot 'bin'
$executable = Join-Path $bin 'vvmux.exe'

if (Test-Path -LiteralPath $executable -PathType Leaf) {
    $sessions = @(& $executable list 2>$null)
    if ($LASTEXITCODE -ne 0) {
        throw 'Unable to verify whether live vvmux sessions exist; uninstall was refused.'
    }
    if ($sessions.Count -ne 0) {
        throw "Live vvmux sessions exist. Run 'vvmux kill-session -t NAME' for each session first."
    }
}

$environmentKey = 'HKCU:\Environment'
$current = (Get-ItemProperty -Path $environmentKey -Name Path -ErrorAction SilentlyContinue).Path
$normalizedBin = $bin.TrimEnd('\')
$entries = @($current -split ';' | Where-Object {
    -not [string]::IsNullOrWhiteSpace($_) -and
    -not $_.TrimEnd('\').Equals($normalizedBin, [StringComparison]::OrdinalIgnoreCase)
})
New-ItemProperty -Path $environmentKey -Name Path -Value ($entries -join ';') -PropertyType ExpandString -Force | Out-Null

if (Test-Path -LiteralPath $executable -PathType Leaf) {
    Remove-Item -LiteralPath $executable
}

Write-Host 'Uninstalled vvmux. User configuration under %APPDATA%\vvmux was preserved.'
