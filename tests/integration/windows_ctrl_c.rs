#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::windows_ctrl_c_support::{Console, PROMPT};

struct SessionGuard {
    executable: PathBuf,
    name: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = Command::new(&self.executable)
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

#[test]
fn ctrl_c_interrupts_pane_process_through_attached_session() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    let shell =
        std::env::var_os("COMSPEC").unwrap_or_else(|| "C:\\Windows\\System32\\cmd.exe".into());
    // The test drives cmd syntax; do not inherit the user's shell or plugin configuration.
    std::fs::write(
        &config,
        format!(
            "[general]\nshell = {}\n[plugins]\nenabled = false\n",
            serde_json::to_string(&shell.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
    let mut console = Console::spawn(directory.path());
    let session = SessionGuard {
        executable,
        name: format!(
            "ctrlprobe-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ),
    };
    assert!(
        console.wait(Duration::from_secs(10), |terminal| {
            terminal.visible_text(0).contains(PROMPT)
        }),
        "no host shell prompt:\n{}",
        console.text()
    );
    console
        .input
        .send(
            format!(
                "\"{}\" --config \"{}\" new -s {}\r",
                session.executable.display(),
                config.display(),
                session.name
            )
            .as_bytes(),
        )
        .unwrap();
    assert!(
        console.wait(Duration::from_secs(15), |terminal| {
            terminal.alternate_screen() && terminal.visible_text(0).contains(PROMPT)
        }),
        "session did not reach its pane prompt:\n{}",
        console.text()
    );
    console.interrupt_ping();
}
