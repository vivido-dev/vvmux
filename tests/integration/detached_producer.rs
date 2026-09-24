//! A Vivid producer in a session no client has ever attached to must still be admitted.
//!
//! A detached session has never seen a display, so its panes had no presentation target until
//! the first attach, and the inner presenter refused every producer's hello as "not ready". The
//! producer here is this test binary, re-executed inside a pane opened by `msg run`.

use crate::common;

use std::fs;
use std::process::Output;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const ADMITTED: &str = "PRODUCER-ADMITTED";
const REFUSED: &str = "PRODUCER-REFUSED";

/// Runs only inside a vvmux pane, where the pane's Vivid capability is in the environment.
#[test]
#[ignore = "re-executed inside a pane by a_detached_session_admits_a_vivid_producer"]
fn producer_child() {
    if std::env::var_os("VIVID_ENDPOINT_CONTROL").is_none() {
        return;
    }
    match vivid_sdk::Session::connect(vivid_sdk::ProducerConfig::default()) {
        Ok(_) => println!("{ADMITTED}"),
        Err(error) => println!("{REFUSED}: {error}"),
    }
}

struct SessionGuard<'a> {
    runtime: &'a std::path::Path,
    name: String,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let _ = common::vvmux_command(self.runtime)
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

#[test]
fn a_detached_session_admits_a_vivid_producer() {
    // Short root: on Unix the runtime directory holds the session socket, whose path must stay
    // inside `sun_path`.
    #[cfg(unix)]
    let directory = {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::Builder::new()
            .prefix("vvp-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    };
    #[cfg(windows)]
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path();

    // Pin the shell so the command line below does not depend on the developer's own shell.
    // Not cmd.exe on Windows: it cannot quote an absolute path inside `/C`, and it refuses the
    // verbatim working directory vvmux hands a pane.
    #[cfg(unix)]
    let shell = std::path::PathBuf::from("/bin/sh");
    #[cfg(windows)]
    let shell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let config = runtime.join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = {}\n",
            serde_json::to_string(shell.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();

    let name = format!(
        "detached-producer-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    assert_success(
        &common::vvmux_command(runtime)
            .args([
                "--config",
                config.to_str().unwrap(),
                "new",
                "-d",
                "-s",
                &name,
            ])
            .output()
            .unwrap(),
    );
    let _guard = SessionGuard {
        runtime,
        name: name.clone(),
    };

    // Both shells take a single-quoted path; only the escape for a quote inside it differs.
    let executable = std::env::current_exe().unwrap();
    let executable = executable.to_str().unwrap();
    #[cfg(unix)]
    let program = format!("'{}'", executable.replace('\'', r"'\''"));
    #[cfg(windows)]
    let program = format!("& '{}'", executable.replace('\'', "''"));
    let command =
        format!("{program} --exact detached_producer::producer_child --ignored --nocapture");
    let opened = json(message(
        runtime,
        &name,
        &["run", &command, "--hold", "--pane-id", "1"],
    ));
    let pane = opened["pane_id"].as_u64().unwrap().to_string();

    let deadline = Instant::now() + Duration::from_secs(30);
    let text = loop {
        let output = message(runtime, &name, &["get-text", "--pane-id", &pane]);
        assert_success(&output);
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if text.contains(ADMITTED) || text.contains(REFUSED) {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "the producer never reported; pane text:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        text.contains(ADMITTED),
        "a detached session refused its producer:\n{text}"
    );
}

fn message(runtime: &std::path::Path, session: &str, arguments: &[&str]) -> Output {
    common::vvmux_command(runtime)
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
