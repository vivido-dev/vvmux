#![cfg(unix)]

#[allow(dead_code)]
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;

use serde_json::Value;

struct SessionGuard {
    runtime: std::path::PathBuf,
    name: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = common::vvmux_command(&self.runtime)
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

#[test]
fn pane_automation_drives_and_observes_only_the_selected_pane() {
    // Short `/tmp` root: the runtime directory holds the session socket, whose path must stay
    // inside the platform's `sun_path` limit. Isolating `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR`
    // keeps a developer's own `startup.toml` and live sessions out of this test's session.
    let directory = tempfile::Builder::new()
        .prefix("vva-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = directory.path().to_path_buf();
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        br#"#!/bin/sh
printf 'READY pane=%s tab=%s\n' "$VVMUX_PANE_ID" "$VVMUX_TAB_ID"
while IFS= read -r line; do
    if [ "$line" = exit ]; then
        exit 0
    fi
    printf 'OUT pane=%s:%s\n' "$VVMUX_PANE_ID" "$line"
done
"#,
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = directory.path().join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = {}\nrender_interval_ms = 1\n",
            toml_string(&shell)
        ),
    )
    .unwrap();

    let name = format!("automation-test-{}", std::process::id());
    let created = common::vvmux_command(&runtime)
        .args([
            "--config",
            config.to_str().unwrap(),
            "new",
            "-s",
            &name,
            "-d",
        ])
        .output()
        .unwrap();
    assert_success(&created);
    let _guard = SessionGuard {
        runtime: runtime.clone(),
        name: name.clone(),
    };

    wait_text(&runtime, &name, 1, "READY pane=1 tab=1");
    let right = json(command(
        &runtime,
        &name,
        &["split", "vertical", "--pane-id", "1"],
    ));
    assert_eq!(right["new_pane_id"], 2);
    let bottom_left = json(command(
        &runtime,
        &name,
        &["split", "horizontal", "--pane-id", "1"],
    ));
    assert_eq!(bottom_left["new_pane_id"], 3);
    let bottom_right = json(command(
        &runtime,
        &name,
        &["split", "horizontal", "--pane-id", "2"],
    ));
    assert_eq!(bottom_right["new_pane_id"], 4);

    for pane in 1..=4 {
        wait_text(&runtime, &name, pane, &format!("READY pane={pane} tab=1"));
    }

    assert_success(&command(
        &runtime,
        &name,
        &["typing", "hello-top-right", "--pane-id", "2"],
    ));
    assert_success(&command(
        &runtime,
        &name,
        &["key", "Enter", "--pane-id", "2"],
    ));
    wait_text(&runtime, &name, 2, "OUT pane=2:hello-top-right");

    let write_report = json(command(
        &runtime,
        &name,
        &["typing", "reported-input", "--pane-id", "2", "--report"],
    ));
    assert_eq!(write_report["pane_id"], 2);
    assert_eq!(write_report["encoded_byte_count"], 14);
    assert_eq!(write_report["pty_write_completed"], true);
    assert_eq!(write_report["application_consumption_observed"], false);
    assert_success(&command(
        &runtime,
        &name,
        &["key", "Enter", "--pane-id", "2"],
    ));
    wait_text(&runtime, &name, 2, "OUT pane=2:reported-input");

    let session = json(command(&runtime, &name, &["session-inspect"]));
    assert_eq!(session["session"], name);
    assert_eq!(session["active_tab_id"], 1);
    assert!(session["pending"]["actor_work"].is_u64());
    assert!(session["queue_health"]["ipc"]["records_read"].is_u64());

    let tabs = json(command(&runtime, &name, &["list-tabs"]));
    assert_eq!(tabs["tabs"][0]["tab_id"], 1);
    assert_eq!(tabs["tabs"][0]["active"], true);
    let selected = json(command(&runtime, &name, &["select-tab", "--tab-id", "1"]));
    assert_eq!(selected["tab_id"], 1);

    let diagnosed = json(command(
        &runtime,
        &name,
        &["diagnose", "--pane-id", "2", "--trace-limit", "16"],
    ));
    assert_eq!(diagnosed["schema_version"], 1);
    assert_eq!(diagnosed["panes"][0]["pane"]["pane_id"], 2);
    assert!(diagnosed["panes"][0]["trace"]["events"].is_array());

    let doctor = common::vvmux_command(&runtime)
        .args(["doctor", "--target", &name, "--json"])
        .output()
        .unwrap();
    let doctor = json(doctor);
    assert_eq!(doctor["checks"]["registry_identity"], "ok");
    assert_eq!(doctor["checks"]["ipc_responsive"], "ok");

    let listed = common::vvmux_command(&runtime)
        .args(["list", "--json"])
        .output()
        .unwrap();
    let listed = json(listed);
    assert!(
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["name"] == name && session["responsive"] == true)
    );

    let bundle = directory.path().join("diagnose.zip");
    let bundled = common::vvmux_command(&runtime)
        .args([
            "debug-bundle",
            "--target",
            &name,
            "--output",
            bundle.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_success(&bundled);
    let archive = fs::read(bundle).unwrap();
    assert!(
        archive
            .windows(b"manifest.json".len())
            .any(|part| part == b"manifest.json")
    );
    assert!(
        archive
            .windows(b"diagnose.json".len())
            .any(|part| part == b"diagnose.json")
    );
    assert!(
        !archive
            .windows(b"content/".len())
            .any(|part| part == b"content/")
    );

    let top_right = text(command(&runtime, &name, &["get-text", "--pane-id", "2"]));
    assert!(top_right.contains("OUT pane=2:hello-top-right"));
    for pane in [1, 3, 4] {
        let output = text(command(
            &runtime,
            &name,
            &["get-text", "--pane-id", &pane.to_string()],
        ));
        assert!(
            !output.contains("hello-top-right"),
            "output leaked into pane {pane}"
        );
    }
    let listed = json(command(&runtime, &name, &["list-panes"]));
    assert_eq!(
        listed["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pane| pane["pane_id"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert!(listed["panes"][0]["agent"].is_null());

    let reported = json(command(
        &runtime,
        &name,
        &[
            "report-agent",
            "--agent",
            "codex",
            "--state",
            "working",
            "--source",
            "integration-test",
            "--sequence",
            "1",
            "--pane-id",
            "2",
        ],
    ));
    assert_eq!(reported["pane_id"], 2);
    assert_eq!(reported["agent"]["kind"], "codex");
    assert_eq!(reported["agent"]["label"], "Codex");
    assert_eq!(reported["agent"]["provider"], "dev.vivido.agent.codex");
    assert_eq!(reported["agent"]["state"], "working");
    assert_eq!(reported["agent"]["status"], "working");
    assert_eq!(reported["agent"]["source"], "report");

    let after_report = json(command(&runtime, &name, &["list-panes"]));
    let panes = after_report["panes"].as_array().unwrap();
    assert!(panes.iter().find(|pane| pane["pane_id"] == 1).unwrap()["agent"].is_null());
    assert_eq!(
        panes.iter().find(|pane| pane["pane_id"] == 2).unwrap()["agent"]["status"],
        "working"
    );
    let inspected = json(command(&runtime, &name, &["inspect", "--pane-id", "2"]));
    assert_eq!(inspected["pane"]["agent"]["kind"], "codex");
    assert_eq!(inspected["pane"]["agent"]["source"], "report");

    let stale_report = command(
        &runtime,
        &name,
        &[
            "report-agent",
            "--agent",
            "codex",
            "--state",
            "blocked",
            "--source",
            "integration-test",
            "--sequence",
            "1",
            "--pane-id",
            "2",
        ],
    );
    assert!(!stale_report.status.success());
    assert!(String::from_utf8_lossy(&stale_report.stderr).contains("invalid_agent_report"));

    let done = json(command(
        &runtime,
        &name,
        &[
            "report-agent",
            "--agent",
            "codex",
            "--state",
            "idle",
            "--source",
            "integration-test",
            "--sequence",
            "2",
            "--pane-id",
            "2",
        ],
    ));
    assert_eq!(done["agent"]["state"], "idle");
    assert_eq!(done["agent"]["status"], "done");

    assert_success(&command(
        &runtime,
        &name,
        &[
            "clear-agent-report",
            "--source",
            "integration-test",
            "--sequence",
            "3",
            "--pane-id",
            "2",
        ],
    ));

    // A block reason is ordinary display data, but the native session reference names a resumable
    // conversation on the user's agent account. Only a single-pane inspect discloses it; the bulk
    // listing and the diagnostic that feeds debug bundles report presence alone.
    let annotated = json(command(
        &runtime,
        &name,
        &[
            "report-agent",
            "--agent",
            "codex",
            "--state",
            "blocked",
            "--source",
            "integration-test",
            "--sequence",
            "10",
            "--message",
            "waiting for approval: write src/main.rs",
            "--agent-session-id",
            "conversation-7",
            "--pane-id",
            "2",
        ],
    ));
    assert_eq!(
        annotated["agent"]["message"],
        "waiting for approval: write src/main.rs"
    );
    assert_eq!(annotated["agent"]["session_present"], true);

    let inspected = json(command(&runtime, &name, &["inspect", "--pane-id", "2"]));
    assert_eq!(
        inspected["pane"]["agent"]["agent_session"]["id"],
        "conversation-7"
    );

    let listed = json(command(&runtime, &name, &["list-panes"]));
    let pane = listed["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["pane_id"] == 2)
        .unwrap();
    assert_eq!(pane["agent"]["session_present"], true);
    assert!(pane["agent"]["agent_session"].is_null());
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains("conversation-7")
    );

    let diagnosed = json(command(
        &runtime,
        &name,
        &["diagnose", "--pane-id", "2", "--trace-limit", "16"],
    ));
    assert!(
        !serde_json::to_string(&diagnosed)
            .unwrap()
            .contains("conversation-7")
    );

    // A state-only report from the same source keeps the reference the resume path needs.
    assert_success(&command(
        &runtime,
        &name,
        &[
            "report-agent",
            "--agent",
            "codex",
            "--state",
            "working",
            "--source",
            "integration-test",
            "--sequence",
            "11",
            "--pane-id",
            "2",
        ],
    ));
    let after_state_only = json(command(&runtime, &name, &["inspect", "--pane-id", "2"]));
    assert_eq!(
        after_state_only["pane"]["agent"]["agent_session"]["id"],
        "conversation-7"
    );
    assert!(after_state_only["pane"]["agent"]["message"].is_null());

    let oversized = command(
        &runtime,
        &name,
        &[
            "report-agent",
            "--agent",
            "codex",
            "--state",
            "blocked",
            "--source",
            "integration-test",
            "--sequence",
            "12",
            "--message",
            &"m".repeat(257),
            "--pane-id",
            "2",
        ],
    );
    assert!(!oversized.status.success());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("invalid_params"));

    assert_success(&command(
        &runtime,
        &name,
        &[
            "clear-agent-report",
            "--source",
            "integration-test",
            "--sequence",
            "13",
            "--pane-id",
            "2",
        ],
    ));

    // Every agent mutation routes through one choke point, so a report that leaves the pane's
    // effective snapshot unchanged is not a transition and must not advance the session sequence.
    // Two consecutive inspects first establish that the session is otherwise quiet, so a failure
    // below points at agent sequencing rather than at unrelated session traffic.
    let sequence_of = |arguments: &[&str]| -> u64 {
        json(command(&runtime, &name, arguments))["session_sequence"]
            .as_u64()
            .unwrap()
    };
    let quiet = sequence_of(&["inspect", "--pane-id", "2"]);
    assert_eq!(quiet, sequence_of(&["inspect", "--pane-id", "2"]));

    let report = |state: &str, sequence: &str| {
        assert_success(&command(
            &runtime,
            &name,
            &[
                "report-agent",
                "--agent",
                "codex",
                "--state",
                state,
                "--source",
                "integration-test",
                "--sequence",
                sequence,
                "--pane-id",
                "2",
            ],
        ));
    };

    report("working", "14");
    let after_transition = sequence_of(&["inspect", "--pane-id", "2"]);
    assert!(after_transition > quiet);

    report("working", "15");
    assert_eq!(
        after_transition,
        sequence_of(&["inspect", "--pane-id", "2"])
    );

    report("blocked", "16");
    assert!(sequence_of(&["inspect", "--pane-id", "2"]) > after_transition);

    assert_success(&command(
        &runtime,
        &name,
        &[
            "clear-agent-report",
            "--source",
            "integration-test",
            "--sequence",
            "17",
            "--pane-id",
            "2",
        ],
    ));

    assert_success(&command(
        &runtime,
        &name,
        &[
            "report-agent",
            "--agent",
            "codex",
            "--state",
            "working",
            "--source",
            "integration-test",
            "--sequence",
            "20",
            "--pane-id",
            "2",
        ],
    ));

    // Display-only metadata annotates a pane without claiming lifecycle authority.
    let metadata = json(command(
        &runtime,
        &name,
        &[
            "report-metadata",
            "--source",
            "integration-test",
            "--sequence",
            "21",
            "--token",
            "files=42",
            "--display-agent",
            "Codex (review)",
            "--state-label",
            "idle=ready",
            "--title",
            "reviewing src/agent.rs",
            "--pane-id",
            "2",
        ],
    ));
    assert_eq!(metadata["changed"], true);
    assert_eq!(metadata["metadata"]["tokens"]["files"], "42");
    assert_eq!(metadata["metadata"]["display_agent"], "Codex (review)");
    assert_eq!(metadata["metadata"]["state_labels"]["idle"], "ready");
    assert_eq!(metadata["metadata"]["title"], "reviewing src/agent.rs");

    // Metadata is display-only: it must not disturb the lifecycle state waiters react to.
    let after_metadata = json(command(&runtime, &name, &["inspect", "--pane-id", "2"]));
    assert_eq!(after_metadata["pane"]["agent"]["state"], "working");
    assert_eq!(
        after_metadata["pane"]["agent"]["metadata"]["tokens"]["files"],
        "42"
    );

    // A token with a TTL is swept once it is due, and the sweep takes only what expired. Polling
    // with `inspect` wakes the actor itself, so this covers the sweep, not the wake scheduling;
    // `next_expiry` feeding `next_agent_evaluation_delay` is unit-tested separately.
    assert_success(&command(
        &runtime,
        &name,
        &[
            "report-metadata",
            "--source",
            "integration-test",
            "--sequence",
            "22",
            "--token",
            "transient=soon",
            "--ttl-ms",
            "200",
            "--pane-id",
            "2",
        ],
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let inspected = json(command(&runtime, &name, &["inspect", "--pane-id", "2"]));
        if inspected["pane"]["agent"]["metadata"]["tokens"]["transient"].is_null() {
            // The untimed token from the previous call is untouched by the sweep.
            assert_eq!(
                inspected["pane"]["agent"]["metadata"]["tokens"]["files"],
                "42"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "metadata token outlived its TTL"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let bad_token = command(
        &runtime,
        &name,
        &[
            "report-metadata",
            "--source",
            "integration-test",
            "--sequence",
            "23",
            "--token",
            &format!("wide={}", "v".repeat(129)),
            "--pane-id",
            "2",
        ],
    );
    assert!(!bad_token.status.success());

    let missing_metadata_pane = common::vvmux_command(&runtime)
        .args([
            "msg",
            "--target",
            &name,
            "report-metadata",
            "--source",
            "integration-test",
            "--sequence",
            "24",
            "--token",
            "files=1",
        ])
        .env_remove("VVMUX_SESSION")
        .env_remove("VVMUX_PANE_ID")
        .output()
        .unwrap();
    assert!(!missing_metadata_pane.status.success());
    assert!(String::from_utf8_lossy(&missing_metadata_pane.stderr).contains("requires --pane-id"));

    let missing_target = common::vvmux_command(&runtime)
        .args([
            "msg",
            "--target",
            &name,
            "report-agent",
            "--agent",
            "codex",
            "--state",
            "idle",
            "--source",
            "integration-test",
            "--sequence",
            "1",
        ])
        .env_remove("VVMUX_SESSION")
        .env_remove("VVMUX_PANE_ID")
        .output()
        .unwrap();
    assert!(!missing_target.status.success());
    assert!(String::from_utf8_lossy(&missing_target.stderr).contains("requires --pane-id"));

    let focused = text(command(&runtime, &name, &["get-text"]));
    assert!(focused.contains("READY pane=4 tab=1"));
    let inherited = common::vvmux_command(&runtime)
        .args(["msg", "get-text"])
        .env("VVMUX_SESSION", &name)
        .env("VVMUX_PANE_ID", "2")
        .output()
        .unwrap();
    assert!(text(inherited).contains("READY pane=2 tab=1"));

    let grid = json(command(&runtime, &name, &["get-grid", "--pane-id", "2"]));
    assert_eq!(grid["pane_id"], 2);
    assert!(grid["full"].as_bool().unwrap());
    assert!(grid["grid"]["columns"].as_u64().unwrap() >= 4);
    assert!(grid["grid"]["rows"].as_u64().unwrap() >= 2);
    let grid_text = grid["rows"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["cells"].as_array().unwrap())
        .filter_map(|cell| cell["text"].as_str())
        .collect::<String>();
    assert!(grid_text.contains("hello-top-right"));

    let sequence = grid["screen_sequence"].as_u64().unwrap();
    let unchanged = json(command(
        &runtime,
        &name,
        &[
            "get-grid",
            "--pane-id",
            "2",
            "--since-screen",
            &sequence.to_string(),
        ],
    ));
    assert!(!unchanged["full"].as_bool().unwrap());
    assert!(unchanged["rows"].as_array().unwrap().is_empty());

    let stable = json(command(
        &runtime,
        &name,
        &[
            "wait",
            "screen-stable",
            "--pane-id",
            "2",
            "--quiet",
            "1ms",
            "--timeout",
            "2s",
        ],
    ));
    assert_eq!(stable["pane_id"], 2);

    let trace = json(command(
        &runtime,
        &name,
        &["trace-media", "--pane-id", "2", "--limit", "16"],
    ));
    assert!(trace["current_sequence"].is_u64());
    assert!(trace["oldest_sequence"].is_u64());
    assert!(trace["events"].as_array().is_some());

    assert_success(&command(
        &runtime,
        &name,
        &["typing", "exit", "--pane-id", "4"],
    ));
    assert_success(&command(
        &runtime,
        &name,
        &["key", "Enter", "--pane-id", "4"],
    ));
    let exited = json(command(
        &runtime,
        &name,
        &["wait", "exit", "--pane-id", "4", "--timeout", "2s"],
    ));
    assert_eq!(exited["code"], 0);
    assert_eq!(exited["success"], true);
    let stale = command(&runtime, &name, &["get-text", "--pane-id", "4"]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("pane_not_found"));
}

fn command(runtime: &Path, session: &str, arguments: &[&str]) -> Output {
    common::vvmux_command(runtime)
        .args(["msg", "--target", session])
        .args(arguments)
        .output()
        .unwrap()
}

fn wait_text(runtime: &Path, session: &str, pane: u64, pattern: &str) {
    let output = command(
        runtime,
        session,
        &[
            "wait",
            "text",
            pattern,
            "--pane-id",
            &pane.to_string(),
            "--timeout",
            "5s",
        ],
    );
    assert_success(&output);
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

fn text(output: Output) -> String {
    assert_success(&output);
    String::from_utf8(output.stdout).unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn toml_string(path: &Path) -> String {
    serde_json::to_string(path.to_str().unwrap()).unwrap()
}
