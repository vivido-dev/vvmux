#![cfg(unix)]

//! The automation contract a caller reads before it acts: what `capabilities` promises, what a
//! pane inherits, and what a subscriber can replay.
//!
//! These run against a real detached session rather than against the tables directly. The unit
//! tests already prove `METHOD_CAPABILITIES` agrees with the wire enum; what they cannot prove is
//! that a live server serves it, that a pane's environment is what the launcher intended, or that
//! an event survives into the journal a subscriber replays from.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output, Stdio};

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

struct Fixture {
    binary: &'static str,
    name: String,
    _directory: tempfile::TempDir,
    _guard: SessionGuard,
}

impl Fixture {
    /// A detached session whose daemon was started with an outer Vivido identity in scope.
    ///
    /// The identity is set deliberately: the scrub is only observable if there was something to
    /// scrub, and a fixture that never sets it would pass no matter what the launcher did.
    fn start(label: &str) -> Self {
        let binary = env!("CARGO_BIN_EXE_vvmux");
        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("fixture-shell");
        fs::write(
            &shell,
            br#"#!/bin/sh
printf 'ENV pane=[%s] socket=[%s] window=[%s] session=[%s]\n' \
    "$VVMUX_PANE_ID" "$VIVIDO_SOCKET" "$VIVIDO_WINDOW_ID" "$VIVIDO_SESSION"
while IFS= read -r line; do :; done
"#,
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();

        let config = directory.path().join("vvmux.toml");
        fs::write(
            &config,
            format!(
                // Plugins off: `subscribe` and `session.started` are session facts, so they must
                // hold with the plugin system out of the picture entirely.
                "[general]\nshell = {}\nrender_interval_ms = 1\n[plugins]\nenabled = false\n",
                serde_json::to_string(shell.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();

        let name = format!("contract-{label}-{}", std::process::id());
        let created = Command::new(binary)
            .args([
                "--config",
                config.to_str().unwrap(),
                "new",
                "-s",
                &name,
                "-d",
            ])
            .env("VIVIDO_SOCKET", "/tmp/does-not-exist-vivido.sock")
            .env("VIVIDO_WINDOW_ID", "4242")
            .env("VIVIDO_SESSION", "outer-window")
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "session did not start: {}",
            String::from_utf8_lossy(&created.stderr)
        );

        let fixture = Fixture {
            binary,
            name: name.clone(),
            _directory: directory,
            _guard: SessionGuard { binary, name },
        };
        let ready = fixture.msg(&[
            "wait",
            "text",
            "ENV pane=",
            "--pane-id",
            "1",
            "--timeout",
            "5s",
        ]);
        assert!(
            ready.status.success(),
            "pane never printed its environment: {}",
            String::from_utf8_lossy(&ready.stderr)
        );
        fixture
    }

    fn msg(&self, arguments: &[&str]) -> Output {
        Command::new(self.binary)
            .args(["msg", "--target", &self.name])
            .args(arguments)
            .output()
            .unwrap()
    }

    fn json(&self, arguments: &[&str]) -> Value {
        let output = self.msg(arguments);
        assert!(
            output.status.success(),
            "{arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{arguments:?} returned invalid JSON ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }
}

#[test]
fn capabilities_classify_every_advertised_method() {
    let fixture = Fixture::start("capabilities");
    let capabilities = fixture.json(&["capabilities"]);

    let methods = capabilities["methods"].as_array().unwrap();
    let classified = capabilities["method_capabilities"].as_array().unwrap();
    assert_eq!(
        methods.len(),
        classified.len(),
        "every advertised method must carry a class"
    );

    // The two the hand-written list got wrong: a CLI spelling that was never on the wire, and a
    // method that was served but never advertised.
    assert!(methods.iter().any(|method| method == "session_snapshot"));
    assert!(methods.iter().any(|method| method == "plugin"));
    assert!(
        !methods.iter().any(|method| method == "snapshot"),
        "`snapshot` is the CLI spelling, not a wire method"
    );

    let entry = |name: &str| {
        classified
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not advertised"))
            .clone()
    };
    // The distinction the whole table exists to serve: a read-only pass may run this one and must
    // skip that one.
    assert_eq!(entry("get_text")["class"], "observe");
    assert_eq!(entry("get_text")["mutating"], false);
    assert_eq!(entry("typing")["class"], "input");
    assert_eq!(entry("typing")["mutating"], true);
    assert_eq!(entry("close_pane")["class"], "pane");
    assert_eq!(entry("reload_config")["class"], "config");
    // Observation in intent, but it scrolls the agent's viewport to get there.
    assert_eq!(entry("agent_read")["mutating"], true);
    for classified_entry in classified {
        assert_eq!(
            classified_entry["mutating"],
            Value::Bool(classified_entry["class"] != "observe"),
            "{classified_entry} disagrees with its own class"
        );
    }

    let codes = capabilities["error_codes"].as_array().unwrap();
    assert!(codes.iter().any(|code| code == "pane_not_found"));
    assert!(codes.iter().any(|code| code == "invalid_params"));

    let events = capabilities["event_kinds"].as_array().unwrap();
    assert!(events.iter().any(|event| event == "session.started"));
    assert!(events.iter().any(|event| event == "agent.status_changed"));
    // Advertised even though this session runs with plugins disabled: the name is part of the
    // protocol, and whether a plugin is installed to emit it is a separate question.
    assert!(events.iter().any(|event| event == "plugin.job_completed"));
}

#[test]
fn get_config_reports_the_configuration_in_force() {
    let fixture = Fixture::start("get-config");
    let effective = fixture.json(&["get-config"]);

    assert!(
        effective["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("vvmux.toml")),
        "get-config must name the file it resolved: {effective}"
    );
    // What this session is running with, which is the question `reload-config` cannot answer.
    assert_eq!(effective["config"]["plugins"]["enabled"], false);
    assert_eq!(effective["config"]["general"]["render_interval_ms"], 1);
    // Defaults are reported too: a caller asking what is in force wants the whole answer, not only
    // the keys the file happened to name.
    assert!(effective["config"]["session"]["auto_snapshot"].is_boolean());
    assert!(effective["config"]["panes"].is_object());
}

#[test]
fn session_started_replays_without_plugins() {
    let fixture = Fixture::start("started");

    // Replay from the beginning rather than waiting for a live event: `session.started` fires once,
    // before any subscriber can exist, so the journal is the only place it can be observed.
    let mut streaming = Command::new(fixture.binary)
        .args([
            "msg",
            "--target",
            &fixture.name,
            "subscribe",
            "--after",
            "0",
            "--name",
            "session.started",
        ])
        .stdout(Stdio::piped())
        // The subscriber is killed below, which closes its socket mid-read.
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Read on a worker with a deadline. A regression here means the event is never published, and
    // a blocking read would hang the suite instead of reporting that.
    let (sender, receiver) = std::sync::mpsc::channel();
    let stdout = streaming.stdout.take().unwrap();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        let mut lines = std::io::BufReader::new(stdout).lines();
        let _ = sender.send(lines.next().and_then(Result::ok));
    });
    let line = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("no session.started event was replayed within 10s")
        .expect("the subscriber closed without emitting an event");
    let _ = streaming.kill();
    let _ = streaming.wait();
    let event: Value = serde_json::from_str(&line).unwrap();

    assert_eq!(event["type"], "event");
    assert_eq!(event["name"], "session.started");
    assert_eq!(
        event["payload"]["restored"], false,
        "a freshly created session did not come from a snapshot"
    );
}

#[test]
fn panes_do_not_inherit_the_outer_vivido_identity() {
    let fixture = Fixture::start("scrub");
    // `get-text` prints terminal text, not JSON.
    let output = fixture.msg(&["get-text", "--pane-id", "1", "--source", "recent"]);
    assert!(output.status.success());
    let printed = String::from_utf8_lossy(&output.stdout).into_owned();

    // The daemon was started with all three set, and the pane must have seen none of them: they
    // name the Vivido window that launched the daemon, which the daemon outlives. A pane agent
    // acting on a stale `VIVIDO_SOCKET` would drive somebody else's terminal.
    assert!(
        printed.contains("socket=[] window=[] session=[]"),
        "pane inherited an outer Vivido identity: {printed}"
    );
    // Guards against a vacuous pass: the pane environment is otherwise populated, so the empty
    // fields above are the scrub rather than a shell that saw no environment at all.
    assert!(
        printed.contains("pane=[1]"),
        "pane environment was not populated at all: {printed}"
    );
}
