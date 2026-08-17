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

    /// Install the conventional `<config dir>/startup.toml`. `vvmux_command` points
    /// `XDG_CONFIG_HOME` at the runtime directory, so the config directory is `<runtime>/vvmux`.
    fn write_startup_layout(&self, source: &str) -> PathBuf {
        let directory = self.runtime.path().join("vvmux");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("startup.toml");
        fs::write(&path, source).unwrap();
        path
    }

    fn set_default_layout(&self, name: &str) {
        let original = fs::read_to_string(&self.config).unwrap();
        fs::write(
            &self.config,
            original.replace(
                "[general]\n",
                &format!("[general]\ndefault_layout = '{name}'\n"),
            ),
        )
        .unwrap();
    }

    fn write_named_layout(&self, name: &str, source: &str) -> PathBuf {
        let layouts = self.runtime.path().join("vvmux/layouts");
        fs::create_dir_all(&layouts).unwrap();
        fs::set_permissions(
            self.runtime.path().join("vvmux"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let path = layouts.join(format!("{name}.toml"));
        fs::write(&path, source).unwrap();
        path
    }

    fn tabs(&self) -> Vec<Value> {
        json(self.msg(&["list-tabs"]))["tabs"]
            .as_array()
            .unwrap()
            .clone()
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

    fn kill(&self) {
        assert_success(
            &common::vvmux_command(self.runtime.path())
                .args(["kill-session", "--target", &self.name])
                .output()
                .unwrap(),
        );
        // The daemon writes its final snapshot and reaps its panes after the request is answered,
        // so a restart racing that write would read a file from one change ago.
        for _ in 0..100 {
            if common::vvmux_command(self.runtime.path())
                .args(["msg", "--target", &self.name, "list-panes"])
                .output()
                .is_ok_and(|output| !output.status.success())
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
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
    fixture.write_named_layout(
        "dev",
        "[[tabs]]\nname='default-dev'\n[[tabs.floating]]\npane='notes'\ncommand=\"printf 'DEFAULT_LAYOUT\\n'; sleep 30\"\n",
    );
    fixture.set_default_layout("dev");

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

#[test]
fn startup_toml_applies_to_a_session_created_without_a_layout() {
    let fixture = Fixture::new("startup");
    fixture.write_startup_layout(
        r#"
[[tabs]]
name = "left-right"
focus = "right"
[tabs.layout]
split = "vertical"
sizes = [40, 60]
[[tabs.layout.children]]
pane = "left"
command = "printf 'STARTUP left\n'; sleep 30"
[[tabs.layout.children]]
pane = "right"
command = "printf 'STARTUP right\n'; sleep 30"

[[tabs]]
name = "top-bottom"
[tabs.layout]
split = "horizontal"
[[tabs.layout.children]]
pane = "top"
command = "printf 'STARTUP top\n'; sleep 30"
[[tabs.layout.children]]
pane = "bottom"
command = "printf 'STARTUP bottom\n'; sleep 30"
"#,
    );

    assert_success(&fixture.start_without_layout());
    let panes = fixture.panes();
    assert_eq!(panes.len(), 4);
    assert_eq!(panes[0]["tab_name"], "left-right");
    assert_eq!(panes[3]["tab_name"], "top-bottom");
    assert_eq!(
        panes.iter().find(|pane| pane["focused"] == true).unwrap()["pane_id"],
        2
    );
    fixture.wait_text(1, "STARTUP left");
    fixture.wait_text(4, "STARTUP bottom");
}

#[test]
fn startup_toml_outranks_the_default_layout_but_not_an_explicit_one() {
    let fixture = Fixture::new("startup-precedence");
    fixture.write_startup_layout(
        "[[tabs]]\nname='from-startup'\n[[tabs.floating]]\npane='p1'\ncommand=\"sleep 30\"\n",
    );
    fixture.write_named_layout(
        "dev",
        "[[tabs]]\nname='from-default'\n[[tabs.floating]]\npane='p1'\ncommand=\"sleep 30\"\n",
    );
    fixture.set_default_layout("dev");
    assert_success(&fixture.start_without_layout());
    assert_eq!(fixture.panes()[0]["tab_name"], "from-startup");

    let explicit = Fixture::new("startup-explicit");
    explicit.write_startup_layout(
        "[[tabs]]\nname='from-startup'\n[[tabs.floating]]\npane='p1'\ncommand=\"sleep 30\"\n",
    );
    let named = explicit.write_named_layout(
        "chosen",
        "[[tabs]]\nname='from-flag'\n[[tabs.floating]]\npane='p1'\ncommand=\"sleep 30\"\n",
    );
    assert_success(&explicit.start(&named));
    assert_eq!(explicit.panes()[0]["tab_name"], "from-flag");
}

/// `startup.toml` is implicit and has no bypass flag, so a broken one must never make a session
/// unlaunchable.
#[test]
fn invalid_startup_toml_warns_and_falls_back_to_one_shell() {
    let fixture = Fixture::new("startup-invalid");
    fixture.write_startup_layout("[[tabs]]\n[tabs.layout]\npane='a'\nsplit='vertical'\n");

    let created = fixture.start_without_layout();
    assert_success(&created);
    let stderr = String::from_utf8_lossy(&created.stderr);
    assert!(stderr.contains("startup.toml"), "stderr was {stderr}");
    assert!(stderr.contains("ignoring"), "stderr was {stderr}");
    assert_eq!(fixture.panes().len(), 1);
    fixture.wait_text(1, "READY pane=1 tab=1");
}

#[test]
fn a_saved_layout_reproduces_the_live_tabs_and_panes() {
    let source = Fixture::new("save");
    assert_success(&source.start_without_layout());
    assert_eq!(
        json(source.msg(&["split", "vertical", "--pane-id", "1"]))["new_pane_id"],
        2
    );
    assert_success(&source.msg(&["action", "new-tab"]));
    let before = source.panes();
    assert_eq!(before.len(), 3);

    let saved = json(source.msg(&["save-layout"]));
    assert_eq!(saved["tabs"], 2);
    assert_eq!(saved["panes"], 3);
    let path = PathBuf::from(saved["path"].as_str().unwrap());
    assert_eq!(path, source.runtime.path().join("vvmux/startup.toml"));
    let captured = fs::read_to_string(&path).unwrap();

    // Saving reports the session; it must not touch it. The panes, their tabs, and focus are
    // unchanged, and the next ordinary mutation still lands.
    assert_eq!(pane_identity(&source.panes()), pane_identity(&before));
    assert_eq!(
        json(source.msg(&["split", "vertical", "--pane-id", "1"]))["new_pane_id"],
        4
    );

    let replayed = Fixture::new("save-replay");
    replayed.write_startup_layout(&captured);
    assert_success(&replayed.start_without_layout());
    let panes = replayed.panes();
    assert_eq!(panes.len(), 3);
    let tabs = replayed.tabs();
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0]["pane_ids"], serde_json::json!([1, 2]));
    assert_eq!(tabs[1]["pane_ids"], serde_json::json!([3]));
    assert_eq!(
        tabs[0]["focused_pane_id"], 2,
        "the saved focus is restored per tab"
    );
    assert_eq!(tabs[1]["focused_pane_id"], 3);
}

#[test]
fn a_failed_save_reports_the_error_and_leaves_the_session_intact() {
    let fixture = Fixture::new("save-failure");
    assert_success(&fixture.start_without_layout());
    let before = fixture.panes();

    let blocked = fixture.runtime.path().join("blocked");
    fs::write(&blocked, b"not a directory").unwrap();
    let target = blocked.join("layout.toml");
    let failed = fixture.msg(&["save-layout", "--path", target.to_str().unwrap()]);
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("save_failed"),
        "stderr was {}",
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(!target.exists());

    assert_eq!(pane_identity(&fixture.panes()), pane_identity(&before));
    assert_eq!(
        json(fixture.msg(&["split", "vertical", "--pane-id", "1"]))["new_pane_id"],
        2
    );
}

/// Pane JSON carries volatile fields (`cursor` advances as shell output lands, and
/// `screen_sequence`/`session_sequence` bump on every command), so comparing two snapshots
/// byte-for-byte races. Reduce each pane to the structural fields a save must not disturb.
fn pane_identity(panes: &[Value]) -> Vec<Value> {
    panes
        .iter()
        .map(|pane| {
            serde_json::json!({
                "pane_id": pane["pane_id"],
                "tab_id": pane["tab_id"],
                "geometry": pane["geometry"],
                "content_geometry": pane["content_geometry"],
                "layer": pane["layer"],
                "focused": pane["focused"],
                "columns": pane["columns"],
                "rows": pane["rows"],
            })
        })
        .collect()
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

/// The shape a session had must come back when its server does, with no user ritual and no layout
/// file — which is the whole point, since a layout file is the ritual.
#[test]
fn a_session_restores_the_shape_it_had_when_its_server_restarts() {
    let fixture = Fixture::new("restore");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1 tab=1");

    // A shape nothing would produce by default: an uneven split, a nested one, a second tab, and a
    // zoomed pane that is not the one focus would land on.
    assert_success(&fixture.msg(&["split", "vertical", "--pane-id", "1"]));
    assert_success(&fixture.msg(&["split", "horizontal", "--pane-id", "1"]));
    assert_success(&fixture.msg(&["action", "new-tab"]));
    for pane in 1..=4 {
        fixture.wait_text(pane, &format!("READY pane={pane}"));
    }
    assert_success(&fixture.msg(&["action", "toggle-zoom", "--pane-id", "2"]));

    let before = shape(&fixture);
    assert_eq!(before.len(), 4, "expected four panes: {before:?}");
    assert!(
        before.iter().any(|pane| pane.contains("\"zoomed\":true")),
        "setup did not zoom a pane: {before:?}"
    );

    let snapshot = json(fixture.msg(&["snapshot"]));
    assert_eq!(snapshot["enabled"], true);
    assert_eq!(snapshot["restored_from_snapshot"], false);
    let path = PathBuf::from(snapshot["path"].as_str().unwrap());

    // Force the debounced write rather than sleeping through it: the shutdown path writes inline.
    fixture.kill();
    assert!(path.exists(), "no snapshot at {}", path.display());
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "a snapshot records working directories and agent identity"
    );

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    let after = shape(&fixture);
    assert_eq!(
        after, before,
        "the restored session does not have the shape it had"
    );
    assert_eq!(
        json(fixture.msg(&["snapshot"]))["restored_from_snapshot"],
        true
    );
}

/// Nothing here may make a session fail to start. A snapshot is a file, and a file can be anything.
#[test]
fn an_unusable_snapshot_starts_a_fresh_session_instead_of_failing() {
    for (label, contents) in [
        ("garbage", "not json at all"),
        ("truncated", "{\"schema\":1,\"layout\":"),
        (
            "newer",
            "{\"schema\":9999,\"layout\":{\"tabs\":[]},\"extras\":{}}",
        ),
        (
            "empty-layout",
            "{\"schema\":1,\"layout\":{\"tabs\":[]},\"extras\":{}}",
        ),
    ] {
        let fixture = Fixture::new(&format!("bad-{label}"));
        // Start once so the state directory exists and the snapshot path is known, then replace it.
        assert_success(&fixture.start_without_layout());
        fixture.wait_text(1, "READY pane=1");
        let path = PathBuf::from(json(fixture.msg(&["snapshot"]))["path"].as_str().unwrap());
        fixture.kill();

        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert_success(&fixture.start_without_layout());
        fixture.wait_text(1, "READY pane=1");
        let panes = fixture.panes();
        assert_eq!(panes.len(), 1, "{label}: expected one fresh shell");
        assert_eq!(
            json(fixture.msg(&["snapshot"]))["restored_from_snapshot"],
            false,
            "{label}: claimed to restore from an unusable snapshot"
        );
    }
}

/// An explicit `--layout` is a request, and a request outranks whatever the session used to be.
#[test]
fn an_explicit_layout_outranks_a_snapshot() {
    let fixture = Fixture::new("precedence");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert_success(&fixture.msg(&["split", "vertical", "--pane-id", "1"]));
    fixture.wait_text(2, "READY pane=2");
    fixture.kill();

    let layout = fixture.write_layout(
        "one.toml",
        "[[tabs]]\nname = \"asked-for\"\n[tabs.layout]\npane = \"only\"\n",
    );
    assert_success(&fixture.start(&layout));
    fixture.wait_text(1, "READY pane=1");
    let panes = fixture.panes();
    assert_eq!(panes.len(), 1, "the snapshot won over an explicit --layout");
    assert_eq!(panes[0]["tab_name"], "asked-for");
}

/// Opting out must also discard what was already written, or the setting would only stop new
/// snapshots while an old one kept restoring.
#[test]
fn turning_snapshots_off_discards_the_one_on_disk() {
    let fixture = Fixture::new("optout");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert_success(&fixture.msg(&["split", "vertical", "--pane-id", "1"]));
    fixture.wait_text(2, "READY pane=2");
    let path = PathBuf::from(json(fixture.msg(&["snapshot"]))["path"].as_str().unwrap());
    fixture.kill();
    assert!(path.exists());

    let original = fs::read_to_string(&fixture.config).unwrap();
    fs::write(
        &fixture.config,
        format!("{original}\n[session]\nauto_snapshot = false\n"),
    )
    .unwrap();

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    let status = json(fixture.msg(&["snapshot"]));
    assert_eq!(status["enabled"], false);
    assert_eq!(status["restored_from_snapshot"], false);
    assert_eq!(
        fixture.panes().len(),
        1,
        "a disabled session still restored"
    );
    // Not merely unread: a snapshot holds the directories a user was working in, so opting out has
    // to remove it rather than leave it for whenever the setting is turned back on.
    assert!(!path.exists(), "opting out left {} on disk", path.display());
}

/// The identity a caller can compare across a restart.
///
/// Sorted, because pane IDs do not survive a restart and are not meant to: `apply_layout_plan`
/// assigns them in the order the layout tree is walked, exactly as it does for `startup.toml`, so a
/// pane that was second by ID can come back third. Everything that describes the *shape* — geometry,
/// zoom, focus, and which tab a pane is in — still has to match exactly, and does. This is also why
/// agent names exist: they are the durable target a pane ID cannot be.
fn shape(fixture: &Fixture) -> Vec<String> {
    let mut shape = fixture
        .panes()
        .iter()
        .map(|pane| {
            serde_json::json!({
                "tab_name": pane["tab_name"],
                "width": pane["geometry"]["width"],
                "height": pane["geometry"]["height"],
                "zoomed": pane["zoomed"],
                "focused": pane["focused"],
                "active_tab": pane["active_tab"],
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    shape.sort();
    shape
}
