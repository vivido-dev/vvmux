#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
fn recursive_git_dependencies_and_bounded_workflows_execute_end_to_end() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);

    let repositories = directory.path().join("repositories");
    fs::create_dir(&repositories).unwrap();
    let base = repositories.join("base");
    write_base(&base);
    git_commit(&base);
    let runner = repositories.join("runner");
    write_runner(&runner);
    git_commit(&runner);
    let bundle = write_bundle(directory.path());
    let rewrite = format!("url.file://{}/.insteadOf", repositories.display());

    let installed = command(binary, &runtime, &config_home)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", rewrite)
        .env("GIT_CONFIG_VALUE_0", "https://fixtures.invalid/")
        .args(["plugin", "install", bundle.to_str().unwrap(), "--yes"])
        .output()
        .unwrap();
    assert_success(&installed);

    let listed = command(binary, &runtime, &config_home)
        .args(["plugin", "list", "--json"])
        .output()
        .unwrap();
    assert_success(&listed);
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 3);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "dev.base")
    );
    assert!(listed.as_array().unwrap().iter().any(|item| {
        item["id"] == "dev.runner"
            && item["source"] == "https://fixtures.invalid/runner"
            && item["commit"]
                .as_str()
                .is_some_and(|commit| commit.len() == 40)
    }));

    let lock = config_home.join("vvmux/plugins/vvmux-plugin.lock");
    let lock_text = fs::read_to_string(&lock).unwrap();
    assert!(lock_text.contains("id = \"dev.bundle\""));
    assert!(lock_text.contains("id = \"dev.base\""));
    assert!(lock_text.contains("id = \"dev.runner\""));
    assert!(lock_text.contains("commit = "));
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "resolve", "--frozen"])
            .output()
            .unwrap(),
    );
    let invalid_bundle = write_invalid_bundle(directory.path());
    let rejected = command(binary, &runtime, &config_home)
        .args([
            "plugin",
            "install",
            invalid_bundle.to_str().unwrap(),
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("schema_invalid"));
    assert_eq!(fs::read_to_string(&lock).unwrap(), lock_text);
    let after_rejection =
        json_command(command(binary, &runtime, &config_home).args(["plugin", "list", "--json"]));
    assert_eq!(after_rejection.as_array().unwrap().len(), 3);
    let caller = write_dependency_caller(directory.path());
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "link", caller.to_str().unwrap(), "--yes"])
            .output()
            .unwrap(),
    );

    let config = write_config(directory.path());
    let name = format!("plugin-workflow-{}", std::process::id());
    assert_success(
        &command(binary, &runtime, &config_home)
            .args([
                "--config",
                config.to_str().unwrap(),
                "new",
                "-s",
                &name,
                "-d",
            ])
            .output()
            .unwrap(),
    );
    let _guard = SessionGuard {
        binary,
        name: name.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };

    let catalog = json_command(
        command(binary, &runtime, &config_home)
            .args(["plugin", "catalog", "--target", &name, "--json"]),
    );
    let verify = catalog["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["reference"] == "dev.bundle/verify")
        .unwrap();
    assert_eq!(verify["runtime_tier"], "workflow");
    assert_eq!(verify["input_schema"]["required"][0], "value");

    let input = directory.path().join("input.json");
    fs::write(&input, br#"{"value":3}"#).unwrap();
    let result = json_command(command(binary, &runtime, &config_home).args([
        "plugin",
        "invoke",
        "dev.bundle/verify",
        "--target",
        &name,
        "--input",
        &format!("@{}", input.display()),
    ]));
    assert_eq!(result, serde_json::json!({"total": 10}));

    let dependency_call = json_command(command(binary, &runtime, &config_home).args([
        "plugin",
        "invoke",
        "dev.caller/call",
        "--target",
        &name,
        "--input",
        &format!("@{}", input.display()),
    ]));
    assert_eq!(
        dependency_call,
        serde_json::json!({"value": 6, "undeclared_denied": true})
    );

    let detached = json_command(command(binary, &runtime, &config_home).args([
        "plugin",
        "invoke",
        "dev.bundle/verify",
        "--target",
        &name,
        "--input",
        &format!("@{}", input.display()),
        "--detach",
    ]));
    let job_id = detached["job_id"].as_str().unwrap();
    let completed = wait_for_job(binary, &runtime, &config_home, job_id, "succeeded");
    let trace = &completed["trace"];
    assert_eq!(trace["workflow"], "verify");
    assert_eq!(trace["steps"].as_array().unwrap().len(), 4);
    assert!(trace["steps"].as_array().unwrap().iter().all(|step| {
        step["plugin_id"] == "dev.runner"
            && step["plugin_version"] == "1.0.0"
            && step["plugin_digest"]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
    }));

    let slow = json_command(command(binary, &runtime, &config_home).args([
        "plugin",
        "invoke",
        "dev.bundle/slow",
        "--target",
        &name,
        "--detach",
    ]));
    let slow_id = slow["job_id"].as_str().unwrap();
    thread::sleep(Duration::from_millis(100));
    let cancelled = json_command(
        command(binary, &runtime, &config_home).args(["plugin", "job", "cancel", slow_id]),
    );
    assert_eq!(cancelled["status"], "cancelling");
    let cancelled = wait_for_job(binary, &runtime, &config_home, slow_id, "cancelled");
    assert_eq!(cancelled["trace"]["status"], "cancelled");

    let failed = command(binary, &runtime, &config_home)
        .args(["plugin", "invoke", "dev.bundle/fail", "--target", &name])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("dependency_failed"));

    let timed_out = command(binary, &runtime, &config_home)
        .args(["plugin", "invoke", "dev.bundle/deadline", "--target", &name])
        .output()
        .unwrap();
    assert!(!timed_out.status.success());
    assert!(String::from_utf8_lossy(&timed_out.stderr).contains("timeout"));

    let events = replay_events(binary, &runtime, &config_home, &name);
    assert!(
        events
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| {
                event["name"] == "plugin.job_completed"
                    && event["payload"]["action"] == "dev.bundle/on-open"
                    && event["payload"]["status"] == "succeeded"
            })
    );

    fs::write(directory.path().join("start-workflow-firehose"), b"start").unwrap();
    for action in ["list-panes", "reload-config"] {
        let started = Instant::now();
        assert_success(
            &command(binary, &runtime, &config_home)
                .args(["msg", "--target", &name, action])
                .output()
                .unwrap(),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "event workflow firehose delayed {action}"
        );
    }
    let event_workflow_jobs = wait_for_event_workflow_jobs(binary, &runtime, &config_home, &name);
    assert!(
        event_workflow_jobs.len() <= 4,
        "coalescing admitted too many event workflows: {event_workflow_jobs:?}"
    );
    assert!(event_workflow_jobs.iter().any(|job_id| {
        let status = json_command(
            command(binary, &runtime, &config_home).args(["plugin", "job", "status", job_id]),
        );
        status["trace"]["steps"]
            .as_array()
            .is_some_and(|steps| steps.iter().any(|step| step["kind"] == "event_gap"))
    }));

    let installed_bundle = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "dev.bundle")
        .unwrap()["root"]
        .as_str()
        .unwrap();
    fs::write(Path::new(installed_bundle).join("drift"), b"changed").unwrap();
    let frozen = command(binary, &runtime, &config_home)
        .args(["plugin", "resolve", "--frozen"])
        .output()
        .unwrap();
    assert!(!frozen.status.success());
    assert!(String::from_utf8_lossy(&frozen.stderr).contains("lock does not match"));
}

fn write_runner(root: &Path) {
    fs::create_dir_all(root.join("schemas")).unwrap();
    let script = root.join("action.py");
    fs::write(
        &script,
        r#"import json, sys, time
value = json.load(sys.stdin)
operation = sys.argv[1]
if operation == "pass":
    result = value
elif operation == "double":
    result = {"value": value["value"] * 2}
elif operation == "increment":
    result = {"value": value["value"] + 1}
elif operation == "sum":
    result = {"total": value["left"] + value["right"]}
elif operation == "slow":
    time.sleep(5)
    result = {}
elif operation == "event-slow":
    time.sleep(1)
    result = {}
elif operation == "fail":
    raise RuntimeError("fixture failure")
else:
    raise RuntimeError(operation)
json.dump(result, sys.stdout)
"#,
    )
    .unwrap();
    fs::write(
        root.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.runner"
name = "Runner"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Workflow action fixture"
platforms = ["linux", "macos"]
permissions = []
[[dependencies]]
alias = "base"
id = "dev.base"
version = "^1.0"
source = "https://fixtures.invalid/base"
[[actions]]
id = "pass"
title = "Pass"
description = "Pass one value"
command = ["python3", "action.py", "pass"]
input_schema = "schemas/value.json"
output_schema = "schemas/value.json"
[[actions]]
id = "double"
title = "Double"
description = "Double one value"
command = ["python3", "action.py", "double"]
input_schema = "schemas/value.json"
output_schema = "schemas/value.json"
[[actions]]
id = "increment"
title = "Increment"
description = "Increment one value"
command = ["python3", "action.py", "increment"]
input_schema = "schemas/value.json"
output_schema = "schemas/value.json"
[[actions]]
id = "sum"
title = "Sum"
description = "Add two values"
command = ["python3", "action.py", "sum"]
input_schema = "schemas/sum-input.json"
output_schema = "schemas/sum-output.json"
[[actions]]
id = "slow"
title = "Slow"
description = "Wait for cancellation"
command = ["python3", "action.py", "slow"]
input_schema = "schemas/empty.json"
output_schema = "schemas/empty.json"
timeout_ms = 10000
[[actions]]
id = "fail"
title = "Fail"
description = "Fail deterministically"
command = ["python3", "action.py", "fail"]
input_schema = "schemas/empty.json"
output_schema = "schemas/empty.json"
[[actions]]
id = "event-slow"
title = "Event slow"
description = "Delay event workflow delivery"
command = ["python3", "action.py", "event-slow"]
input_schema = "schemas/empty.json"
output_schema = "schemas/empty.json"
timeout_ms = 5000
"#,
    )
    .unwrap();
    fs::write(
        root.join("schemas/value.json"),
        r#"{"type":"object","required":["value"],"properties":{"value":{"type":"integer"}},"additionalProperties":false}"#,
    )
    .unwrap();
    fs::write(
        root.join("schemas/sum-input.json"),
        r#"{"type":"object","required":["left","right"],"properties":{"left":{"type":"integer"},"right":{"type":"integer"}},"additionalProperties":false}"#,
    )
    .unwrap();
    fs::write(
        root.join("schemas/sum-output.json"),
        r#"{"type":"object","required":["total"],"properties":{"total":{"type":"integer"}},"additionalProperties":false}"#,
    )
    .unwrap();
    fs::write(
        root.join("schemas/empty.json"),
        r#"{"type":"object","additionalProperties":false}"#,
    )
    .unwrap();
}

fn write_base(root: &Path) {
    fs::create_dir(root).unwrap();
    fs::write(
        root.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.base"
name = "Base"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Transitive dependency fixture"
platforms = ["linux", "macos"]
permissions = []
"#,
    )
    .unwrap();
}

fn write_bundle(root: &Path) -> PathBuf {
    let bundle = root.join("bundle");
    fs::create_dir_all(bundle.join("schemas")).unwrap();
    fs::write(
        bundle.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.bundle"
name = "Workflow bundle"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "TOML-only workflow fixture"
platforms = ["linux", "macos"]
permissions = ["events.subscribe", "plugin.invoke"]
[[dependencies]]
alias = "runner"
id = "dev.runner"
version = "^1.0"
source = "https://fixtures.invalid/runner"
[[workflows]]
id = "verify"
title = "Verify"
agent_visible = true
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
timeout_ms = 10000
output = "${steps.total.output}"
[[workflows.steps]]
id = "seed"
uses = "runner/pass"
with = { value = "${trigger#/value}" }
[[workflows.steps]]
id = "left"
uses = "runner/double"
with = { value = "${steps.seed.output#/value}" }
[[workflows.steps]]
id = "right"
uses = "runner/increment"
with = { value = "${steps.seed.output#/value}" }
[[workflows.steps]]
id = "total"
uses = "runner/sum"
with = { left = "${steps.left.output#/value}", right = "${steps.right.output#/value}" }
[[workflows]]
id = "slow"
title = "Slow"
timeout_ms = 10000
output = "${steps.wait.output}"
[[workflows.steps]]
id = "wait"
uses = "runner/slow"
with = {}
[[workflows]]
id = "on-open"
title = "On pane open"
trigger = "pane.opened"
timeout_ms = 5000
output = "${steps.record.output}"
[[workflows.steps]]
id = "record"
uses = "runner/pass"
with = { value = 1 }
[[workflows]]
id = "on-screen"
title = "On screen change"
trigger = "pane.screen_changed"
timeout_ms = 10000
output = "${steps.wait.output}"
[[workflows.steps]]
id = "wait"
uses = "runner/event-slow"
with = {}
[[workflows]]
id = "fail"
title = "Fail"
timeout_ms = 5000
output = "${steps.failure.output}"
[[workflows.steps]]
id = "failure"
uses = "runner/fail"
with = {}
[[workflows]]
id = "deadline"
title = "Deadline"
timeout_ms = 100
output = "${steps.wait.output}"
[[workflows.steps]]
id = "wait"
uses = "runner/slow"
with = {}
"#,
    )
    .unwrap();
    fs::write(
        bundle.join("schemas/input.json"),
        r#"{"type":"object","required":["value"],"properties":{"value":{"type":"integer"}},"additionalProperties":false}"#,
    )
    .unwrap();
    fs::write(
        bundle.join("schemas/output.json"),
        r#"{"type":"object","required":["total"],"properties":{"total":{"type":"integer"}},"additionalProperties":false}"#,
    )
    .unwrap();
    bundle
}

fn write_invalid_bundle(root: &Path) -> PathBuf {
    let bundle = root.join("invalid-bundle");
    fs::create_dir(&bundle).unwrap();
    fs::write(
        bundle.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.invalid"
name = "Invalid workflow"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Schema mismatch fixture"
platforms = ["linux", "macos"]
permissions = ["plugin.invoke"]
[[dependencies]]
alias = "runner"
id = "dev.runner"
version = "^1.0"
source = "https://fixtures.invalid/runner"
[[workflows]]
id = "bad"
title = "Bad"
output = "${steps.total.output}"
[[workflows.steps]]
id = "seed"
uses = "runner/pass"
with = { value = 1 }
[[workflows.steps]]
id = "total"
uses = "runner/sum"
with = { left = "${steps.seed.output}", right = 1 }
"#,
    )
    .unwrap();
    bundle
}

fn write_dependency_caller(root: &Path) -> PathBuf {
    let package = root.join("caller");
    fs::create_dir_all(package.join("schemas")).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_dependency_plugin.py"),
        package.join("plugin.py"),
    )
    .unwrap();
    fs::write(
        package.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.caller"
name = "Dependency caller"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Dependency invocation fixture"
platforms = ["linux", "macos"]
permissions = ["plugin.invoke"]
[runtime]
kind = "process"
command = ["python3", "plugin.py"]
activation = "on_demand"
[[dependencies]]
alias = "runner"
id = "dev.runner"
version = "^1.0"
source = "https://fixtures.invalid/runner"
[[actions]]
id = "call"
title = "Call dependency"
description = "Invoke one declared dependency"
handler = "call"
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
timeout_ms = 5000
"#,
    )
    .unwrap();
    fs::write(
        package.join("schemas/input.json"),
        r#"{"type":"object","required":["value"],"properties":{"value":{"type":"integer"}},"additionalProperties":false}"#,
    )
    .unwrap();
    fs::write(
        package.join("schemas/output.json"),
        r#"{"type":"object","required":["value","undeclared_denied"],"properties":{"value":{"type":"integer"},"undeclared_denied":{"type":"boolean"}},"additionalProperties":false}"#,
    )
    .unwrap();
    package
}

fn git_commit(root: &Path) {
    assert_success(&Command::new("git").arg("init").arg(root).output().unwrap());
    assert_success(
        &Command::new("git")
            .args([
                "-C",
                root.to_str().unwrap(),
                "config",
                "user.name",
                "Fixture",
            ])
            .output()
            .unwrap(),
    );
    assert_success(
        &Command::new("git")
            .args([
                "-C",
                root.to_str().unwrap(),
                "config",
                "user.email",
                "fixture@example.invalid",
            ])
            .output()
            .unwrap(),
    );
    assert_success(
        &Command::new("git")
            .args(["-C", root.to_str().unwrap(), "add", "."])
            .output()
            .unwrap(),
    );
    assert_success(
        &Command::new("git")
            .args(["-C", root.to_str().unwrap(), "commit", "-m", "fixture"])
            .output()
            .unwrap(),
    );
}

fn wait_for_job(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    job_id: &str,
    expected: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = json_command(
            command(binary, runtime, config_home).args(["plugin", "job", "status", job_id]),
        );
        if status["status"] == expected {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "job {job_id} stayed in state {}",
            status["status"]
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn replay_events(binary: &str, runtime: &Path, config_home: &Path, name: &str) -> String {
    let mut stream = command(binary, runtime, config_home)
        .args(["plugin", "events", "--target", name, "--after", "0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    stream.kill().unwrap();
    String::from_utf8(stream.wait_with_output().unwrap().stdout).unwrap()
}

fn wait_for_event_workflow_jobs(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    name: &str,
) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(9);
    loop {
        let events = replay_events(binary, runtime, config_home, name);
        let jobs = events
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| {
                event["name"] == "plugin.job_completed"
                    && event["payload"]["action"] == "dev.bundle/on-screen"
            })
            .filter_map(|event| event["payload"]["job_id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let has_gap = jobs.iter().any(|job_id| {
            let status = command(binary, runtime, config_home)
                .args(["plugin", "job", "status", job_id])
                .output()
                .unwrap();
            status.status.success()
                && serde_json::from_slice::<Value>(&status.stdout).is_ok_and(|status| {
                    status["trace"]["steps"]
                        .as_array()
                        .is_some_and(|steps| steps.iter().any(|step| step["kind"] == "event_gap"))
                })
        });
        if jobs.len() >= 2 && has_gap {
            return jobs;
        }
        assert!(
            Instant::now() < deadline,
            "event workflow did not finish a coalesced successor: {events}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn json_command(command: &mut Command) -> Value {
    let output = command.output().unwrap();
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write_config(root: &Path) -> PathBuf {
    let shell = root.join("fixture-shell");
    let marker = root.join("start-workflow-firehose");
    fs::write(
        &shell,
        format!(
            "#!/bin/sh\nprintf 'READY\\n'\nwhile ! test -f {}; do sleep 0.01; done\ni=0\nwhile test \"$i\" -lt 1000; do printf '\\r%04d' \"$i\"; i=$((i + 1)); sleep 0.003; done\nprintf '\\nWORKFLOW-FLOOD-DONE\\n'\nwhile :; do sleep 60; done\n",
            serde_json::to_string(marker.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = {}\nrender_interval_ms = 1\n\n[plugins]\nenabled = true\n",
            serde_json::to_string(shell.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    config
}

fn command(binary: &str, runtime: &Path, config_home: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", config_home)
        .env_remove("VIVID_ENDPOINT")
        .env_remove("VIVID_TOKEN");
    command
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
