#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
fn reference_plugins_cover_install_discovery_invoke_pane_media_and_disable() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::Builder::new()
        .prefix("vpf-")
        .tempdir_in("/tmp")
        .unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);
    let config = write_config(directory.path());
    write_vivi_helper(directory.path());
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins");

    for package in [
        "rust-component",
        "python-dashboard",
        "typescript-agent",
        "vivid-chart",
    ] {
        assert_success(
            &command(binary, &runtime, &config_home)
                .args([
                    "plugin",
                    "link",
                    examples.join(package).to_str().unwrap(),
                    "--yes",
                ])
                .output()
                .unwrap(),
        );
    }

    let name = format!("plugin-reference-{}", std::process::id());
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

    let catalog = json(
        command(binary, &runtime, &config_home)
            .args(["plugin", "catalog", "--target", &name, "--json"])
            .output()
            .unwrap(),
    );
    let references = catalog["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|action| action["reference"].as_str())
        .collect::<Vec<_>>();
    assert!(references.contains(&"dev.vivido.reference.hello/greet"));
    assert!(references.contains(&"dev.vivido.reference.agent/summarize"));
    assert!(references.contains(&"dev.vivido.reference.chart/statistics"));

    let statistics = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "dev.vivido.reference.chart/statistics",
        r#"{"values":[1,4,2]}"#,
    );
    assert_eq!(
        statistics,
        serde_json::json!({"count": 3, "minimum": 1.0, "maximum": 4.0})
    );

    let opened = json(
        command(binary, &runtime, &config_home)
            .args([
                "plugin",
                "pane",
                "open",
                "dev.vivido.reference.chart/chart",
                "--target",
                &name,
            ])
            .output()
            .unwrap(),
    );
    let pane_id = opened["pane_id"].as_u64().unwrap();
    assert_eq!(opened["vivid"], true);
    assert_success(&msg(
        binary,
        &runtime,
        &config_home,
        &name,
        &[
            "wait",
            "text",
            "Vivid chart submitted off the PTY",
            "--pane-id",
            &pane_id.to_string(),
            "--timeout",
            "5s",
        ],
    ));
    let screen = msg(
        binary,
        &runtime,
        &config_home,
        &name,
        &["get-text", "--pane-id", &pane_id.to_string()],
    );
    assert_success(&screen);
    let screen = String::from_utf8_lossy(&screen.stdout);
    assert!(!screen.contains("VIVID_ROOT_SECRET"));
    assert!(!screen.contains("89504e470d0a1a0a"));
    let media = json(msg(
        binary,
        &runtime,
        &config_home,
        &name,
        &["inspect-media", "--pane-id", &pane_id.to_string()],
    ));
    assert!(media.is_object(), "inspect-media returned {media}");

    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "disable", "dev.vivido.reference.chart"])
            .output()
            .unwrap(),
    );
    let inspect = msg(
        binary,
        &runtime,
        &config_home,
        &name,
        &["inspect", "--pane-id", &pane_id.to_string()],
    );
    assert!(!inspect.status.success());
    assert!(String::from_utf8_lossy(&inspect.stderr).contains("pane_not_found"));
}

fn invoke(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    reference: &str,
    input: &str,
) -> Value {
    json(
        command(binary, runtime, config_home)
            .args([
                "plugin", "invoke", reference, "--target", session, "--input", input,
            ])
            .output()
            .unwrap(),
    )
}

fn write_config(root: &Path) -> PathBuf {
    let shell = root.join("shell.sh");
    fs::write(
        &shell,
        b"#!/bin/sh\nprintf 'READY\\r\\n'\nwhile :; do sleep 60; done\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = {}\nrender_interval_ms = 1\n[plugins]\nenabled = true\n",
            serde_json::to_string(shell.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    config
}

fn write_vivi_helper(root: &Path) {
    let helper = root.join("vivi");
    fs::write(
        &helper,
        "#!/bin/sh\ntest \"$VVMUX_VIVI_PROTOCOL_VERSION\" = 1.5\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
}

fn msg(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    arguments: &[&str],
) -> Output {
    command(binary, runtime, config_home)
        .args(["msg", "--target", session])
        .args(arguments)
        .output()
        .unwrap()
}

fn command(binary: &str, runtime: &Path, config_home: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", config_home)
        .env("VVMUX_VIVI_BIN", runtime.parent().unwrap().join("vivi"))
        .env_remove("VIVID_ENDPOINT")
        .env_remove("VIVID_TOKEN")
        .env_remove("VIVID_ROOT_SECRET");
    command
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn json(output: Output) -> Value {
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
