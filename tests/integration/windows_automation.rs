#![cfg(windows)]

use std::fs;
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

struct SessionGuard {
    binary: &'static str,
    name: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = Command::new(self.binary)
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

#[test]
fn windows_session_exposes_structured_ai_automation() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let name = format!(
        "automation-win-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let created = Command::new(binary)
        .args(["new", "--session", &name, "--detached"])
        .output()
        .unwrap();
    assert_success(&created);
    let _guard = SessionGuard {
        binary,
        name: name.clone(),
    };

    let deadline = Instant::now() + Duration::from_secs(10);
    let inspect = loop {
        let output = message(binary, &name, &["session-inspect"]);
        if output.status.success() {
            break json(output);
        }
        assert!(
            Instant::now() < deadline,
            "session automation never became ready"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(inspect["active_tab_id"], 1);
    assert_eq!(inspect["active_pane_id"], 1);

    let tabs = json(message(binary, &name, &["list-tabs"]));
    assert_eq!(tabs["tabs"][0]["tab_id"], 1);
    let selected = json(message(binary, &name, &["select-tab", "--tab-id", "1"]));
    assert_eq!(selected["tab_id"], 1);

    let report = json(message(
        binary,
        &name,
        &[
            "typing",
            "echo VVMUX-AUTOMATION\r",
            "--pane-id",
            "1",
            "--report",
        ],
    ));
    assert_eq!(report["pane_id"], 1);
    assert_eq!(report["pty_write_completed"], true);
    assert_eq!(report["application_consumption_observed"], false);
    assert_success(&message(
        binary,
        &name,
        &[
            "wait",
            "text",
            "VVMUX-AUTOMATION",
            "--pane-id",
            "1",
            "--timeout",
            "5s",
        ],
    ));

    let diagnose = json(message(
        binary,
        &name,
        &["diagnose", "--all-panes", "--trace-limit", "16"],
    ));
    assert_eq!(diagnose["schema_version"], 1);
    assert_eq!(diagnose["panes"][0]["pane"]["pane_id"], 1);

    let doctor = json(
        Command::new(binary)
            .args(["doctor", "--target", &name, "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(doctor["checks"]["registry_identity"], "ok");

    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("vvmux-debug.zip");
    assert_success(
        &Command::new(binary)
            .args([
                "debug-bundle",
                "--target",
                &name,
                "--output",
                bundle.to_str().unwrap(),
            ])
            .output()
            .unwrap(),
    );
    let archive = fs::read(bundle).unwrap();
    assert!(
        archive
            .windows(b"manifest.json".len())
            .any(|part| part == b"manifest.json")
    );
    assert!(
        !archive
            .windows(b"content/".len())
            .any(|part| part == b"content/")
    );
}

/// Concurrent clients race for the session pipe's single listening instance. A client that loses
/// that race sees ERROR_PIPE_BUSY and must wait for the next instance rather than fail.
#[test]
fn concurrent_clients_share_one_session_pipe() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let name = format!(
        "pipe-busy-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    assert_success(
        &Command::new(binary)
            .args(["new", "--session", &name, "--detached"])
            .output()
            .unwrap(),
    );
    let _guard = SessionGuard {
        binary,
        name: name.clone(),
    };

    std::thread::scope(|scope| {
        let workers = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    for _ in 0..10 {
                        assert_success(&message(binary, &name, &["session-inspect"]));
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
    });
}

fn message(binary: &str, session: &str, arguments: &[&str]) -> Output {
    Command::new(binary)
        .args(["msg", "--target", session])
        .args(arguments)
        .output()
        .unwrap()
}

fn json(output: Output) -> Value {
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
