[CmdletBinding()]
param(
    [string]$SecondUser = $env:VVMUX_SECOND_USER,
    [string]$SecondUserPassword = $env:VVMUX_SECOND_USER_PASSWORD
)

$ErrorActionPreference = 'Stop'
if (-not $SecondUser -or -not $SecondUserPassword) {
    throw 'VVMUX_SECOND_USER and VVMUX_SECOND_USER_PASSWORD are required on the provisioned runner.'
}

$executable = (Resolve-Path (Join-Path $PSScriptRoot '..\target\debug\vvmux.exe')).Path
$before = @(Get-ChildItem '\.\pipe\' | Where-Object Name -Like 'vvmux-*' | ForEach-Object Name)
$session = "multi-user-$PID"
try {
    & $executable new -d -s $session
    if ($LASTEXITCODE -ne 0) { throw 'could not create test session' }
    $after = @(Get-ChildItem '\.\pipe\' | Where-Object Name -Like 'vvmux-*' | ForEach-Object Name)
    $pipe = @($after | Where-Object { $_ -notin $before }) | Select-Object -First 1
    if (-not $pipe) { throw 'could not identify the owner pipe' }

    $escapedPipe = $pipe.Replace("'", "''")
    $probe = "try { `$p = [IO.Pipes.NamedPipeClientStream]::new('.', '$escapedPipe', [IO.Pipes.PipeDirection]::InOut); `$p.Connect(1000); exit 9 } catch { exit 0 }"
    $securePassword = ConvertTo-SecureString $SecondUserPassword -AsPlainText -Force
    $credential = [PSCredential]::new($SecondUser, $securePassword)
    $process = Start-Process powershell.exe -Credential $credential -WindowStyle Hidden -Wait -PassThru -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', $probe)
    if ($process.ExitCode -ne 0) { throw 'a second local user connected to the owner pipe' }

    try {
        $remote = [IO.Pipes.NamedPipeClientStream]::new($env:COMPUTERNAME, $pipe, [IO.Pipes.PipeDirection]::InOut)
        $remote.Connect(1000)
        throw 'a remote-style named-pipe connection was accepted'
    } catch [TimeoutException], [UnauthorizedAccessException], [IO.IOException] {
        # Expected: PIPE_REJECT_REMOTE_CLIENTS or the owner DACL rejects the connection.
    }
} finally {
    & $executable kill-session -t $session 2>$null
}
