<#
.SYNOPSIS
Windows soak gates for vvmux: console restoration, detached lifecycle, detached media
production, and (on a provisioned host) multi-user pipe admission.

.EXAMPLE
.\windows	est-soak.ps1
Quick local pass: 1,000 lifecycle iterations and a 60-second media run.

.EXAMPLE
.\windows	est-soak.ps1 -Gate media -MediaSeconds 3600
The full one-hour media soak.
#>
[CmdletBinding()]
param(
    [ValidateSet('console', 'lifecycle', 'media', 'multi-user')]
    [string[]]$Gate = @('console', 'lifecycle', 'media'),
    [ValidateRange(1, 100000)]
    [int]$Iterations = 1000,
    [ValidateRange(1, 86400)]
    [int]$MediaSeconds = 60,
    [switch]$NoBuild
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$executable = Join-Path $root 'target\debug\vvmux.exe'
$run = "$PID-$([DateTime]::UtcNow.ToString('yyyyMMddHHmmss'))"

function Invoke-Cargo {
    param([string[]]$Arguments)
    Push-Location $root
    try {
        & cargo @Arguments
        if ($LASTEXITCODE -ne 0) { throw "cargo $($Arguments -join ' ') failed" }
    } finally {
        Pop-Location
    }
}

function Test-ConsoleRestoration {
    & $executable __console-self-test
    if ($LASTEXITCODE -ne 0) { throw 'allocated-console unwind restoration failed' }
}

function Get-SessionNames {
    # `list` prints one `name<TAB>pid N` line per session; compare names exactly.
    @(& $executable list | ForEach-Object { ($_ -split "`t")[0] })
}

function Get-VvmuxPipes {
    @(Get-ChildItem '\\.\pipe\' | Where-Object Name -Like 'vvmux-*' | ForEach-Object Name)
}

function Get-SoakServers {
    param([string]$Prefix)
    @(Get-CimInstance Win32_Process -Filter "Name='vvmux.exe'" |
        Where-Object { $_.CommandLine -match "__server --session $([regex]::Escape($Prefix))" })
}

function Test-LifecycleSoak {
    # Each session is its own `__server` process, so leaks show up as surviving
    # servers or named pipes rather than as growth in this driver process.
    $baselinePipes = Get-VvmuxPipes
    for ($iteration = 0; $iteration -lt $Iterations; $iteration++) {
        $name = "soak-$run-$iteration"
        & $executable new -d -s $name | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "create failed at $iteration" }
        & $executable kill-session -t $name
        if ($LASTEXITCODE -ne 0) { throw "kill failed at $iteration" }
        if (($iteration + 1) % 100 -eq 0) { Write-Host "   $($iteration + 1)/$Iterations sessions" }
    }
    Start-Sleep -Seconds 2
    $left = @(Get-SessionNames | Where-Object { $_.StartsWith("soak-$run-") })
    if ($left) { throw "sessions remain after soak: $left" }
    $servers = Get-SoakServers "soak-$run-"
    if ($servers) { throw "server processes remain after soak: $($servers.ProcessId -join ', ')" }
    $leakedPipes = @(Get-VvmuxPipes | Where-Object { $_ -notin $baselinePipes })
    if ($leakedPipes) { throw "named pipes remain after soak: $($leakedPipes -join ', ')" }
}

function Test-DetachedMediaSoak {
    $name = "media-$run"
    $producer = (Resolve-Path (Join-Path $root 'target\debug\examples\detached_media_soak.exe')).Path
    $config = Join-Path ([IO.Path]::GetTempPath()) "vvmux-media-soak-$run.toml"
    "[general]`nshell = '$producer'" | Set-Content -LiteralPath $config -Encoding utf8NoBOM
    $previousSeconds = $env:VVMUX_MEDIA_SOAK_SECONDS
    $env:VVMUX_MEDIA_SOAK_SECONDS = "$MediaSeconds"
    Write-Host "   producing for $MediaSeconds s; expect the session to end by $((Get-Date).AddSeconds($MediaSeconds).ToString('T'))"
    try {
        $started = [Diagnostics.Stopwatch]::StartNew()
        & $executable --config $config new -d -s $name | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'detached media session failed to start' }
        $reported = 0
        while ($started.Elapsed.TotalSeconds -lt $MediaSeconds + 90) {
            if ($name -notin (Get-SessionNames)) { break }
            $elapsed = [int]$started.Elapsed.TotalSeconds
            if ($elapsed -ge $reported + 30) {
                $reported = $elapsed
                Write-Host "   ${elapsed}/${MediaSeconds} s, session alive"
            }
            Start-Sleep -Seconds 5
        }
        if ($started.Elapsed.TotalSeconds -lt [Math]::Max($MediaSeconds - 10, $MediaSeconds / 2)) {
            throw "media producer exited before $MediaSeconds seconds"
        }
        if ($name -in (Get-SessionNames)) {
            throw 'media session did not reclaim after producer exit'
        }
    } finally {
        # Also reached on Ctrl+C or a failure, which would otherwise leave the producer running.
        if ($name -in (Get-SessionNames)) { & $executable kill-session -t $name 2>$null }
        $env:VVMUX_MEDIA_SOAK_SECONDS = $previousSeconds
        Remove-Item -LiteralPath $config -ErrorAction SilentlyContinue
    }
}

if (-not $NoBuild) {
    if ($Gate -contains 'media') {
        Invoke-Cargo @('build', '--locked', '--bins', '--example', 'detached_media_soak')
    } else {
        Invoke-Cargo @('build', '--locked')
    }
}

foreach ($name in $Gate) {
    Write-Host "== $name"
    switch ($name) {
        'console' { Test-ConsoleRestoration }
        'lifecycle' { Test-LifecycleSoak }
        'media' { Test-DetachedMediaSoak }
        'multi-user' { & (Join-Path $PSScriptRoot 'test-multi-user.ps1') }
    }
}
Write-Host 'all requested gates passed'
