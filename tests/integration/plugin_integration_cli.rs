//! End-to-end coverage of manifest-declared integrations through the real binary.
//!
//! The engine has unit tests; what this file proves is the part only a process can show — that an
//! absent agent config directory is a skip rather than a failure, that the environment override
//! the agent itself honors is the one the install writes into, that `integrate` repairs, that
//! `uninstall` is marker-safe, and that the shorthand source form resolves to the first-party
//! organization and satisfies the reserved-ID policy.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const CONFIG_ENV: &str = "VVMUX_TEST_AGENT_HOME";

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_vvmux")
}

/// A vvmux invocation with its home, config, and runtime directories inside the test's tree.
fn vvmux(home: &Path) -> Command {
    let mut command = Command::new(binary());
    command.env("HOME", home);
    command.env("XDG_CONFIG_HOME", home.join(".config"));
    command.env("XDG_STATE_HOME", home.join(".state"));
    command.env("XDG_RUNTIME_DIR", home.join(".run"));
    command.env_remove(CONFIG_ENV);
    command
}

fn assert_success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn manifest(id: &str) -> String {
    format!(
        r#"manifest_version = 2
[plugin]
id = "{id}"
name = "Demo agent support"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "test fixture"
platforms = ["linux", "macos", "windows"]
permissions = ["integration.write"]

[[agents]]
id = "demoagent"
name = "Demo Agent"
process = {{ executables = ["demoagent"] }}

[[integrations]]
id = "demo"
version = 1
config_dir = ".demo"
config_dir_env = "{CONFIG_ENV}"
notice = "remember to enable it by hand"

[[integrations.files]]
source = "integration/hook.sh"
dest = "hooks/vvmux-agent-state.sh"
executable = true

[[integrations.registrations]]
kind = "json-hook"
file = "settings.json"
event = "SessionStart"
matcher = "*"
command_file = "hooks/vvmux-agent-state.sh"
args = ["session"]

[[integrations.registrations]]
kind = "toml-flag"
file = "config.toml"
section = "features"
key = "hooks"
value = true
"#
    )
}

fn write_package(root: &Path, id: &str) {
    fs::create_dir_all(root.join("integration")).unwrap();
    fs::write(root.join("vvmux-plugin.toml"), manifest(id)).unwrap();
    fs::write(
        root.join("integration/hook.sh"),
        "#!/bin/sh\n# VVMUX_INTEGRATION_ID=demo\n# VVMUX_INTEGRATION_VERSION=1\nexit 0\n",
    )
    .unwrap();
}

fn git_repository(root: &Path) {
    for arguments in [
        vec!["init", "--quiet", "-b", "main"],
        vec!["config", "user.email", "test@example.invalid"],
        vec!["config", "user.name", "Test"],
        vec!["add", "."],
        vec!["commit", "--quiet", "-m", "package"],
    ] {
        let status = Command::new("git")
            .args(&arguments)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?} failed");
    }
}

fn listed(home: &Path) -> Vec<Value> {
    let output = vvmux(home)
        .args(["plugin", "list", "--json"])
        .output()
        .unwrap();
    serde_json::from_str::<Value>(&assert_success(&output))
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn integration_status(home: &Path, id: &str, extra_env: Option<&Path>) -> String {
    let mut command = vvmux(home);
    command.args(["plugin", "inspect", id, "--json"]);
    if let Some(config) = extra_env {
        command.env(CONFIG_ENV, config);
    }
    let value: Value = serde_json::from_str(&assert_success(&command.output().unwrap())).unwrap();
    value["integrations"][0]["status"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn an_integration_installs_repairs_and_uninstalls_through_the_real_binary() {
    let directory = tempfile::Builder::new()
        .prefix("vpi-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = directory.path().join("home");
    private_directory(&home);
    private_directory(&home.join(".config/vvmux"));
    private_directory(&home.join(".run/vvmux"));

    let package = directory.path().join("package");
    write_package(&package, "com.example.agent.demo");

    // The agent is not installed on this machine, so its config directory does not exist. That is
    // an ordinary outcome, not a failure: the package still installs.
    let output = vvmux(&home)
        .args(["plugin", "install"])
        .arg(&package)
        .arg("--yes")
        .output()
        .unwrap();
    let stdout = assert_success(&output);
    assert!(stdout.contains("integration: demo v1"), "{stdout}");
    assert!(
        stdout.contains("integration.write") || stdout.contains("IntegrationWrite"),
        "the preview must show the permission: {stdout}"
    );
    assert!(stdout.contains("skipped"), "{stdout}");
    assert!(stdout.contains(CONFIG_ENV), "{stdout}");
    assert!(!home.join(".demo").exists());
    assert_eq!(
        integration_status(&home, "com.example.agent.demo", None),
        "skipped"
    );

    // The override the agent itself reads is the one the install has to honor.
    let elsewhere = directory.path().join("agent-home");
    private_directory(&elsewhere);
    fs::write(
        elsewhere.join("settings.json"),
        r#"{"hooks":{"Notification":[{"hooks":[{"type":"command","command":"keep-me"}]}]}}"#,
    )
    .unwrap();
    let stdout = assert_success(
        &vvmux(&home)
            .env(CONFIG_ENV, &elsewhere)
            .args(["plugin", "integrate", "com.example.agent.demo"])
            .output()
            .unwrap(),
    );
    assert!(stdout.contains("installed"), "{stdout}");
    assert!(stdout.contains("remember to enable it by hand"), "{stdout}");
    let hook = elsewhere.join("hooks/vvmux-agent-state.sh");
    assert!(hook.is_file());
    assert_eq!(
        fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(!home.join(".demo").exists(), "the override was ignored");

    let settings = fs::read_to_string(elsewhere.join("settings.json")).unwrap();
    assert!(settings.contains("keep-me"));
    assert!(settings.contains("SessionStart"));
    assert!(settings.contains(&hook.display().to_string()));
    assert_eq!(
        fs::read_to_string(elsewhere.join("config.toml")).unwrap(),
        "[features]\nhooks = true\n"
    );
    assert_eq!(
        integration_status(&home, "com.example.agent.demo", Some(&elsewhere)),
        "current (v1)"
    );

    // Repair: a file deleted by hand is what `integrate` exists for.
    fs::remove_file(&hook).unwrap();
    assert_eq!(
        integration_status(&home, "com.example.agent.demo", Some(&elsewhere)),
        "not installed"
    );
    assert_success(
        &vvmux(&home)
            .env(CONFIG_ENV, &elsewhere)
            .args(["plugin", "integrate", "com.example.agent.demo"])
            .output()
            .unwrap(),
    );
    assert!(hook.is_file());

    // `list --json` carries the same status, so a caller need not run `inspect` per plugin.
    let entries = listed(&home);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["integrations"][0]["id"], "demo");

    let doctor = vvmux(&home)
        .env(CONFIG_ENV, &elsewhere)
        .args(["plugin", "doctor", "--json"])
        .output()
        .unwrap();
    let doctor: Value = serde_json::from_str(&assert_success(&doctor)).unwrap();
    assert_eq!(doctor["ok"], true);
    assert_eq!(
        doctor["plugins"][0]["integrations"][0]["status"],
        "current (v1)"
    );

    // Uninstall removes what it owns, and only that.
    let stdout = assert_success(
        &vvmux(&home)
            .env(CONFIG_ENV, &elsewhere)
            .args(["plugin", "uninstall", "com.example.agent.demo"])
            .output()
            .unwrap(),
    );
    assert!(stdout.contains("integration demo: removed"), "{stdout}");
    assert!(!hook.exists());
    assert!(!elsewhere.join("hooks").exists());
    let settings = fs::read_to_string(elsewhere.join("settings.json")).unwrap();
    assert!(settings.contains("keep-me"));
    assert!(!settings.contains("vvmux-agent-state"));
    // The agent's own global feature flag stays set.
    assert_eq!(
        fs::read_to_string(elsewhere.join("config.toml")).unwrap(),
        "[features]\nhooks = true\n"
    );
    assert!(listed(&home).is_empty());
}

/// A hand-edited managed file blocks the write, and never the package removal.
#[test]
fn a_foreign_file_is_never_replaced_and_never_blocks_an_uninstall() {
    let directory = tempfile::Builder::new()
        .prefix("vpf-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = directory.path().join("home");
    private_directory(&home);
    private_directory(&home.join(".config/vvmux"));
    private_directory(&home.join(".run/vvmux"));
    private_directory(&home.join(".demo/hooks"));
    fs::write(home.join(".demo/hooks/vvmux-agent-state.sh"), "# mine\n").unwrap();

    let package = directory.path().join("package");
    write_package(&package, "com.example.agent.demo");
    let output = vvmux(&home)
        .args(["plugin", "install"])
        .arg(&package)
        .arg("--yes")
        .output()
        .unwrap();
    // The package installs; only the adapter is refused, and it says which file stopped it.
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("refusing to overwrite"), "{stderr}");
    assert!(
        stderr.contains("plugin integrate com.example.agent.demo"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(home.join(".demo/hooks/vvmux-agent-state.sh")).unwrap(),
        "# mine\n"
    );
    assert_eq!(
        integration_status(&home, "com.example.agent.demo", None),
        "foreign file"
    );

    let output = vvmux(&home)
        .args(["plugin", "uninstall", "com.example.agent.demo"])
        .output()
        .unwrap();
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("left in place"),
        "the refusal must be reported"
    );
    assert_eq!(
        fs::read_to_string(home.join(".demo/hooks/vvmux-agent-state.sh")).unwrap(),
        "# mine\n"
    );
    assert!(listed(&home).is_empty());
}

/// The bare-name form resolves to the first-party organization, which is also the only source a
/// reserved ID may be installed from.
///
/// Offline: git's `insteadOf` rewrites the resolved URL to a local repository, so this asserts
/// what vvmux resolved rather than what GitHub would have served.
#[test]
fn a_shorthand_name_resolves_to_the_first_party_organization_and_unlocks_its_reserved_ids() {
    let directory = tempfile::Builder::new()
        .prefix("vps-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = directory.path().join("home");
    private_directory(&home);
    private_directory(&home.join(".config/vvmux"));
    private_directory(&home.join(".run/vvmux"));

    let repositories = directory.path().join("repositories");
    let repository = repositories.join("demo");
    write_package(&repository, "dev.vivido.agent.demo");
    git_repository(&repository);

    let rewritten = |home: &Path| {
        let mut command = vvmux(home);
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env(
                "GIT_CONFIG_KEY_0",
                format!("url.file://{}/.insteadOf", repositories.display()),
            )
            .env("GIT_CONFIG_VALUE_0", "https://github.com/vivido-dev/");
        command
    };

    assert_success(
        &rewritten(&home)
            .args(["plugin", "install", "demo", "--yes"])
            .output()
            .unwrap(),
    );
    let entries = listed(&home);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "dev.vivido.agent.demo");
    assert_eq!(entries[0]["source"], "https://github.com/vivido-dev/demo");

    // The same package from anywhere else keeps the ID it is not entitled to, and is refused.
    let copy = directory.path().join("copy");
    write_package(&copy, "dev.vivido.agent.demo");
    let refused = vvmux(&home)
        .args(["plugin", "install"])
        .arg(&copy)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(stderr.contains("reserved"), "{stderr}");
    assert!(stderr.contains("vivido-dev"), "{stderr}");

    // Linking a working copy is how the first-party packages are developed, and stays allowed.
    assert_success(
        &vvmux(&home)
            .args(["plugin", "link"])
            .arg(&copy)
            .arg("--yes")
            .output()
            .unwrap(),
    );

    // The policy holds on the dependency edge too: a bundle cannot smuggle in a reserved ID by
    // declaring it as a dependency of some other repository.
    let bundle = directory.path().join("bundle");
    fs::create_dir_all(&bundle).unwrap();
    fs::write(
        bundle.join("vvmux-plugin.toml"),
        r#"manifest_version = 2
[plugin]
id = "com.example.bundle"
name = "Bundle"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "test fixture"
platforms = ["linux", "macos", "windows"]
permissions = []
[[dependencies]]
alias = "demo"
id = "dev.vivido.agent.demo"
version = "*"
source = "https://github.com/someguy/demo"
"#,
    )
    .unwrap();
    let refused = vvmux(&home)
        .args(["plugin", "install"])
        .arg(&bundle)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(stderr.contains("reserved"), "{stderr}");

    // Two segments name somebody else's repository; more than two name nothing vvmux can clone.
    let refused = vvmux(&home)
        .args(["plugin", "install", "owner/repo/extra", "--yes"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("too many segments"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

/// A package that needs a newer vvmux says so, rather than failing as an unknown manifest field.
#[test]
fn a_package_requiring_a_newer_vvmux_is_refused_by_version() {
    let directory = tempfile::Builder::new()
        .prefix("vpv-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = directory.path().join("home");
    private_directory(&home);
    private_directory(&home.join(".config/vvmux"));
    private_directory(&home.join(".run/vvmux"));

    let package: PathBuf = directory.path().join("package");
    write_package(&package, "com.example.agent.demo");
    let text = fs::read_to_string(package.join("vvmux-plugin.toml"))
        .unwrap()
        .replace(
            r#"min_vvmux_version = "0.4.0""#,
            r#"min_vvmux_version = "99.1.0""#,
        );
    fs::write(package.join("vvmux-plugin.toml"), text).unwrap();

    let refused = vvmux(&home)
        .args(["plugin", "install"])
        .arg(&package)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("99.1.0"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}
