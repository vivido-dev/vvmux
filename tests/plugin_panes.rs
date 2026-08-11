#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

struct SessionGuard {
    binary: &'static str,
    name: String,
    runtime: PathBuf,
    config_home: PathBuf,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = command(self.binary, &self.runtime, &self.config_home)
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

#[test]
fn plugin_panes_use_real_pty_placement_identity_vivid_and_owner_scoped_cleanup() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);
    let config = write_config(directory.path());
    let package = write_package(directory.path());
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "link", package.to_str().unwrap(), "--yes"])
            .output()
            .unwrap(),
    );

    let suffix = std::process::id();
    let first = format!("plugin-pane-a-{suffix}");
    let second = format!("plugin-pane-b-{suffix}");
    start_session(binary, &runtime, &config_home, &config, &first);
    start_session(binary, &runtime, &config_home, &config, &second);
    let _first_guard = SessionGuard {
        binary,
        name: first.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };
    let _second_guard = SessionGuard {
        binary,
        name: second.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };

    let first_split = open(binary, &runtime, &config_home, &first, "split");
    let second_split = open(binary, &runtime, &config_home, &second, "split");
    assert_eq!(first_split["pane_id"], 2);
    assert_eq!(second_split["pane_id"], 2, "pane IDs are owner-local");
    assert_ne!(
        first_split["plugin_instance"], second_split["plugin_instance"],
        "each pane gets an exact, collision-resistant plugin instance"
    );
    wait_text(binary, &runtime, &config_home, &first, 2, "PLUGIN split");
    wait_text(binary, &runtime, &config_home, &second, 2, "PLUGIN split");

    let first_inspect = inspect(binary, &runtime, &config_home, &first, 2);
    assert_eq!(first_inspect["pane"]["layer"], "tiled");
    assert_eq!(first_inspect["pane"]["title"], "Split pane");
    assert_eq!(first_inspect["pane"]["plugin"]["plugin_id"], "dev.panes");
    assert_eq!(
        first_inspect["pane"]["plugin"]["plugin_instance"],
        first_split["plugin_instance"]
    );
    assert_eq!(first_inspect["pane"]["plugin"]["accept_sync_input"], false);
    assert_eq!(first_split["vivid"], true);

    let floating = open(binary, &runtime, &config_home, &first, "floating");
    wait_text(
        binary,
        &runtime,
        &config_home,
        &first,
        floating["pane_id"].as_u64().unwrap(),
        "PLUGIN floating",
    );
    assert_eq!(
        inspect(
            binary,
            &runtime,
            &config_home,
            &first,
            floating["pane_id"].as_u64().unwrap(),
        )["pane"]["layer"],
        "floating"
    );

    let tab = open(binary, &runtime, &config_home, &first, "tabbed");
    assert_ne!(tab["tab_id"], first_split["tab_id"]);
    let tab_inspect = inspect(
        binary,
        &runtime,
        &config_home,
        &first,
        tab["pane_id"].as_u64().unwrap(),
    );
    assert_eq!(tab_inspect["pane"]["tab_name"], "Plugin tab");

    let exited = open(binary, &runtime, &config_home, &first, "exit");
    let exited_id = exited["pane_id"].as_u64().unwrap();
    wait_text(
        binary,
        &runtime,
        &config_home,
        &first,
        exited_id,
        "plugin dev.panes/exit exited 9",
    );
    assert_eq!(
        inspect(binary, &runtime, &config_home, &first, exited_id)["pane"]["process_state"],
        "exited"
    );

    // Closing owner A's numeric pane 2 cannot affect owner B's pane 2 or its next valid update.
    assert_success(&msg(
        binary,
        &runtime,
        &config_home,
        &first,
        &["close-pane", "--pane-id", "2"],
    ));
    assert_eq!(
        inspect(binary, &runtime, &config_home, &second, 2)["pane"]["plugin"]["plugin_id"],
        "dev.panes"
    );
    assert_success(&msg(
        binary,
        &runtime,
        &config_home,
        &second,
        &["typing", "still-live\n", "--pane-id", "2"],
    ));
    wait_text(
        binary,
        &runtime,
        &config_home,
        &second,
        2,
        "INPUT still-live",
    );

    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "disable", "dev.panes"])
            .output()
            .unwrap(),
    );
    assert_pane_missing(binary, &runtime, &config_home, &first, exited_id);
    assert_pane_missing(binary, &runtime, &config_home, &second, 2);
}

fn write_package(root: &Path) -> PathBuf {
    let package = root.join("plugin-panes");
    fs::create_dir(&package).unwrap();
    let script = package.join("pane.sh");
    fs::write(
        &script,
        br#"#!/bin/sh
label=$1
printf 'PLUGIN %s id=%s instance=%s vivid=%s\r\n' "$label" "$VVMUX_PLUGIN_ID" "$VVMUX_PLUGIN_INSTANCE" "${VIVID_ROOT_SECRET:+yes}"
if test "$label" = exit; then exit 9; fi
while IFS= read -r line; do printf 'INPUT %s\r\n' "$line"; done
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        package.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.panes"
name = "Pane fixture"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Plugin PTY pane fixture"
platforms = ["linux", "macos"]
permissions = ["pane.create", "pane.manage_own", "media.produce"]

[[panes]]
id = "split"
title = "Split pane"
placement = "split"
command = ["./pane.sh", "split"]

[[panes]]
id = "floating"
title = "Floating pane"
placement = "float"
command = ["./pane.sh", "floating"]

[[panes]]
id = "tabbed"
title = "Plugin tab"
placement = "tab"
command = ["./pane.sh", "tabbed"]
accept_sync_input = true

[[panes]]
id = "exit"
title = "Exit diagnostic"
placement = "float"
command = ["./pane.sh", "exit"]
"#,
    )
    .unwrap();
    package
}

fn write_config(root: &Path) -> PathBuf {
    let shell = root.join("shell.sh");
    fs::write(
        &shell,
        b"#!/bin/sh\nprintf 'READY\\r\\n'\nwhile :; do sleep 60; done\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = {}\nrender_interval_ms = 1\n[plugins]\nenabled = true\n",
            serde_json::to_string(shell.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    config
}

fn start_session(binary: &str, runtime: &Path, config_home: &Path, config: &Path, name: &str) {
    let output = command(binary, runtime, config_home)
        .args([
            "--config",
            config.to_str().unwrap(),
            "new",
            "-s",
            name,
            "-d",
        ])
        .output()
        .unwrap();
    assert_success(&output);
}

fn open(binary: &str, runtime: &Path, config_home: &Path, session: &str, pane: &str) -> Value {
    json(
        command(binary, runtime, config_home)
            .args([
                "plugin",
                "pane",
                "open",
                &format!("dev.panes/{pane}"),
                "--target",
                session,
            ])
            .output()
            .unwrap(),
    )
}

fn inspect(binary: &str, runtime: &Path, config_home: &Path, session: &str, pane: u64) -> Value {
    json(msg(
        binary,
        runtime,
        config_home,
        session,
        &["inspect", "--pane-id", &pane.to_string()],
    ))
}

fn wait_text(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    pane: u64,
    text: &str,
) {
    let output = msg(
        binary,
        runtime,
        config_home,
        session,
        &[
            "wait",
            "text",
            text,
            "--pane-id",
            &pane.to_string(),
            "--timeout",
            "5s",
        ],
    );
    if !output.status.success() {
        let snapshot = msg(
            binary,
            runtime,
            config_home,
            session,
            &["get-text", "--pane-id", &pane.to_string()],
        );
        panic!(
            "pane {pane} did not contain {text:?}: wait stderr={} snapshot stdout={} snapshot stderr={}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&snapshot.stdout),
            String::from_utf8_lossy(&snapshot.stderr),
        );
    }
}

fn assert_pane_missing(binary: &str, runtime: &Path, config_home: &Path, session: &str, pane: u64) {
    let output = msg(
        binary,
        runtime,
        config_home,
        session,
        &["inspect", "--pane-id", &pane.to_string()],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("pane_not_found"));
}

fn msg(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    arguments: &[&str],
) -> Output {
    command(binary, runtime, config_home)
        .args(["msg", "--target", session])
        .args(arguments)
        .output()
        .unwrap()
}

fn command(binary: &str, runtime: &Path, config_home: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", config_home)
        .env_remove("VIVID_ENDPOINT")
        .env_remove("VIVID_TOKEN")
        .env_remove("VIVID_ROOT_SECRET");
    command
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn json(output: Output) -> Value {
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
