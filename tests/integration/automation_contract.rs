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
        let shell = directory.path().join("sh");
        fs::write(
            &shell,
            // `-c` has to work: vvmux runs `run` and `submit` commands through this shell, and a
            // fixture that swallowed them would make every test below wait for output that was
            // never going to arrive.
            br#"#!/bin/sh
if [ "$1" = "-c" ]; then
    shift
    exec /bin/sh -c "$@"
fi
printf 'ENV pane=[%s] socket=[%s] window=[%s] session=[%s]\n' \
    "$VVMUX_PANE_ID" "$VIVIDO_SOCKET" "$VIVIDO_WINDOW_ID" "$VIVIDO_SESSION"
exec /bin/sh
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

/// A signal reaches the job holding the terminal, which typed input cannot promise.
#[test]
fn signal_reaches_the_foreground_job_and_reports_the_exit() {
    let fixture = Fixture::start("signal");
    // `--hold` keeps the pane open after the process dies, so its exit is still readable.
    let opened = fixture.json(&["run", "sleep 300", "--hold"]);
    let pane = opened["pane_id"].as_u64().unwrap().to_string();
    // Wait for the child to exist before signalling it; `run` returns as soon as the PTY is up.
    for _ in 0..50 {
        let inspected = fixture.json(&["inspect", "--pane-id", &pane]);
        if inspected["pane"]["process"]["pid"].as_u64().unwrap_or(0) != 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let signalled = fixture.json(&["signal", "INT", "--pane-id", &pane]);
    assert_eq!(signalled["signal"], "INT");
    assert!(signalled["process_group"].as_u64().unwrap_or(0) > 0);

    for _ in 0..100 {
        let inspected = fixture.json(&["inspect", "--pane-id", &pane]);
        if inspected["pane"]["process_state"] == "exited" {
            // The exit carries the signal, not an exit code: a caller can tell "I killed it" from
            // "it finished".
            assert_eq!(
                inspected["pane"]["process"]["exit"]["signal"], 2,
                "{inspected}"
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the pane never exited after SIGINT");
}

/// The gap `get-text` cannot close: output the grid has already overwritten.
#[test]
fn transcript_and_wait_output_see_what_the_screen_lost() {
    let fixture = Fixture::start("transcript");
    let before = fixture.json(&["inspect", "--pane-id", "1"])["pane"]["output_offset"]
        .as_u64()
        .unwrap();

    // A carriage return, not a newline: the marker never survives on screen, because `DONE` is
    // written over it in the same line. The marker is assembled at runtime so it does not appear
    // in the echoed command either — otherwise the screen would still contain it and the
    // comparison below would prove nothing.
    assert!(
        fixture
            .msg(&[
                "submit",
                "--pane-id",
                "1",
                r#"A=fla; B=sh; printf '%s%s\rDONE\n' "$A" "$B""#,
            ])
            .status
            .success()
    );

    let matched = fixture.json(&[
        "wait",
        "output",
        "flash",
        "--pane-id",
        "1",
        "--after-offset",
        &before.to_string(),
        "--timeout",
        "5s",
    ]);
    assert!(matched["output_offset"].as_u64().unwrap() > before);

    let transcript = fixture.json(&[
        "transcript",
        "--pane-id",
        "1",
        "--after-offset",
        &before.to_string(),
    ]);
    let text = transcript["text"].as_str().unwrap();
    assert!(
        text.contains("flash"),
        "transcript lost the output: {text:?}"
    );
    assert!(text.contains("DONE"));
    assert!(transcript["dropped_before_offset"].is_null());

    // The screen, meanwhile, has only what survived the overwrite. This is the whole reason the
    // transcript exists, so the test says so rather than assuming it.
    let output = fixture.msg(&["get-text", "--pane-id", "1", "--source", "recent"]);
    let visible = String::from_utf8_lossy(&output.stdout);
    assert!(
        !visible.contains("flash"),
        "the screen still had the overwritten text, so this proves nothing: {visible:?}"
    );

    // An offset that has scrolled out of the window is reported, never silently shortened.
    let gapped = fixture.json(&["transcript", "--pane-id", "1", "--after-offset", "0"]);
    assert!(gapped["retained_from_offset"].is_number());
}

/// Explicit targeting is what lets automation drive a pane nobody is looking at.
#[test]
fn mouse_encodes_pane_local_cells_for_a_pane_that_is_not_visible() {
    let fixture = Fixture::start("mouse");
    assert!(
        fixture
            .msg(&["split", "vertical", "--pane-id", "1"])
            .status
            .success()
    );
    // Put the target on a tab that is not selected, so a hit-tested implementation would miss it.
    let scratch = fixture.json(&["new-tab", "--name", "scratch"]);
    assert!(
        fixture
            .msg(&["select-tab", "--tab-name", "scratch"])
            .status
            .success()
    );
    assert!(scratch["tab_id"].is_number());

    // A reader that enables SGR mouse reporting and echoes what it receives.
    assert!(
        fixture
            .msg(&[
                "submit",
                "--pane-id",
                "2",
                r#"printf '\033[?1000h\033[?1006h'; cat -v"#,
            ])
            .status
            .success()
    );
    let mut before = None;
    for _ in 0..100 {
        let inspected = fixture.json(&["inspect", "--pane-id", "2"]);
        if inspected["pane"]["modes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|mode| mode == "mouse_clicks")
        {
            before = inspected["pane"]["output_offset"].as_u64();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let before = before.expect("the pane never enabled mouse reporting");

    let clicked = fixture.json(&[
        "mouse",
        "click",
        "--cell-column",
        "5",
        "--cell-row",
        "2",
        "--pane-id",
        "2",
    ]);
    assert_eq!(clicked["events"], 2, "a click is a press and a release");
    assert_eq!(clicked["cell"]["column"], 5);
    assert_eq!(clicked["cell"]["row"], 2);

    assert!(
        fixture
            .msg(&[
                "wait",
                "output",
                r"\[<0;6;3M",
                "--regex",
                "--pane-id",
                "2",
                "--after-offset",
                &before.to_string(),
                "--timeout",
                "5s",
            ])
            .status
            .success(),
        "the pane never received a press at its own cell 5,2 (SGR is one-based)"
    );
    // The release too, so a gesture cannot leave a button held.
    assert!(
        fixture
            .msg(&[
                "wait",
                "output",
                r"\[<0;6;3m",
                "--regex",
                "--pane-id",
                "2",
                "--after-offset",
                &before.to_string(),
                "--timeout",
                "5s",
            ])
            .status
            .success()
    );

    // A position outside the pane is refused rather than clamped onto some other pane.
    let outside = fixture.msg(&[
        "mouse",
        "click",
        "--cell-column",
        "9999",
        "--cell-row",
        "0",
        "--pane-id",
        "2",
    ]);
    assert!(!outside.status.success());
    assert!(
        String::from_utf8_lossy(&outside.stderr).contains("outside the pane"),
        "{}",
        String::from_utf8_lossy(&outside.stderr)
    );
}

/// A setter can be replayed; a toggle cannot.
#[test]
fn set_flag_is_idempotent_and_reports_whether_anything_changed() {
    let fixture = Fixture::start("set-flag");
    assert!(
        fixture
            .msg(&["split", "vertical", "--pane-id", "1"])
            .status
            .success()
    );

    let first = fixture.json(&["set-flag", "zoom", "--on", "--pane-id", "1"]);
    assert_eq!(first["enabled"], true);
    assert_eq!(first["changed"], true);

    // The property a toggle cannot have: running it again leaves the state alone.
    let again = fixture.json(&["set-flag", "zoom", "--on", "--pane-id", "1"]);
    assert_eq!(again["enabled"], true);
    assert_eq!(again["changed"], false, "a setter must be idempotent");

    let off = fixture.json(&["set-flag", "zoom", "--off", "--pane-id", "1"]);
    assert_eq!(off["enabled"], false);
    assert_eq!(off["changed"], true);

    // Turning zoom off on a pane that was not the zoomed one is a no-op, not a way to unzoom
    // somebody else.
    assert!(
        fixture
            .msg(&["set-flag", "zoom", "--on", "--pane-id", "1"])
            .status
            .success()
    );
    let other = fixture.json(&["set-flag", "zoom", "--off", "--pane-id", "2"]);
    assert_eq!(other["changed"], false, "{other}");
    assert_eq!(
        fixture.json(&["layout"])["tabs"][0]["zoomed_pane_id"],
        1,
        "unzooming pane 2 cleared pane 1's zoom"
    );

    // Pinning refuses a tiled pane rather than silently doing nothing.
    let pinned = fixture.msg(&["set-flag", "pinned", "--on", "--pane-id", "2"]);
    assert!(!pinned.status.success());
    assert!(
        String::from_utf8_lossy(&pinned.stderr).contains("only floating panes"),
        "{}",
        String::from_utf8_lossy(&pinned.stderr)
    );
}

#[test]
fn resize_pane_sets_an_exact_size_and_move_pane_relocates_without_respawning() {
    let fixture = Fixture::start("resize-move");
    assert!(
        fixture
            .msg(&["split", "vertical", "--pane-id", "1"])
            .status
            .success()
    );

    let resized = fixture.json(&["resize-pane", "--pane-id", "1", "--columns", "20"]);
    assert_eq!(
        resized["columns"], 20,
        "resize-pane must land on the exact size, not near it: {resized}"
    );
    // The neighbour absorbed the difference rather than the tab growing.
    let layout = fixture.json(&["layout"]);
    let total: u64 = layout["tabs"][0]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| pane["geometry"]["width"].as_u64().unwrap())
        .sum();
    assert_eq!(total, layout["area"]["width"].as_u64().unwrap());

    // A pane with no split on the requested axis cannot be given a size on it.
    let vertical = fixture.msg(&["resize-pane", "--pane-id", "1", "--rows", "5"]);
    assert!(!vertical.status.success());

    // Name the pane first: the move must carry its identity, not just its process.
    assert!(
        fixture
            .msg(&["pane-rename", "--pane-id", "1", "--name", "mover"])
            .status
            .success()
    );
    let scratch = fixture.json(&["new-tab", "--name", "scratch"]);
    let scratch_id = scratch["tab_id"].as_u64().unwrap().to_string();

    let moved = fixture.json(&["move-pane", "--pane-id", "1", "--to-tab", &scratch_id]);
    assert_eq!(moved["moved"], "tab");
    assert_eq!(moved["tab_id"].as_u64().unwrap().to_string(), scratch_id);

    // Same pane, same name, new tab. A move that respawned anything would lose both.
    let inspected = fixture.json(&["inspect", "--pane-name", "mover"]);
    assert_eq!(inspected["pane"]["pane_id"], 1);
    assert_eq!(
        inspected["pane"]["tab_id"].as_u64().unwrap().to_string(),
        scratch_id
    );

    // A swap trades tree positions and leaves both panes alive.
    assert!(
        fixture
            .msg(&["split", "vertical", "--pane-id", "1"])
            .status
            .success()
    );
    let before = fixture.json(&["inspect", "--pane-id", "1"])["pane"]["split_path"].clone();
    let swapped = fixture.json(&["move-pane", "--pane-id", "1", "--swap", "right"]);
    assert_eq!(swapped["moved"], "swap");
    assert_ne!(
        fixture.json(&["inspect", "--pane-id", "1"])["pane"]["split_path"],
        before,
        "the swap did not move the pane"
    );
}
