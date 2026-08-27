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
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct SessionGuard {
    binary: &'static str,
    name: String,
    runtime: PathBuf,
    state: PathBuf,
    config_home: PathBuf,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = Command::new(self.binary)
            .args(["kill-session", "--target", &self.name])
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .output();
    }
}

struct Fixture {
    binary: &'static str,
    name: String,
    runtime: PathBuf,
    state: PathBuf,
    config_home: PathBuf,
    _guard: SessionGuard,
    _directory: tempfile::TempDir,
}

impl Fixture {
    /// A detached session whose daemon was started with an outer Vivido identity in scope.
    ///
    /// The identity is set deliberately: the scrub is only observable if there was something to
    /// scrub, and a fixture that never sets it would pass no matter what the launcher did.
    fn start(label: &str) -> Self {
        let binary = env!("CARGO_BIN_EXE_vvmux");
        let directory = tempfile::Builder::new()
            .prefix("vvmux-contract-")
            .tempdir_in("/tmp")
            .unwrap();
        let runtime = directory.path().join("runtime");
        let state = directory.path().join("state");
        let config_home = directory.path().join("config-home");
        private_directory(&runtime);
        private_directory(&state);
        private_directory(&config_home);
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
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", &state)
            .env("XDG_CONFIG_HOME", &config_home)
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
            runtime: runtime.clone(),
            state: state.clone(),
            config_home: config_home.clone(),
            _guard: SessionGuard {
                binary,
                name,
                runtime,
                state,
                config_home,
            },
            _directory: directory,
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

    fn command(&self) -> Command {
        let mut command = Command::new(self.binary);
        command
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config_home);
        command
    }

    fn msg(&self, arguments: &[&str]) -> Output {
        self.command()
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

fn private_directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
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
    let mut streaming = fixture
        .command()
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

/// A plan is one connection, and results flow between its steps.
#[test]
fn run_plan_binds_results_between_steps_and_verifies_them() {
    let fixture = Fixture::start("plan");
    let plan = fixture._directory.path().join("plan.json");
    fs::write(
        &plan,
        r#"{
  "version": 1,
  "steps": [
    {"id": "split", "method": "split", "params": {"axis": "Vertical"}, "pane_id": 1,
     "bind": {"right": "/new_pane_id"}},
    {"id": "name", "method": "pane_rename", "pane_id": {"$ref": "right"},
     "params": {"name": "worker"}},
    {"id": "run", "method": "submit_line", "pane_name": "worker",
     "params": {"text": "printf 'plan-ran\\n'", "report": true},
     "verify": {"screen_changed": true, "capture": true, "timeout_ms": 5000}},
    {"id": "confirm", "method": "wait_text", "pane_name": "worker",
     "params": {"text": "plan-ran", "regex": false, "after_screen": null, "timeout_ms": 5000}}
  ]
}"#,
    )
    .unwrap();

    let output = fixture.msg(&["run-plan", "--file", plan.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = ndjson(&output.stdout);
    assert_eq!(events[0]["type"], "plan_started");
    assert_eq!(events[0]["mode"], "execute");
    assert_eq!(events.last().unwrap()["status"], "ok");
    assert_eq!(events.last().unwrap()["failures"], 0);

    let step = |id: &str| {
        events
            .iter()
            .find(|event| event["id"] == id)
            .unwrap_or_else(|| panic!("no step {id} in {events:?}"))
            .clone()
    };
    for id in ["split", "name", "run", "confirm"] {
        assert_eq!(step(id)["status"], "ok", "{id} did not run");
    }
    // The second step targeted a pane the first one created; without binding it could not have.
    assert_eq!(step("name")["result"]["pane_name"], "worker");
    // Verification rides inside the step, so its result is part of that step's answer.
    let verification = &step("run")["result"]["verification"];
    assert!(verification["screen"].is_object(), "{verification}");
    assert!(verification["capture"].is_string(), "{verification}");
}

#[test]
fn run_plan_preflight_skips_mutations_and_validation_rejects_a_plan_whole() {
    let fixture = Fixture::start("plan-guards");
    let plan = fixture._directory.path().join("plan.json");
    fs::write(
        &plan,
        r#"{
  "version": 1,
  "steps": [
    {"id": "look", "method": "list_panes"},
    {"id": "change", "method": "split", "params": {"axis": "Vertical"}, "pane_id": 1}
  ]
}"#,
    )
    .unwrap();

    let preflight = fixture.msg(&["run-plan", "--file", plan.to_str().unwrap(), "--preflight"]);
    assert!(preflight.status.success());
    let events = ndjson(&preflight.stdout);
    let step = |id: &str| {
        events
            .iter()
            .find(|event| event["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(step("look")["status"], "ok");
    assert_eq!(step("change")["status"], "skipped");
    assert_eq!(step("change")["reason"], "preflight_mutation");
    // The mutation really was skipped, not merely reported as skipped.
    assert_eq!(
        fixture.json(&["list-panes"])["panes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let dry = fixture.msg(&["run-plan", "--file", plan.to_str().unwrap(), "--dry-run"]);
    assert!(dry.status.success());
    for event in ndjson(&dry.stdout).iter().filter(|e| e["type"] == "step") {
        assert_eq!(event["status"], "planned");
    }

    // A plan is rejected before any of it runs, so a typo on the last step does not first perform
    // the mutations in the steps before it.
    let forward = fixture._directory.path().join("forward.json");
    fs::write(
        &forward,
        r#"{"version":1,"steps":[
          {"id":"uses","method":"inspect","pane_id":{"$ref":"later"}},
          {"id":"binds","method":"list_panes","bind":{"later":"/panes/0/pane_id"}}]}"#,
    )
    .unwrap();
    let rejected = fixture.msg(&["run-plan", "--file", forward.to_str().unwrap()]);
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("no earlier step binds"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(
        rejected.stdout.is_empty(),
        "a rejected plan must not have started running: {:?}",
        String::from_utf8_lossy(&rejected.stdout)
    );

    let unknown = fixture._directory.path().join("unknown.json");
    fs::write(
        &unknown,
        r#"{"version":1,"steps":[{"id":"nope","method":"teleport"}]}"#,
    )
    .unwrap();
    let refused = fixture.msg(&["run-plan", "--file", unknown.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("does not serve"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

/// The race every inspect-then-act pair has, and the retry every lost reply causes.
#[test]
fn expectations_reject_stale_actions_and_idempotency_keys_apply_once() {
    let fixture = Fixture::start("atomicity");
    let sequence = fixture.json(&["inspect", "--pane-id", "1"])["pane"]["screen_sequence"]
        .as_u64()
        .unwrap();

    // A current expectation is accepted.
    assert!(
        fixture
            .msg(&[
                "--expect-screen",
                &sequence.to_string(),
                "typing",
                "--pane-id",
                "1",
                "x",
            ])
            .status
            .success()
    );

    // A stale one is refused before anything reaches the PTY.
    let stale = fixture.msg(&[
        "--expect-screen",
        "999999",
        "typing",
        "--pane-id",
        "1",
        "never-typed",
    ]);
    assert!(!stale.status.success());
    let message = String::from_utf8_lossy(&stale.stderr);
    assert!(message.contains("invalid_state"), "{message}");
    assert!(
        !String::from_utf8_lossy(&fixture.msg(&["transcript", "--pane-id", "1"]).stdout)
            .contains("never-typed"),
        "a refused request still reached the PTY"
    );

    // A retried mutation is applied once, and the retry gets the first answer back.
    let first = fixture.json(&[
        "--idempotency-key",
        "k1",
        "split",
        "vertical",
        "--pane-id",
        "1",
    ]);
    let retry = fixture.json(&[
        "--idempotency-key",
        "k1",
        "split",
        "vertical",
        "--pane-id",
        "1",
    ]);
    assert_eq!(first, retry, "a retry must replay the original reply");
    assert_eq!(
        fixture.json(&["list-panes"])["panes"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "the retry created a second pane"
    );

    // An observation refuses a key: replaying a cached read would be a lie about the present.
    let observation = fixture.msg(&["--idempotency-key", "k2", "get-text", "--pane-id", "1"]);
    assert!(!observation.status.success());

    // A failed request releases its key, so a corrected retry can use it.
    let failed = fixture.msg(&[
        "--idempotency-key",
        "k3",
        "pane-rename",
        "--pane-id",
        "9999",
        "--name",
        "ghost",
    ]);
    assert!(!failed.status.success());
    assert!(
        fixture
            .msg(&[
                "--idempotency-key",
                "k3",
                "pane-rename",
                "--pane-id",
                "1",
                "--name",
                "real"
            ])
            .status
            .success(),
        "a key claimed by a failed request stayed claimed"
    );
}

/// A command boundary is reported by the shell or not known at all.
#[test]
fn shell_command_returns_a_real_exit_status_and_refuses_a_shell_that_reports_none() {
    let fixture = Fixture::start("shell-command");

    // The fixture shell emits no OSC 133, so the boundary is genuinely unknowable. Refused rather
    // than guessed at from prompt text, which is the whole point of requiring the markers.
    let refused = fixture.msg(&["shell-command", "true", "--pane-id", "1"]);
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("OSC 133"), "{message}");

    // A pane whose shell does report boundaries. bash needs no rc file for this: a DEBUG trap
    // marks the start of a command and PROMPT_COMMAND marks its end with the real status.
    let opened = fixture.json(&["run", "exec bash --norc --noprofile", "--hold"]);
    let pane = opened["pane_id"].as_u64().unwrap().to_string();
    for setup in [
        r#"trap 'printf "\033]133;C\007"' DEBUG"#,
        r#"PROMPT_COMMAND='printf "\033]133;D;%s\007" "$?"'"#,
    ] {
        assert!(
            fixture
                .msg(&["submit", "--pane-id", &pane, setup])
                .status
                .success()
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // Wait for the markers to be seen before relying on them.
    for _ in 0..50 {
        if fixture
            .msg(&[
                "shell-command",
                "true",
                "--pane-id",
                &pane,
                "--timeout",
                "5s",
            ])
            .status
            .success()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    for (command, expected) in [("true", 0), ("false", 1), ("(exit 7)", 7)] {
        let result = fixture.json(&[
            "shell-command",
            command,
            "--pane-id",
            &pane,
            "--timeout",
            "10s",
        ]);
        assert_eq!(
            result["exit_code"], expected,
            "`{command}` reported the wrong status: {result}"
        );
        // The status comes from the shell, so it is the command's own, not a guess from output.
        assert!(result["command_id"].as_u64().unwrap() > 0);
        #[cfg(target_os = "linux")]
        assert!(result["cwd"].is_string(), "{result}");
        #[cfg(not(target_os = "linux"))]
        assert!(result["cwd"].is_null(), "{result}");
    }

    // One line, one command: a newline would submit two and the marker waited for would belong to
    // whichever finished first.
    let multiline = fixture.msg(&["shell-command", "true\nfalse", "--pane-id", &pane]);
    assert!(!multiline.status.success());
}

#[test]
fn capture_reveals_waits_and_reads_in_one_request() {
    let fixture = Fixture::start("capture");
    assert!(
        fixture
            .msg(&["split", "vertical", "--pane-id", "1"])
            .status
            .success()
    );
    assert!(
        fixture
            .msg(&["new-tab", "--name", "elsewhere"])
            .status
            .success()
    );
    assert!(
        fixture
            .msg(&["select-tab", "--tab-name", "elsewhere"])
            .status
            .success()
    );

    // The target is on a tab nobody is looking at. `capture` activates it as part of the read,
    // which is the sequencing a caller would otherwise have to get right itself.
    let captured = fixture.json(&["capture", "--pane-id", "2", "--stable", "200ms", "--grid"]);
    assert_eq!(captured["pane_id"], 2);
    assert!(captured["text"].is_string());
    assert!(captured["grid"].is_object(), "--grid was ignored");
    assert!(captured["geometry"].is_object());
    assert!(captured["screen_sequence"].is_number());
    assert_eq!(
        fixture.json(&["layout"])["active_tab_id"],
        fixture.json(&["inspect", "--pane-id", "2"])["pane"]["tab_id"],
        "capture did not reveal the pane's tab"
    );

    // `--no-activate` reads where the pane is, for a caller that must not disturb the layout.
    assert!(
        fixture
            .msg(&["select-tab", "--tab-name", "elsewhere"])
            .status
            .success()
    );
    let elsewhere = fixture.json(&["list-tabs"])["active_tab_id"].clone();
    let quiet = fixture.json(&["capture", "--pane-id", "2", "--no-activate"]);
    assert_eq!(quiet["pane_id"], 2);
    assert_eq!(
        fixture.json(&["list-tabs"])["active_tab_id"],
        elsewhere,
        "--no-activate changed the selected tab"
    );
}

/// Every NDJSON line a plan run emitted.
fn ndjson(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect()
}

/// Several agents share one session; a lease is how one says a pane is theirs.
#[test]
fn a_lease_excludes_other_automation_without_locking_anyone_out() {
    let fixture = Fixture::start("lease");
    assert!(
        fixture
            .msg(&["split", "vertical", "--pane-id", "1"])
            .status
            .success()
    );

    // Nothing held: everything is allowed. Leases are advisory, so adding the mechanism must not
    // make anything that worked before start failing.
    assert!(
        fixture
            .msg(&["typing", "--pane-id", "1", "x"])
            .status
            .success()
    );

    let held = fixture.json(&[
        "lease",
        "acquire",
        "--scope",
        "input",
        "--pane-id",
        "1",
        "--holder",
        "agent-a",
    ]);
    let lease = held["lease_id"].as_str().unwrap().to_owned();
    assert_eq!(held["scope"], "input");

    // Another caller is refused, and told who has it rather than just that it failed.
    let refused = fixture.msg(&["typing", "--pane-id", "1", "x"]);
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("lease_denied"), "{message}");
    assert!(message.contains("agent-a"), "{message}");

    // The holder acts under it.
    assert!(
        fixture
            .msg(&["--lease", &lease, "typing", "--pane-id", "1", "x"])
            .status
            .success()
    );
    // Observation is never excluded: watching a pane changes nothing about it.
    assert!(
        fixture
            .msg(&["get-text", "--pane-id", "1"])
            .status
            .success()
    );
    // An unleased pane is unaffected, and so is a scope nobody holds on this one.
    assert!(
        fixture
            .msg(&["typing", "--pane-id", "2", "x"])
            .status
            .success()
    );
    assert!(
        fixture
            .msg(&["set-flag", "zoom", "--on", "--pane-id", "1"])
            .status
            .success()
    );
    assert!(
        fixture
            .msg(&["set-flag", "zoom", "--off", "--pane-id", "1"])
            .status
            .success()
    );

    // A second holder of the same scope is refused at acquire time, not at use time.
    let contested = fixture.msg(&[
        "lease",
        "acquire",
        "--scope",
        "input",
        "--pane-id",
        "1",
        "--holder",
        "agent-b",
    ]);
    assert!(!contested.status.success());

    assert_eq!(
        fixture.json(&["lease", "list"])["leases"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(fixture.msg(&["lease", "release", &lease]).status.success());
    assert!(
        fixture
            .msg(&["typing", "--pane-id", "1", "x"])
            .status
            .success(),
        "releasing the lease did not free the pane"
    );

    // A lease must expire, so a crashed holder cannot keep a pane forever.
    let unbounded = fixture.msg(&[
        "lease",
        "acquire",
        "--scope",
        "input",
        "--pane-id",
        "1",
        "--ttl",
        "48h",
    ]);
    assert!(!unbounded.status.success());
}

/// A recording reproduces a session's shape without becoming a credential dump.
#[test]
fn a_recording_replays_output_and_never_stores_what_was_typed() {
    let fixture = Fixture::start("record");
    let path = fixture._directory.path().join("recording.ndjson");

    assert_eq!(fixture.json(&["record", "status"])["recording"], false);
    let started = fixture.json(&["record", "start", path.to_str().unwrap()]);
    assert_eq!(started["recording"], true);
    // Nothing is written until it stops, so a running recording cannot be half-read.
    assert!(!path.exists());

    // A marker that is typed, so it must NOT survive into the file as text, and output derived
    // from it, which must.
    assert!(
        fixture
            .msg(&[
                "submit",
                "--pane-id",
                "1",
                r#"S=hunter2-typed-secret; printf 'echoed-%s\n' "${S#hunter2-}""#,
            ])
            .status
            .success()
    );
    assert!(
        fixture
            .msg(&[
                "wait",
                "output",
                "echoed-typed-secret",
                "--pane-id",
                "1",
                "--timeout",
                "5s"
            ])
            .status
            .success()
    );
    assert!(
        fixture
            .msg(&["split", "vertical", "--pane-id", "1"])
            .status
            .success()
    );

    let stopped = fixture.json(&["record", "stop"]);
    assert!(stopped["events"].as_u64().unwrap() > 0);
    assert_eq!(stopped["dropped_events"], 0);
    assert_eq!(fixture.json(&["record", "status"])["recording"], false);

    let raw = fs::read_to_string(&path).unwrap();
    // The input frame records that a pane was written to and how much, never the bytes.
    assert!(
        raw.contains("\"submit_line\""),
        "no input class was recorded"
    );
    assert!(
        !raw.contains("hunter2"),
        "the recording stored what was typed"
    );

    let output = fixture.msg(&["replay", "--pane-id", "1"]);
    // `replay` is a top-level command, not a `msg` one: it reads a file and talks to no session.
    let _ = output;
    let replayed: Value = serde_json::from_slice(
        &Command::new(fixture.binary)
            .args(["replay"])
            .arg(&path)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(replayed["events"].as_u64().unwrap() > 0);
    assert!(
        replayed["gap"].is_null(),
        "an unbounded recording reported a gap"
    );
    let pane = replayed["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["pane_id"] == 1)
        .expect("no pane 1 in the replay");
    assert!(
        pane["text"]
            .as_str()
            .unwrap()
            .contains("echoed-typed-secret"),
        "replay did not reconstruct the pane's output: {pane}"
    );
}

/// A pane cannot inherit the outer identity, so the session publishes the live one instead.
#[test]
fn the_session_reports_the_presenting_window_or_says_there_is_none() {
    let fixture = Fixture::start("outer");
    // Detached: there is no window presenting this session, and saying so is the useful answer.
    // A stale value here is exactly the bug the environment scrub removed.
    let inspected = fixture.json(&["session-inspect"]);
    assert!(
        inspected["outer"].is_null(),
        "a detached session claimed a presenting window: {}",
        inspected["outer"]
    );
    assert!(
        inspected["attachment"].is_null(),
        "the fixture session is not attached"
    );

    // The per-pane crop is absent for the same reason: a rectangle in a window that is not there
    // would be confidently wrong.
    let pane = fixture.json(&["inspect", "--pane-id", "1"]);
    assert!(pane["pane"]["outer_crop"].is_null(), "{pane}");

    // Whatever the session reports, it never carries anything that could reach the outer
    // presenter. This is the standing invariant the whole struct exists to keep.
    let encoded = serde_json::to_string(&inspected).unwrap();
    for secret in [
        "VIVID_ROOT_SECRET",
        "VIVID_ENDPOINT",
        "root_secret",
        "token",
    ] {
        assert!(
            !encoded.contains(secret),
            "session-inspect leaked {secret}: {encoded}"
        );
    }
}
