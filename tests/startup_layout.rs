#![cfg(unix)]

#[allow(dead_code)]
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;

struct Fixture {
    name: String,
    runtime: tempfile::TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let mut builder = tempfile::Builder::new();
        builder.prefix("vvl-");
        let runtime = builder.tempdir_in("/tmp").unwrap();
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let shell = runtime.path().join("fixture-shell");
        fs::write(
            &shell,
            br#"#!/bin/sh
if [ "$1" = "-c" ]; then
    shift
    exec /bin/sh -c "$@"
fi
printf 'READY pane=%s tab=%s\n' "$VVMUX_PANE_ID" "$VVMUX_TAB_ID"
while IFS= read -r line; do :; done
"#,
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        let config = runtime.path().join("config.toml");
        fs::write(
            &config,
            format!(
                "[general]\nshell = {}\nrender_interval_ms = 1\n",
                serde_json::to_string(shell.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        Self {
            name: format!("layout-{label}-{}", std::process::id()),
            runtime,
            config,
        }
    }

    fn write_layout(&self, name: &str, source: &str) -> PathBuf {
        let path = self.runtime.path().join(name);
        fs::write(&path, source).unwrap();
        path
    }

    fn start(&self, layout: &Path) -> Output {
        common::vvmux_command(self.runtime.path())
            .args(["--config"])
            .arg(&self.config)
            .args(["new", "--session", &self.name, "--detached", "--layout"])
            .arg(layout)
            .output()
            .unwrap()
    }

    fn start_without_layout(&self) -> Output {
        common::vvmux_command(self.runtime.path())
            .args(["--config"])
            .arg(&self.config)
            .args(["new", "--session", &self.name, "--detached"])
            .output()
            .unwrap()
    }

    fn msg(&self, arguments: &[&str]) -> Output {
        common::vvmux_command(self.runtime.path())
            .args(["msg", "--target", &self.name])
            .args(arguments)
            .output()
            .unwrap()
    }

    fn panes(&self) -> Vec<Value> {
        json(self.msg(&["list-panes"]))["panes"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn wait_text(&self, pane: u64, text: &str) {
        assert_success(&self.msg(&[
            "wait",
            "text",
            text,
            "--pane-id",
            &pane.to_string(),
            "--timeout",
            "5s",
        ]));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = common::vvmux_command(self.runtime.path())
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

#[test]
fn nested_layout_starts_named_tab_with_weighted_tiled_panes() {
    let fixture = Fixture::new("nested");
    let layout = fixture.write_layout(
        "nested.toml",
        r#"
[[tabs]]
name = "dev"
focus = "shell"
[tabs.layout]
split = "vertical"
sizes = [30, 70]
[[tabs.layout.children]]
pane = "editor"
command = "printf 'PANE editor\n'; sleep 30"
[[tabs.layout.children]]
split = "horizontal"
sizes = [60, 40]
[[tabs.layout.children.children]]
pane = "shell"
command = "printf 'PANE shell\n'; sleep 30"
[[tabs.layout.children.children]]
pane = "logs"
command = "printf 'PANE logs\n'; sleep 30"
"#,
    );
    assert_success(&fixture.start(&layout));

    let panes = fixture.panes();
    assert_eq!(panes.len(), 3);
    assert!(panes.iter().all(|pane| pane["tab_id"] == 1));
    assert!(panes.iter().all(|pane| pane["tab_name"] == "dev"));
    assert_eq!(
        panes.iter().find(|pane| pane["focused"] == true).unwrap()["pane_id"],
        2
    );
    fixture.wait_text(1, "PANE editor");
    fixture.wait_text(2, "PANE shell");
    fixture.wait_text(3, "PANE logs");
}

#[test]
fn floats_only_layout_starts_and_offsets_each_float() {
    let fixture = Fixture::new("floats");
    let layout = fixture.write_layout(
        "floats.toml",
        r#"
[[tabs]]
name = "notes"
focus = "second"
[[tabs.floating]]
pane = "first"
command = "printf 'FLOAT first\n'; sleep 30"
width_percent = 50
[[tabs.floating]]
pane = "second"
command = "printf 'FLOAT second\n'; sleep 30"
width_percent = 50
pinned = true
"#,
    );
    assert_success(&fixture.start(&layout));
    let panes = fixture.panes();
    assert_eq!(panes.len(), 2);
    assert_eq!(panes[0]["layer"], "floating");
    assert_eq!(panes[1]["layer"], "pinned");
    assert_ne!(panes[0]["geometry"], panes[1]["geometry"]);
    fixture.wait_text(1, "FLOAT first");
    fixture.wait_text(2, "FLOAT second");
}

#[test]
fn partial_failure_is_owner_scoped_and_the_other_session_keeps_updating() {
    let owner_b = Fixture::new("owner-b");
    let healthy = owner_b.write_layout(
        "healthy.toml",
        r#"
[[tabs]]
[tabs.layout]
split = "vertical"
[[tabs.layout.children]]
pane = "one"
command = "printf 'OWNER_B one\n'; sleep 30"
[[tabs.layout.children]]
pane = "two"
command = "printf 'OWNER_B two\n'; sleep 30"
"#,
    );
    assert_success(&owner_b.start(&healthy));
    owner_b.wait_text(1, "OWNER_B one");
    owner_b.wait_text(2, "OWNER_B two");

    let owner_a = Fixture::new("owner-a");
    let missing = owner_a.runtime.path().join("does-not-exist");
    let partial = owner_a.write_layout(
        "partial.toml",
        &format!(
            r#"
[[tabs]]
[tabs.layout]
split = "vertical"
[[tabs.layout.children]]
pane = "broken"
cwd = {}
[[tabs.layout.children]]
pane = "survivor"
command = "printf 'OWNER_A survivor\n'; sleep 30"
"#,
            serde_json::to_string(missing.to_str().unwrap()).unwrap()
        ),
    );
    assert_success(&owner_a.start(&partial));
    let owner_a_panes = owner_a.panes();
    assert_eq!(owner_a_panes.len(), 1);
    assert_eq!(owner_a_panes[0]["pane_id"], 2);
    assert_eq!(owner_a_panes[0]["tab_id"], 1);
    owner_a.wait_text(2, "OWNER_A survivor");

    // Both owners deliberately reused pane IDs 1/2 and tab ID 1. Owner A's failed pane must not
    // change owner B's panes, and owner B's next valid update must still succeed.
    assert_eq!(
        owner_b
            .panes()
            .iter()
            .map(|pane| pane["pane_id"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    owner_b.wait_text(1, "OWNER_B one");
    owner_b.wait_text(2, "OWNER_B two");
    let opened = json(owner_b.msg(&[
        "run",
        "printf 'OWNER_B next\\n'; sleep 30",
        "--pane-id",
        "1",
    ]));
    assert_eq!(opened["pane_id"], 3);
    owner_b.wait_text(3, "OWNER_B next");
}

#[test]
fn missing_explicit_layout_fails_without_creating_a_session() {
    let fixture = Fixture::new("missing");
    let created = common::vvmux_command(fixture.runtime.path())
        .args(["--config"])
        .arg(&fixture.config)
        .args([
            "new",
            "--session",
            &fixture.name,
            "--detached",
            "--layout",
            "does-not-exist",
        ])
        .output()
        .unwrap();
    assert!(!created.status.success());
    let listed = common::vvmux_command(fixture.runtime.path())
        .arg("list")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&listed.stdout).contains(&fixture.name));
}

#[test]
fn four_pane_stack_starts_before_a_real_display_is_attached() {
    let fixture = Fixture::new("stack");
    let children = (1..=4)
        .map(|pane| {
            format!(
                "[[tabs.layout.children]]\npane='p{pane}'\ncommand=\"printf 'STACK {pane}\\\\n'; sleep 30\"\n"
            )
        })
        .collect::<String>();
    let layout = fixture.write_layout(
        "stack.toml",
        &format!("[[tabs]]\n[tabs.layout]\nsplit='horizontal'\nsizes=[1,1,1,1]\n{children}"),
    );
    assert_success(&fixture.start(&layout));
    assert_eq!(fixture.panes().len(), 4);
}

#[test]
fn named_default_layout_resolves_from_the_config_directory() {
    let fixture = Fixture::new("default");
    let layouts = fixture.runtime.path().join("vvmux/layouts");
    fs::create_dir_all(&layouts).unwrap();
    fs::set_permissions(
        fixture.runtime.path().join("vvmux"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(
        layouts.join("dev.toml"),
        "[[tabs]]\nname='default-dev'\n[[tabs.floating]]\npane='notes'\ncommand=\"printf 'DEFAULT_LAYOUT\\n'; sleep 30\"\n",
    )
    .unwrap();
    let original = fs::read_to_string(&fixture.config).unwrap();
    fs::write(
        &fixture.config,
        original.replace("[general]\n", "[general]\ndefault_layout = 'dev'\n"),
    )
    .unwrap();

    assert_success(&fixture.start_without_layout());
    let panes = fixture.panes();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0]["tab_name"], "default-dev");
    fixture.wait_text(1, "DEFAULT_LAYOUT");
}

#[test]
fn missing_default_layout_warns_and_falls_back_to_one_shell() {
    let fixture = Fixture::new("default-missing");
    let original = fs::read_to_string(&fixture.config).unwrap();
    fs::write(
        &fixture.config,
        original.replace(
            "[general]\n",
            "[general]\ndefault_layout = 'not-installed'\n",
        ),
    )
    .unwrap();

    let created = fixture.start_without_layout();
    assert_success(&created);
    assert!(
        String::from_utf8_lossy(&created.stderr).contains("was not found"),
        "stderr was {}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert_eq!(fixture.panes().len(), 1);
    fixture.wait_text(1, "READY pane=1 tab=1");
}

#[test]
fn a_fully_failed_plan_falls_back_to_a_live_shell_tab() {
    let fixture = Fixture::new("all-failed");
    let missing = fixture.runtime.path().join("missing-cwd");
    let layout = fixture.write_layout(
        "all-failed.toml",
        &format!(
            "[[tabs]]\n[[tabs.floating]]\npane='broken'\ncwd={}\n",
            serde_json::to_string(missing.to_str().unwrap()).unwrap()
        ),
    );
    assert_success(&fixture.start(&layout));
    let panes = fixture.panes();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0]["pane_id"], 2, "the failed slot remains consumed");
    fixture.wait_text(2, "READY pane=2 tab=2");
}

#[test]
fn multiple_layout_tabs_keep_their_names_and_tab_ids() {
    let fixture = Fixture::new("tabs");
    let layout = fixture.write_layout(
        "tabs.toml",
        r#"
[[tabs]]
name = "one"
[[tabs.floating]]
pane = "first"
command = "printf 'TAB one\n'; sleep 30"

[[tabs]]
name = "two"
[tabs.layout]
pane = "second"
command = "printf 'TAB two\n'; sleep 30"
"#,
    );
    assert_success(&fixture.start(&layout));
    let panes = fixture.panes();
    assert_eq!(panes.len(), 2);
    assert_eq!(panes[0]["tab_id"], 1);
    assert_eq!(panes[0]["tab_name"], "one");
    assert_eq!(panes[1]["tab_id"], 2);
    assert_eq!(panes[1]["tab_name"], "two");
    fixture.wait_text(1, "TAB one");
    fixture.wait_text(2, "TAB two");
}

fn json(output: Output) -> Value {
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
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
