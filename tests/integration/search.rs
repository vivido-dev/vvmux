#![cfg(unix)]

//! Detached end-to-end coverage for `vvmux msg search`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

use serde_json::Value;

struct Fixture {
    binary: &'static str,
    name: String,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn start() -> Self {
        let binary = env!("CARGO_BIN_EXE_vvmux");
        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("search-shell");
        fs::write(
            &shell,
            br#"#!/bin/sh
i=0
while [ "$i" -lt 300 ]; do
    printf 'line %03d\r\n' "$i"
    i=$((i + 1))
done
while IFS= read -r line; do :; done
"#,
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        let config = directory.path().join("vvmux.toml");
        fs::write(
            &config,
            format!(
                "[general]\nshell = {}\nrender_interval_ms = 1\nscrollback_lines = 1000\n",
                serde_json::to_string(shell.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        let name = format!("search-{}", std::process::id());
        let created = Command::new(binary)
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
        let fixture = Self {
            binary,
            name,
            _directory: directory,
        };
        assert_success(&fixture.msg(&[
            "wait",
            "text",
            "line 299",
            "--pane-id",
            "1",
            "--timeout",
            "5s",
        ]));
        fixture
    }

    fn msg(&self, arguments: &[&str]) -> Output {
        Command::new(self.binary)
            .args(["msg", "--target", &self.name])
            .args(arguments)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(self.binary)
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

#[test]
fn detached_search_reports_scrollback_regex_limits_and_errors() {
    let fixture = Fixture::start();
    let inspected = json(fixture.msg(&["inspect", "--pane-id", "1"]));
    let history = inspected["pane"]["history_size"].as_i64().unwrap();

    let literal = json(fixture.msg(&["search", "--pattern", "line 042", "--pane-id", "1"]));
    let matches = literal["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["line"], 42 - history);
    assert_eq!(matches[0]["start_column"], 0);
    assert_eq!(matches[0]["end_column"], 8);
    assert_eq!(matches[0]["text"], "line 042");
    assert_eq!(literal["truncated"], false);

    let regex = json(fixture.msg(&[
        "search",
        "--pattern",
        r"line 04\d",
        "--regex",
        "--pane-id",
        "1",
    ]));
    assert_eq!(regex["matches"].as_array().unwrap().len(), 10);

    let limited = json(fixture.msg(&[
        "search",
        "--pattern",
        "line",
        "--limit",
        "2",
        "--pane-id",
        "1",
    ]));
    assert_eq!(limited["matches"].as_array().unwrap().len(), 2);
    assert_eq!(limited["truncated"], true);

    let invalid = fixture.msg(&["search", "--pattern", "(", "--regex", "--pane-id", "1"]);
    assert!(!invalid.status.success());
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("invalid_params"),
        "{}",
        String::from_utf8_lossy(&invalid.stderr)
    );
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
