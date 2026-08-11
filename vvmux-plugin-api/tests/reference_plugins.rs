use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use vvmux_plugin_api::{LoadedManifest, Manifest, validate_schema_instance};

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins")
}

#[test]
fn ready_to_run_reference_packages_have_strict_loadable_manifests() {
    for package in [
        "python-dashboard",
        "typescript-agent",
        "vivid-chart",
        "verification-bundle",
    ] {
        let loaded = LoadedManifest::load(examples().join(package))
            .unwrap_or_else(|error| panic!("{package}: {error}"));
        assert!(
            loaded.warnings.is_empty(),
            "{package}: {:?}",
            loaded.warnings
        );
    }
}

#[test]
fn component_reference_manifest_and_schemas_validate_before_build() {
    let root = examples().join("rust-component");
    let manifest: Manifest = toml::from_str(
        &fs::read_to_string(root.join("vvmux-plugin.toml")).expect("component manifest"),
    )
    .expect("component manifest TOML");
    manifest.validate().expect("component manifest contract");
    let input: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("schemas/greet-input.json")).unwrap()).unwrap();
    let output: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("schemas/greet-output.json")).unwrap()).unwrap();
    validate_schema_instance(&input, &serde_json::json!({"name": "vvmux"})).unwrap();
    validate_schema_instance(
        &output,
        &serde_json::json!({"message": "Hello, vvmux!", "correlation_id": null}),
    )
    .unwrap();
}

#[test]
fn native_reference_actions_emit_schema_shaped_json() {
    let dashboard = run(
        &examples().join("python-dashboard"),
        Path::new("python3"),
        &["dashboard.py", "action"],
        r#"{"summary":"passed","chart":{"count":1,"minimum":2,"maximum":2}}"#,
    );
    assert_eq!(dashboard["title"], "Verification dashboard");

    let chart = run(
        &examples().join("vivid-chart"),
        Path::new("python3"),
        &["chart.py", "action"],
        r#"{"values":[2,4,8]}"#,
    );
    assert_eq!(
        chart,
        serde_json::json!({"count": 3, "minimum": 2.0, "maximum": 8.0})
    );

    let node = working_program("node");
    let agent = run(
        &examples().join("typescript-agent"),
        &node,
        &["agent-utility.mjs", "summarize"],
        r#"{"result":{"success":false,"status":1,"stdout":"","stderr":"failure","duration_ms":7}}"#,
    );
    assert_eq!(agent["durations"], serde_json::json!([7]));
}

fn working_program(name: &str) -> PathBuf {
    let executable = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let paths = std::env::var_os("PATH").expect("PATH must be set for reference action tests");
    std::env::split_paths(&paths)
        .map(|directory| directory.join(&executable))
        .find(|candidate| {
            candidate.is_file()
                && Command::new(candidate)
                    .arg("--version")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
        })
        .unwrap_or_else(|| panic!("could not find a working {name} executable on PATH"))
}

fn run(root: &Path, program: &Path, arguments: &[&str], input: &str) -> serde_json::Value {
    use std::io::Write;

    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("could not start {}: {error}", program.display()));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{} failed: {}",
        program.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
