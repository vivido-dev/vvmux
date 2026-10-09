//! Helpers shared by the integration tests.

// The tunnel harness serves the gateway's VVTUN protocol with axum and tokio, which exist only
// with the `server-capability` feature.
#[cfg(feature = "server-capability")]
mod tunnel;
#[cfg(feature = "server-capability")]
pub use tunnel::*;

/// Run a `vvmux` binary command with an isolated runtime directory.
///
/// `HOME` is isolated along with the XDG directories, and it is not optional. Pane shells are login
/// shells (`spawn` in `vvmux-terminal/src/pty/unix.rs`), so each one sources the invoking user's
/// `~/.profile` — and the conventional profile prepends `$HOME/bin` and `$HOME/.local/bin` to `PATH`
/// when those directories exist. A test that puts a fixture executable first on the `PATH` it passes
/// here would therefore have its ordering silently rewritten by the developer's own profile, and a
/// real binary of the same name would win instead. That failure is invisible on a machine that
/// happens not to have one installed, which is exactly how it survived unnoticed.
///
/// `XDG_STATE_HOME` is pinned for a second reason: without it, persisted session state falls back to
/// `$HOME/.local/state`, and an integration test would write snapshots into the developer's real
/// state directory.
///
/// A fresh `HOME` is also a fresh Ubuntu MOTD state: `/etc/profile.d/update-motd.sh` — sourced by
/// the same login shell for the same reason as `~/.profile` — treats an absent `$HOME/.motd_shown`
/// as "never shown" and runs every script under `/etc/update-motd.d` before the shell reaches its
/// prompt. On a host with `landscape-common` installed that costs several hundred milliseconds a
/// pane, long enough to race an immediate shell-availability check. `.hushlogin` is the same host
/// behavior every real login already has the option to suppress, so creating it here keeps a pane's
/// prompt timing independent of what happens to be installed on the machine running the test.
pub fn vvmux_command(runtime: &std::path::Path) -> std::process::Command {
    let _ = std::fs::File::create(runtime.join(".hushlogin"));
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_vvmux"));
    command.env("XDG_RUNTIME_DIR", runtime);
    command.env("XDG_CONFIG_HOME", runtime);
    // A subdirectory, not the runtime directory itself: in production these are genuinely different
    // places, and sharing one here would put persisted session state beside the runtime artifacts
    // that tests assert are cleaned up on shutdown.
    command.env("XDG_STATE_HOME", runtime.join("state"));
    command.env("HOME", runtime);
    command
}

/// Install the fixture agent providers into an isolated config directory.
///
/// vvmux ships no agent providers of its own any more — they are ordinary installed packages —
/// so a test that reports or detects an agent has to install one first. These fixtures are copies
/// of the first-party packages with `com.example.` IDs, which is what keeps a local install from
/// hitting the reserved-ID policy.
pub fn install_agent_providers(runtime: &std::path::Path, providers: &[&str]) {
    // These tests point `XDG_RUNTIME_DIR` and `XDG_CONFIG_HOME` at one directory, so installing
    // before any session exists is what first creates `<runtime>/vvmux`. It has to be owner-only
    // from the start, or the reload that follows the install refuses to scan it.
    let directory = runtime.join("vvmux");
    std::fs::create_dir_all(&directory).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    for provider in providers {
        let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-providers")
            .join(provider);
        let output = vvmux_command(runtime)
            .args(["plugin", "install"])
            .arg(&package)
            .arg("--yes")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "installing the {provider} provider failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
