# VVMUX_INTEGRATION_ID=claude
# VVMUX_INTEGRATION_VERSION=1
# Managed by vvmux; reinstalling may overwrite this file.

param([string]$Action = "")
if ($Action -ne "session") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:VVMUX_BIN)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:VVMUX_SESSION)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:VVMUX_PANE_ID)) { exit 0 }
try { $payload = [Console]::In.ReadToEnd() | ConvertFrom-Json } catch { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($payload.agent_id)) { exit 0 }
if ($payload.hook_event_name -eq "SubagentStop") { exit 0 }
if ([string]::IsNullOrWhiteSpace($payload.session_id)) { exit 0 }
$arguments = @("msg", "--target", $env:VVMUX_SESSION, "report-agent-session",
    "--agent", "claude", "--source", "vvmux:claude", "--sequence",
    ([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() * 1000000),
    "--agent-session-id", $payload.session_id)
if (-not [string]::IsNullOrWhiteSpace($payload.transcript_path)) {
    $arguments += @("--agent-session-path", $payload.transcript_path)
}
$arguments += @("--pane-id", $env:VVMUX_PANE_ID)
try {
    $process = Start-Process -FilePath $env:VVMUX_BIN -ArgumentList $arguments -PassThru -WindowStyle Hidden
    if (-not $process.WaitForExit(500)) { $process.Kill() }
} catch {}
exit 0
