#![cfg(unix)]

use crate::common;

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
        // Named `sh` because that is what it is: a POSIX shell wrapper. The name matters — vvmux
        // recognizes a pane's shell by it, and refuses to type a command line at one whose quoting
        // rules it does not implement.
        let shell = runtime.path().join("sh");
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

    /// vvmux ships no agent providers of its own, so a test that reports or resumes an agent has
    /// to install one first. The catalog still reaches the session by registry event, which is why
    /// `msg_when_agents_ready` stays.
    fn with_agents(label: &str) -> Self {
        let fixture = Self::new(label);
        common::install_agent_providers(fixture.runtime.path(), &["codex"]);
        fixture
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

    /// Swap the pane shell for one that actually runs what is typed at it.
    ///
    /// The default fixture reads its input and discards it, which is right for tests that only need
    /// a pane to exist. A resume is *typed at a shell*, so testing one needs a shell that executes.
    /// Still named `sh`, because vvmux recognizes a pane's shell by name before quoting for it.
    fn use_executing_shell(&self) {
        let shell = self.runtime.path().join("sh");
        fs::write(
            &shell,
            br#"#!/bin/sh
if [ "$1" = "-c" ]; then
    shift
    exec /bin/sh -c "$@"
fi
printf 'READY pane=%s tab=%s
' "$VVMUX_PANE_ID" "$VVMUX_TAB_ID"
while IFS= read -r line; do
    eval "$line"
done
"#,
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn append_config(&self, section: &str) {
        let original = fs::read_to_string(&self.config).unwrap();
        fs::write(&self.config, format!("{original}\n{section}\n")).unwrap();
    }

    /// Attach through a real pty, long enough for the server to apply geometry and act on it.
    ///
    /// A resume deliberately waits for an attach, so a test of one has to produce a genuine
    /// attachment: a pipe is refused for having zero dimensions, which is exactly right and exactly
    /// unhelpful here.
    fn attach_briefly(&self, seconds: u64) {
        let script = format!(
            "import pty,os,time,fcntl,termios,struct,sys\n\
             pid,fd = pty.fork()\n\
             if pid == 0:\n\
             \x20   os.environ['XDG_RUNTIME_DIR']={runtime:?}\n\
             \x20   os.environ['XDG_CONFIG_HOME']={runtime:?}\n\
             \x20   os.environ['XDG_STATE_HOME']={state:?}\n\
             \x20   os.environ['HOME']={runtime:?}\n\
             \x20   os.execvp({binary:?}, [{binary:?},'attach','--target',{name:?}])\n\
             fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))\n\
             time.sleep({seconds})\n\
             os.kill(pid, 15)\n",
            runtime = self.runtime.path().to_str().unwrap(),
            state = self.runtime.path().join("state").to_str().unwrap(),
            binary = env!("CARGO_BIN_EXE_vvmux"),
            name = self.name,
        );
        let status = std::process::Command::new("python3")
            .arg("-c")
            .arg(script)
            .status()
            .unwrap();
        assert!(status.success(), "attach helper failed");
    }

    fn enable_pane_history(&self) {
        let original = fs::read_to_string(&self.config).unwrap();
        fs::write(
            &self.config,
            format!("{original}\n[session]\npane_history = true\n"),
        )
        .unwrap();
    }

    /// Start a single pane that scrolls enough output past to reach scrollback.
    ///
    /// A pane only pushes lines into history when the screen *scrolls*, so a handful of lines on a
    /// 23-row grid reaches none. Driven by a layout command rather than by `submit`, because this
    /// file's fixture shell reads its input and discards it rather than running it.
    fn start_scrolling(&self, tag: &str) -> Output {
        // A TOML *literal* string: the command is full of `$` and `\n`, and a basic string would
        // process the escapes before the shell ever sees them.
        let layout = self.write_layout(
            &format!("{tag}.toml"),
            &format!(
                "[[tabs]]\n[tabs.layout]\npane = \"noisy\"\ncommand = 'i=1; while [ $i -le 60 ]; do printf \"{tag}-$i\\n\"; i=$((i+1)); done; sleep 300'\n"
            ),
        );
        self.start(&layout)
    }

    fn start_without_layout(&self) -> Output {
        let mut command = common::vvmux_command(self.runtime.path());
        // The daemon's PATH is what a pane's shell resolves a bare command name against, so a
        // fixture executable has to be on it. `HOME` is already isolated by the harness, which is
        // what stops a developer's profile from reordering this out from under the test.
        let bin = self.runtime.path().join("bin");
        if bin.is_dir() {
            command.env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        }
        command
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

    /// Run a `msg` command, retrying while the agent catalog is still being compiled.
    ///
    /// The builtin providers arrive from the plugin registry by event, after the session is already
    /// answering requests, so a report sent immediately after `new` can be refused with
    /// `agent definition is not enabled`. Retrying the same call is safe: a rejected report does not
    /// consume its sequence slot, so the retry is identical rather than merely similar.
    fn msg_when_agents_ready(&self, arguments: &[&str]) -> Output {
        for _ in 0..100 {
            let output = self.msg(arguments);
            if output.status.success() {
                return output;
            }
            if !String::from_utf8_lossy(&output.stderr).contains("agent definition is not enabled")
            {
                return output;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.msg(arguments)
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

/// A detached start lays floats out against the 80x24 placeholder host, because no client has
/// attached yet. Their birth percentages must re-proportion onto the real host at attach — and
/// the optional position percents must place the top-left edge — instead of keeping the
/// placeholder's 56-column idea of `width_percent = 70` forever.
#[test]
fn startup_float_reproportions_to_the_attached_display() {
    let fixture = Fixture::new("float-size");
    let layout = fixture.write_layout(
        "float-size.toml",
        r#"
[[tabs]]
[[tabs.floating]]
pane = "sized"
command = "printf 'FLOAT sized\n'; sleep 30"
width_percent = 70
height_percent = 70
x_percent = 10
y_percent = 20
"#,
    );
    assert_success(&fixture.start(&layout));

    // Detached, everything is measured against the placeholder 80x23 content area.
    let geometry = fixture.panes()[0]["geometry"].clone();
    assert_eq!(geometry["width"], 56, "{geometry}");
    assert_eq!(geometry["height"], 16, "{geometry}");
    assert_eq!(geometry["x"], 8, "{geometry}");
    assert_eq!(geometry["y"], 4, "{geometry}");

    // A 120x40 client attaches: the same percents now describe 120x39 of content.
    fixture.attach_briefly(2);
    let geometry = fixture.panes()[0]["geometry"].clone();
    assert_eq!(geometry["width"], 84, "{geometry}");
    assert_eq!(geometry["height"], 27, "{geometry}");
    assert_eq!(geometry["x"], 12, "{geometry}");
    assert_eq!(geometry["y"], 7, "{geometry}");
    fixture.wait_text(1, "FLOAT sized");
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

/// Opt-in pane history brings back what was on a pane's screen, as scrollback rather than as a
/// screen: the viewport belongs to the shell that just started.
#[test]
fn opt_in_pane_history_restores_scrollback_without_replaying_it() {
    let fixture = Fixture::new("history");
    fixture.enable_pane_history();
    assert_success(&fixture.start_scrolling("HIST"));
    fixture.wait_text(1, "HIST-60");

    let before = history_size(&fixture, 1);
    assert!(before > 0, "nothing reached scrollback to persist");
    let history = PathBuf::from(
        json(fixture.msg(&["snapshot"]))["path"]
            .as_str()
            .unwrap()
            .replace("snapshot-", "history-"),
    );
    fixture.kill();
    assert!(history.exists(), "no history at {}", history.display());
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&history).unwrap().permissions().mode() & 0o777,
        0o600,
        "pane output is whatever scrolled past, including secrets"
    );

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert_eq!(
        history_size(&fixture, 1),
        before,
        "the restored pane did not get its scrollback back"
    );
    // `HIST-1` is a line that genuinely scrolled off before the restart, so it is the one that has
    // to come back. The trailing lines never left the viewport and so were never in scrollback —
    // history holds what scrolled past, not what was on screen.
    //
    // More rows than the screen holds, so the read reaches past the viewport into the restored
    // scrollback rather than reporting the fresh shell's blank grid.
    let recent = text(fixture.msg(&[
        "get-text",
        "--pane-id",
        "1",
        "--source",
        "recent",
        "--rows",
        "80",
    ]));
    assert!(
        recent.contains("HIST-1\n"),
        "restored history is not in scrollback: {recent:?}"
    );
    // Restored into scrollback, not onto the screen: the fresh shell's own output owns the
    // viewport, and the old lines sit above it.
    let visible = text(fixture.msg(&["get-text", "--pane-id", "1"]));
    assert!(
        !visible.contains("HIST-1"),
        "restored history was painted onto the live screen: {visible:?}"
    );
    assert!(
        visible.contains("READY pane=1"),
        "the restored pane is not a live shell: {visible:?}"
    );
}

/// Off by default, because pane output is whatever scrolled past.
#[test]
fn pane_history_is_off_unless_asked_for() {
    let fixture = Fixture::new("nohistory");
    assert_success(&fixture.start_scrolling("SECRET"));
    fixture.wait_text(1, "SECRET-60");
    assert!(history_size(&fixture, 1) > 0);
    let history = PathBuf::from(
        json(fixture.msg(&["snapshot"]))["path"]
            .as_str()
            .unwrap()
            .replace("snapshot-", "history-"),
    );
    fixture.kill();
    assert!(
        !history.exists(),
        "pane output was written to {} without being asked for",
        history.display()
    );

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert_eq!(history_size(&fixture, 1), 0, "scrollback came back anyway");
}

/// Turning it off has to take the data with it. Output alone never marks the shape dirty, so
/// waiting for the next ordinary write would keep the file indefinitely on a session that only
/// scrolls.
#[test]
fn turning_pane_history_off_discards_what_was_written() {
    let fixture = Fixture::new("histoff");
    fixture.enable_pane_history();
    assert_success(&fixture.start_scrolling("GONE"));
    fixture.wait_text(1, "GONE-60");
    let history = PathBuf::from(
        json(fixture.msg(&["snapshot"]))["path"]
            .as_str()
            .unwrap()
            .replace("snapshot-", "history-"),
    );
    fixture.kill();
    assert!(history.exists());

    let config = fs::read_to_string(&fixture.config).unwrap();
    fs::write(
        &fixture.config,
        config.replace("pane_history = true", "pane_history = false"),
    )
    .unwrap();
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert!(
        !history.exists(),
        "opting out left pane output at {}",
        history.display()
    );
    assert_eq!(history_size(&fixture, 1), 0);
}

fn history_size(fixture: &Fixture, pane: u64) -> u64 {
    json(fixture.msg(&["inspect", "--pane-id", &pane.to_string()]))["pane"]["history_size"]
        .as_u64()
        .unwrap()
}

fn text(output: Output) -> String {
    assert_success(&output);
    String::from_utf8(output.stdout).unwrap()
}

/// After a restart, an agent pane reopens the conversation it had — and does so only once someone
/// is looking.
#[test]
fn a_restored_agent_pane_resumes_its_conversation_when_a_client_attaches() {
    let fixture = Fixture::with_agents("resume");
    fixture.use_executing_shell();
    // An executable named `codex` that records how it was invoked, then holds the pane while
    // painting a marker the codex rules classify. It forks nothing: a fixture that spawned a child
    // every second would flap detection, and every identity change correctly clears agent state.
    let bin = fixture.runtime.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = fixture.runtime.path().join("argv.txt");
    fs::write(
        bin.join("codex"),
        format!(
            "#!/bin/sh\nprintf 'ARGV[%s]\\n' \"$@\" >> {log:?}\n\
             printf '\\033[H\\033[2JAllow command? esc to interrupt\\n'\n\
             while IFS= read -r line; do :; done\n"
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o700)).unwrap();

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    // The integration's own source name: the resume is gated on it, so reporting under any other
    // name must not produce one.
    assert_success(&fixture.msg_when_agents_ready(&[
        "report-agent",
        "--agent",
        "codex",
        "--state",
        "idle",
        "--source",
        "vvmux:codex",
        "--sequence",
        "1",
        "--agent-session-id",
        "CONV-42",
        "--pane-id",
        "1",
    ]));
    assert_success(&fixture.msg(&["agent-rename", "--pane-id", "1", "--name", "reviewer"]));
    fixture.kill();

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    // Armed, and deliberately not fired: nobody is attached, so nothing has been launched.
    assert_eq!(
        json(fixture.msg(&["inspect", "--pane-id", "1"]))["pane"]["pending_resume"]["agent"],
        "codex"
    );
    assert!(
        !log.exists(),
        "an agent was launched into a session nobody is watching"
    );

    fixture.attach_briefly(8);

    assert!(
        log.exists(),
        "the resume never ran: pane after attach = {}",
        json(fixture.msg(&["inspect", "--pane-id", "1"]))["pane"]
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap().trim(),
        "ARGV[resume]\nARGV[CONV-42]",
        "the agent was not resumed with its own session"
    );
    let pane = json(fixture.msg(&["inspect", "--pane-id", "1"]))["pane"].clone();
    assert!(pane["pending_resume"].is_null(), "the resume did not clear");
    assert_eq!(pane["agent"]["kind"], "codex");
    // The name comes back with the agent, so a script written before the restart still works.
    assert_eq!(pane["agent"]["alias"], "reviewer");
    assert_success(&fixture.msg(&["--alias", "reviewer", "agent-explain"]));
}

/// A session reference is only actionable when the integration that owns the agent reported it.
/// Anything else restores as the plain shell it already is.
#[test]
fn a_session_reported_by_a_foreign_source_is_not_resumed() {
    let fixture = Fixture::with_agents("foreign");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert_success(&fixture.msg_when_agents_ready(&[
        "report-agent",
        "--agent",
        "codex",
        "--state",
        "idle",
        // Not `vvmux:codex`: a source that does not own this agent kind.
        "--source",
        "some-other-tool",
        "--sequence",
        "1",
        "--agent-session-id",
        "CONV-42",
        "--pane-id",
        "1",
    ]));
    fixture.kill();

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    fixture.attach_briefly(4);
    assert!(
        json(fixture.msg(&["inspect", "--pane-id", "1"]))["pane"]["agent"].is_null(),
        "a foreign source's session reference produced a resume"
    );
}

/// Opting out has to mean no agent processes are started, whatever the snapshot says.
#[test]
fn resume_can_be_turned_off() {
    let fixture = Fixture::with_agents("noresume");
    fixture.append_config("[session]\nresume_agents = false");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert_success(&fixture.msg_when_agents_ready(&[
        "report-agent",
        "--agent",
        "codex",
        "--state",
        "idle",
        "--source",
        "vvmux:codex",
        "--sequence",
        "1",
        "--agent-session-id",
        "CONV-42",
        "--pane-id",
        "1",
    ]));
    fixture.kill();

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert!(
        json(fixture.msg(&["inspect", "--pane-id", "1"]))["pane"]["pending_resume"].is_null(),
        "a resume was armed with resume_agents off"
    );
}

/// An agent that reopens its own conversation repaints its own transcript. Replaying the screen
/// underneath it would show that transcript twice, so a pane with a resume armed gets no history —
/// while a pane beside it, with no agent, still does.
#[test]
fn a_resuming_pane_gets_no_history_replay_but_its_neighbour_does() {
    let fixture = Fixture::with_agents("resumehist");
    fixture.use_executing_shell();
    fixture.enable_pane_history();
    let bin = fixture.runtime.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        bin.join("codex"),
        "#!/bin/sh\nprintf '\\033[H\\033[2JAllow command? esc to interrupt\\n'\nwhile IFS= read -r line; do :; done\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o700)).unwrap();

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert_success(&fixture.msg(&["split", "vertical", "--pane-id", "1"]));
    fixture.wait_text(2, "READY pane=2");

    // Both panes scroll, so both have history worth replaying; only one has an agent.
    for pane in [1, 2] {
        assert_success(&fixture.msg(&[
            "submit",
            "i=1; while [ $i -le 60 ]; do printf \"SCROLL-$i\\n\"; i=$((i+1)); done",
            "--pane-id",
            &pane.to_string(),
        ]));
        fixture.wait_text(pane, "SCROLL-60");
    }
    assert_success(&fixture.msg_when_agents_ready(&[
        "report-agent",
        "--agent",
        "codex",
        "--state",
        "idle",
        "--source",
        "vvmux:codex",
        "--sequence",
        "1",
        "--agent-session-id",
        "CONV-9",
        "--pane-id",
        "1",
    ]));
    fixture.kill();

    assert_success(&fixture.start_without_layout());
    fixture.wait_text(2, "READY pane=2");
    // Pane 1 holds the agent, so its scrollback is left to the agent; pane 2 gets its own back.
    assert_eq!(
        history_size(&fixture, 1),
        0,
        "history was replayed under a pane that is about to resume its agent"
    );
    assert!(
        history_size(&fixture, 2) > 0,
        "the pane with no agent lost its history too"
    );
}

/// The whole point of a pane name: a pane ID does not survive a restart, and a name does.
#[test]
fn pane_names_survive_a_restart_that_reassigns_pane_ids() {
    let fixture = Fixture::new("pane-names");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1 tab=1");
    assert_success(&fixture.msg(&["split", "vertical", "--pane-id", "1"]));
    assert_success(&fixture.msg(&["split", "horizontal", "--pane-id", "1"]));
    for pane in 1..=3 {
        fixture.wait_text(pane, &format!("READY pane={pane}"));
    }

    // Name the pane that is neither first nor focused, so a restart that "restored the name" by
    // accident of ordering would be caught.
    assert_success(&fixture.msg(&["pane-rename", "--pane-id", "2", "--name", "editor"]));
    let named = json(fixture.msg(&["inspect", "--pane-name", "editor"]));
    assert_eq!(named["pane"]["pane_id"], 2);
    let before_path = named["pane"]["split_path"].clone();
    assert!(before_path.is_array(), "no split path: {named}");

    fixture.kill();
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1");
    assert_eq!(
        json(fixture.msg(&["snapshot"]))["restored_from_snapshot"],
        true
    );

    let after = json(fixture.msg(&["inspect", "--pane-name", "editor"]));
    assert!(
        after["pane"]["pane_id"].is_number(),
        "the name did not survive the restart: {after}"
    );
    assert_eq!(after["pane"]["pane_name"], "editor");
    // The name came back attached to the same *place* in the layout, which is what makes it a
    // usable target: restoring it onto whichever pane happened to be first would be worse than
    // losing it.
    assert_eq!(
        after["pane"]["split_path"], before_path,
        "the restored name landed on a different pane"
    );
}

#[test]
fn a_pane_name_is_unique_and_releasable() {
    let fixture = Fixture::new("pane-name-unique");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1 tab=1");
    assert_success(&fixture.msg(&["split", "vertical", "--pane-id", "1"]));
    fixture.wait_text(2, "READY pane=2");

    assert_success(&fixture.msg(&["pane-rename", "--pane-id", "1", "--name", "editor"]));
    let taken = fixture.msg(&["pane-rename", "--pane-id", "2", "--name", "editor"]);
    assert!(!taken.status.success());
    assert!(
        String::from_utf8_lossy(&taken.stderr).contains("pane_name_taken"),
        "{}",
        String::from_utf8_lossy(&taken.stderr)
    );

    // Renaming a pane to the name it already holds is not a collision with itself.
    assert_success(&fixture.msg(&["pane-rename", "--pane-id", "1", "--name", "editor"]));

    // Cleared, then free for another pane.
    assert_success(&fixture.msg(&["pane-rename", "--pane-id", "1", "--clear"]));
    assert_success(&fixture.msg(&["pane-rename", "--pane-id", "2", "--name", "editor"]));
    assert_eq!(
        json(fixture.msg(&["inspect", "--pane-name", "editor"]))["pane"]["pane_id"],
        2
    );
}

/// `layout` has to describe the tree, and `resolve-pane` has to walk the same graph focus does.
#[test]
fn layout_describes_the_tree_and_resolve_pane_agrees_with_focus() {
    let fixture = Fixture::new("topology");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1 tab=1");
    // 1 on the left; 2 above 3 on the right.
    assert_success(&fixture.msg(&["split", "vertical", "--pane-id", "1"]));
    assert_success(&fixture.msg(&["split", "horizontal", "--pane-id", "2"]));
    for pane in 1..=3 {
        fixture.wait_text(pane, &format!("READY pane={pane}"));
    }

    let layout = json(fixture.msg(&["layout"]));
    assert_eq!(layout["schema_version"], 1);
    let panes = layout["tabs"][0]["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 3);
    let path_of = |pane_id: u64| {
        panes
            .iter()
            .find(|pane| pane["pane_id"] == pane_id)
            .map(|pane| pane["split_path"].clone())
            .unwrap()
    };
    // Position in the tree, not on screen: a resize moves the rectangles and leaves these alone.
    assert_eq!(path_of(1), serde_json::json!([1]));
    assert_eq!(path_of(2), serde_json::json!([2, 1]));
    assert_eq!(path_of(3), serde_json::json!([2, 2]));

    // Every direction resolve-pane can walk must land where `action focus` would, or the two
    // models an agent holds — "where is that pane" and "go to that pane" — disagree.
    for direction in ["left", "right", "up", "down"] {
        let resolved = fixture.msg(&["resolve-pane", "--pane-id", "1", "--path", direction]);
        assert_success(&fixture.msg(&["focus", "--pane-id", "1"]));
        let focused = fixture.msg(&["action", "focus", direction, "--pane-id", "1"]);
        if !resolved.status.success() {
            // Nothing that way: focus must have stayed put rather than moved somewhere else.
            assert_success(&focused);
            assert_eq!(
                json(fixture.msg(&["layout"]))["tabs"][0]["focused_pane_id"],
                1,
                "{direction}: resolve-pane found nothing but focus moved"
            );
            continue;
        }
        let target = json(resolved)["target"]["pane_id"].clone();
        assert_success(&focused);
        assert_eq!(
            json(fixture.msg(&["layout"]))["tabs"][0]["focused_pane_id"],
            target,
            "{direction}: resolve-pane and action focus disagree"
        );
    }

    // A route is a sequence of steps, and a step that cannot be taken fails rather than stopping
    // early on a pane the caller did not ask for.
    let over = fixture.msg(&["resolve-pane", "--pane-id", "1", "--path", "left,left"]);
    assert!(!over.status.success());
    assert!(
        String::from_utf8_lossy(&over.stderr).contains("pane_not_found"),
        "{}",
        String::from_utf8_lossy(&over.stderr)
    );
}

/// Revealing a pane and typing into it are different requests.
#[test]
fn activate_pane_reveals_without_taking_focus() {
    let fixture = Fixture::new("activate");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1 tab=1");
    assert_success(&fixture.msg(&["split", "vertical", "--pane-id", "1"]));
    fixture.wait_text(2, "READY pane=2");
    assert_success(&fixture.msg(&["action", "new-tab"]));
    fixture.wait_text(3, "READY pane=3");

    // Zoom pane 1, which hides pane 2 behind it, and select the other tab.
    assert_success(&fixture.msg(&["focus", "--pane-id", "1"]));
    assert_success(&fixture.msg(&["action", "toggle-zoom", "--pane-id", "1"]));
    let first_tab = json(fixture.msg(&["list-tabs"]))["tabs"][0]["tab_id"].clone();
    let second_tab = json(fixture.msg(&["list-tabs"]))["tabs"][1]["tab_id"].clone();
    assert_success(&fixture.msg(&["select-tab", "--tab-id", &second_tab.to_string()]));

    let activated = json(fixture.msg(&["activate-pane", "--pane-id", "2"]));
    assert_eq!(activated["tab_selected"], true, "{activated}");
    assert_eq!(
        activated["unzoomed"], true,
        "a zoom hiding the target must be lifted: {activated}"
    );
    assert_eq!(activated["focus_changed"], false);

    let layout = json(fixture.msg(&["layout"]));
    assert_eq!(layout["active_tab_id"], first_tab);
    let tab = layout["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tab| tab["tab_id"] == first_tab)
        .unwrap();
    assert!(tab["zoomed_pane_id"].is_null(), "zoom was not lifted");
    // The pane that had focus still has it. Revealing is not selecting.
    assert_eq!(
        tab["focused_pane_id"], 1,
        "activate-pane moved focus: {layout}"
    );
}

#[test]
fn tabs_are_addressable_and_renameable_by_name() {
    let fixture = Fixture::new("tab-names");
    assert_success(&fixture.start_without_layout());
    fixture.wait_text(1, "READY pane=1 tab=1");

    let created = json(fixture.msg(&["new-tab", "--name", "logs"]));
    assert_eq!(created["tab_name"], "logs");
    let logs_tab = created["tab_id"].clone();
    // `action new-tab` answers nothing; this reports the identities it just made.
    assert!(created["pane_id"].is_number(), "{created}");

    // Matched case-insensitively: a name is typed by a person.
    let selected = json(fixture.msg(&["select-tab", "--tab-name", "LOGS"]));
    assert_eq!(selected["tab_id"], logs_tab);

    let renamed = json(fixture.msg(&["rename-tab", "--tab-name", "logs", "--name", "server"]));
    assert_eq!(renamed["previous_tab_name"], "logs");
    assert_eq!(renamed["tab_name"], "server");

    let reset = json(fixture.msg(&["reset-tab-title", "--tab-name", "server"]));
    assert!(reset["tab_name"].is_null(), "{reset}");

    // An ambiguous name is refused rather than resolved by position: acting on the wrong tab is
    // worse than saying the name does not identify one.
    assert_success(&fixture.msg(&["new-tab", "--name", "twin"]));
    assert_success(&fixture.msg(&["new-tab", "--name", "twin"]));
    let ambiguous = fixture.msg(&["select-tab", "--tab-name", "twin"]);
    assert!(!ambiguous.status.success());
    assert!(
        String::from_utf8_lossy(&ambiguous.stderr).contains("more than one tab"),
        "{}",
        String::from_utf8_lossy(&ambiguous.stderr)
    );

    let closed = json(fixture.msg(&["close-tab", "--tab-id", &logs_tab.to_string()]));
    assert_eq!(closed["accepted"], true);
    assert!(!closed["closed_pane_ids"].as_array().unwrap().is_empty());
    let remaining = json(fixture.msg(&["list-tabs"]));
    assert!(
        !remaining["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tab| tab["tab_id"] == logs_tab),
        "the tab was not closed: {remaining}"
    );
}
