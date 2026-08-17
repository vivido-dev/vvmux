use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use clap::{Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

const OPENCODE_PLUGIN: &str = include_str!("integration/opencode.js");
#[cfg(not(windows))]
const CLAUDE_SH: &str = include_str!("integration/claude.sh");
#[cfg(windows)]
const CLAUDE_PS1: &str = include_str!("integration/claude.ps1");
const CODEX_SH: &str = include_str!("integration/codex.sh");
const HERMES_PLUGIN: &str = include_str!("integration/hermes_plugin.py");
const HERMES_MANIFEST: &str = include_str!("integration/hermes_plugin.yaml");
const HOOK_NAME: &str = "vvmux-agent-state";
const HERMES_ENABLE_STANZA: &str = "plugins:\n  enabled:\n    - vvmux-agent-state";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum IntegrationTarget {
    Claude,
    Codex,
    Opencode,
    Hermes,
}

impl IntegrationTarget {
    const ALL: [Self; 4] = [Self::Claude, Self::Codex, Self::Opencode, Self::Hermes];

    fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Hermes => "hermes",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum IntegrationCommand {
    /// Install a managed agent lifecycle adapter.
    Install { target: IntegrationTarget },
    /// Show every managed adapter and whether it is current.
    Status,
    /// Remove a managed adapter without touching unrelated hooks or plugins.
    Uninstall { target: IntegrationTarget },
}

#[derive(Clone, Copy)]
struct IntegrationDescriptor {
    target: IntegrationTarget,
    marker_id: &'static str,
    version: u32,
}

impl IntegrationDescriptor {
    fn for_target(target: IntegrationTarget) -> Self {
        Self {
            target,
            marker_id: target.id(),
            version: if target == IntegrationTarget::Opencode {
                2
            } else {
                1
            },
        }
    }

    fn marker(self) -> String {
        format!("VVMUX_INTEGRATION_ID={}", self.marker_id)
    }

    fn version_marker(self) -> String {
        format!("VVMUX_INTEGRATION_VERSION={}", self.version)
    }

    fn config_dir(self, home: &Path) -> PathBuf {
        match self.target {
            IntegrationTarget::Claude => home.join(".claude"),
            IntegrationTarget::Codex => home.join(".codex"),
            IntegrationTarget::Opencode => home.join(".config/opencode"),
            IntegrationTarget::Hermes => home.join(".hermes"),
        }
    }

    fn runtime_config_dir(self, home: &Path) -> PathBuf {
        let override_name = match self.target {
            IntegrationTarget::Claude => Some("CLAUDE_CONFIG_DIR"),
            IntegrationTarget::Codex => Some("CODEX_HOME"),
            IntegrationTarget::Hermes => Some("HERMES_HOME"),
            IntegrationTarget::Opencode => None,
        };
        override_name
            .and_then(std::env::var_os)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config_dir(home))
    }

    fn managed_files(self, config: &Path) -> Vec<ManagedFile> {
        match self.target {
            IntegrationTarget::Claude => {
                #[cfg(windows)]
                let file = ManagedFile::new(
                    config.join("hooks/vvmux-agent-state.ps1"),
                    CLAUDE_PS1,
                    false,
                );
                #[cfg(not(windows))]
                let file =
                    ManagedFile::new(config.join("hooks/vvmux-agent-state.sh"), CLAUDE_SH, true);
                vec![file]
            }
            IntegrationTarget::Codex => vec![ManagedFile::new(
                config.join("vvmux-agent-state.sh"),
                CODEX_SH,
                true,
            )],
            IntegrationTarget::Opencode => vec![ManagedFile::new(
                config.join("plugins/vvmux-agent-state.js"),
                OPENCODE_PLUGIN,
                false,
            )],
            IntegrationTarget::Hermes => vec![
                ManagedFile::new(
                    config.join("plugins/vvmux-agent-state/plugin.yaml"),
                    HERMES_MANIFEST,
                    false,
                ),
                ManagedFile::new(
                    config.join("plugins/vvmux-agent-state/__init__.py"),
                    HERMES_PLUGIN,
                    false,
                ),
            ],
        }
    }

    fn install(self, config: &Path) -> io::Result<Vec<PathBuf>> {
        require_config_dir(config, self.target.id())?;
        let files = self.managed_files(config);
        preflight_managed(&files, self, io::ErrorKind::AlreadyExists, "overwrite")?;
        let config_write = match self.target {
            IntegrationTarget::Claude => {
                let path = config.join("settings.json");
                Some((
                    path.clone(),
                    install_json_hook(&path, &files[0].path, true)?,
                ))
            }
            IntegrationTarget::Codex => {
                let path = config.join("hooks.json");
                Some((
                    path.clone(),
                    install_json_hook(&path, &files[0].path, false)?,
                ))
            }
            IntegrationTarget::Opencode | IntegrationTarget::Hermes => None,
        };
        for file in &files {
            write_atomic(&file.path, file.contents.as_bytes(), file.executable)?;
        }
        if let Some((path, contents)) = config_write {
            write_atomic(&path, contents.as_bytes(), false)?;
        }
        if self.target == IntegrationTarget::Codex {
            let path = config.join("config.toml");
            let existing = read_optional(&path)?;
            let updated = enable_codex_hooks(&existing);
            if updated != existing {
                write_atomic(&path, updated.as_bytes(), false)?;
            }
        }
        Ok(files.into_iter().map(|file| file.path).collect())
    }

    fn status(self, config: &Path) -> io::Result<String> {
        let files = self.managed_files(config);
        let existing = files
            .iter()
            .filter(|file| file.path.exists())
            .collect::<Vec<_>>();
        if existing.is_empty() {
            return Ok("not installed".into());
        }
        for file in &existing {
            if !managed_header(&file.path)?.contains(&self.marker()) {
                return Ok("foreign file".into());
            }
        }
        if existing.len() != files.len()
            || existing.iter().any(|file| {
                !managed_header(&file.path)
                    .is_ok_and(|header| header.contains(&self.version_marker()))
            })
        {
            return Ok("outdated".into());
        }
        if matches!(
            self.target,
            IntegrationTarget::Claude | IntegrationTarget::Codex
        ) {
            let path = config.join(if self.target == IntegrationTarget::Claude {
                "settings.json"
            } else {
                "hooks.json"
            });
            if !json_hook_present(&path)? {
                return Ok("outdated".into());
            }
        }
        if self.target == IntegrationTarget::Hermes {
            return Ok(format!(
                "current (v{}; manual enable required: {})",
                self.version,
                HERMES_ENABLE_STANZA.replace('\n', " / ")
            ));
        }
        Ok(format!("current (v{})", self.version))
    }

    fn uninstall(self, config: &Path) -> io::Result<Vec<PathBuf>> {
        let files = self.managed_files(config);
        preflight_managed(&files, self, io::ErrorKind::PermissionDenied, "remove")?;
        let config_write = match self.target {
            IntegrationTarget::Claude => {
                let path = config.join("settings.json");
                uninstall_json_hook(&path)?.map(|contents| (path, contents))
            }
            IntegrationTarget::Codex => {
                let path = config.join("hooks.json");
                uninstall_json_hook(&path)?.map(|contents| (path, contents))
            }
            IntegrationTarget::Opencode | IntegrationTarget::Hermes => None,
        };
        if let Some((path, contents)) = config_write {
            write_atomic(&path, contents.as_bytes(), false)?;
        }
        let mut removed = Vec::new();
        for file in files {
            match fs::remove_file(&file.path) {
                Ok(()) => removed.push(file.path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if self.target == IntegrationTarget::Hermes {
            let _ = fs::remove_dir(config.join("plugins/vvmux-agent-state"));
        }
        Ok(removed)
    }
}

struct ManagedFile {
    path: PathBuf,
    contents: &'static str,
    executable: bool,
}

impl ManagedFile {
    fn new(path: PathBuf, contents: &'static str, executable: bool) -> Self {
        Self {
            path,
            contents,
            executable,
        }
    }
}

pub fn run(command: IntegrationCommand) -> io::Result<()> {
    let home = home_dir()?;
    match command {
        IntegrationCommand::Install { target } => {
            let descriptor = IntegrationDescriptor::for_target(target);
            let config = descriptor.runtime_config_dir(&home);
            let paths = descriptor.install(&config)?;
            crate::plugin::enable_builtin(&format!("dev.vivido.agent.{}", target.id()))?;
            println!("installed {} integration:", target.id());
            for path in paths {
                println!("  {}", path.display());
            }
            if target == IntegrationTarget::Hermes {
                println!(
                    "manual enable required in {}:",
                    config.join("config.yaml").display()
                );
                println!("{HERMES_ENABLE_STANZA}");
            }
        }
        IntegrationCommand::Status => {
            for target in IntegrationTarget::ALL {
                let descriptor = IntegrationDescriptor::for_target(target);
                let config = descriptor.runtime_config_dir(&home);
                println!("{}: {}", target.id(), descriptor.status(&config)?);
            }
        }
        IntegrationCommand::Uninstall { target } => {
            let descriptor = IntegrationDescriptor::for_target(target);
            let config = descriptor.runtime_config_dir(&home);
            let removed = descriptor.uninstall(&config)?;
            if removed.is_empty() {
                println!("{} integration is not installed", target.id());
            } else {
                println!("removed {} integration:", target.id());
                for path in removed {
                    println!("  {}", path.display());
                }
            }
        }
    }
    Ok(())
}

fn home_dir() -> io::Result<PathBuf> {
    #[cfg(windows)]
    let value = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let value = std::env::var_os("HOME");
    value.map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine the user home directory",
        )
    })
}

fn require_config_dir(path: &Path, target: &str) -> io::Result<()> {
    path.is_dir().then_some(()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{target} config directory not found at {}", path.display()),
        )
    })
}

fn preflight_managed(
    files: &[ManagedFile],
    descriptor: IntegrationDescriptor,
    kind: io::ErrorKind,
    action: &str,
) -> io::Result<()> {
    for file in files {
        let contents = match fs::read_to_string(&file.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !contents
            .lines()
            .take(8)
            .any(|line| line.contains(&descriptor.marker()))
        {
            return Err(io::Error::new(
                kind,
                format!(
                    "refusing to {action} unrelated file at {}",
                    file.path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn managed_header(path: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(path)?
        .lines()
        .take(8)
        .collect::<Vec<_>>()
        .join("\n"))
}

fn read_optional(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn write_atomic(path: &Path, contents: &[u8], executable: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".vvmux-integration.{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = file.write_all(contents).and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| crate::runtime::atomic_replace(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return result;
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn read_json_object(path: &Path) -> io::Result<Map<String, Value>> {
    let contents = read_optional(path)?;
    if contents.is_empty() {
        return Ok(Map::new());
    }
    serde_json::from_str::<Value>(&contents)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .as_object()
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "config root must be an object"))
}

fn hook_command(path: &Path) -> String {
    #[cfg(windows)]
    return format!("powershell -NoProfile -File \"{}\" session", path.display());
    #[cfg(not(windows))]
    return format!(
        "'{}' session",
        path.display().to_string().replace('\'', "'\\''")
    );
}

fn install_json_hook(path: &Path, hook: &Path, matcher: bool) -> io::Result<String> {
    let mut root = read_json_object(path)?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hooks must be an object"))?;
    remove_owned_hooks(hooks);
    let mut entry =
        json!({"hooks": [{"type": "command", "command": hook_command(hook), "timeout": 10}]});
    if matcher {
        entry["matcher"] = Value::String("*".into());
    }
    hooks
        .entry("SessionStart")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "SessionStart hooks must be an array",
            )
        })?
        .push(entry);
    serde_json::to_string_pretty(&Value::Object(root)).map_err(io::Error::other)
}

fn uninstall_json_hook(path: &Path) -> io::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut root = read_json_object(path)?;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    if !remove_owned_hooks(hooks) {
        return Ok(None);
    }
    serde_json::to_string_pretty(&Value::Object(root))
        .map(Some)
        .map_err(io::Error::other)
}

fn remove_owned_hooks(hooks: &mut Map<String, Value>) -> bool {
    let mut changed = false;
    for event in hooks.keys().cloned().collect::<Vec<_>>() {
        let Some(entries) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        for entry in entries.iter_mut() {
            let Some(commands) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            let before = commands.len();
            commands.retain(|command| {
                !command
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(HOOK_NAME))
            });
            changed |= before != commands.len();
        }
        let before = entries.len();
        entries.retain(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|commands| !commands.is_empty())
        });
        changed |= before != entries.len();
        if entries.is_empty() {
            hooks.remove(&event);
        }
    }
    changed
}

fn json_hook_present(path: &Path) -> io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let root = read_json_object(path)?;
    Ok(root
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|hooks| hooks.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|command| command.get("command").and_then(Value::as_str))
        .any(|command| command.contains(HOOK_NAME)))
}

fn enable_codex_hooks(contents: &str) -> String {
    let mut lines = contents.lines().map(str::to_owned).collect::<Vec<_>>();
    match lines.iter().position(|line| line.trim() == "[features]") {
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|line| line.trim_start().starts_with('['))
                .map_or(lines.len(), |offset| start + 1 + offset);
            if let Some(index) = (start + 1..end).find(|index| {
                lines[*index]
                    .split_once('=')
                    .is_some_and(|(key, _)| key.trim() == "hooks")
            }) {
                lines[index] = "hooks = true".into();
            } else {
                lines.insert(start + 1, "hooks = true".into());
            }
        }
        None => {
            if !lines.is_empty() && !lines.last().is_some_and(String::is_empty) {
                lines.push(String::new());
            }
            lines.extend(["[features]".into(), "hooks = true".into()]);
        }
    }
    let mut output = lines.join("\n");
    output.push('\n');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare(home: &Path, target: IntegrationTarget) -> IntegrationDescriptor {
        let descriptor = IntegrationDescriptor::for_target(target);
        fs::create_dir_all(descriptor.config_dir(home)).unwrap();
        descriptor
    }

    #[test]
    fn every_target_preserves_foreign_files_and_tracks_versions() {
        for target in IntegrationTarget::ALL {
            let home = tempfile::tempdir().unwrap();
            let descriptor = prepare(home.path(), target);
            let config = descriptor.config_dir(home.path());
            let first = descriptor.managed_files(&config).remove(0).path;
            fs::create_dir_all(first.parent().unwrap()).unwrap();
            fs::write(&first, "foreign").unwrap();
            assert_eq!(
                descriptor.install(&config).unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(
                descriptor.uninstall(&config).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            fs::write(
                &first,
                format!("# {}\n# VVMUX_INTEGRATION_VERSION=0\n", descriptor.marker()),
            )
            .unwrap();
            assert_eq!(descriptor.status(&config).unwrap(), "outdated");
            descriptor.install(&config).unwrap();
            assert!(
                descriptor
                    .status(&config)
                    .unwrap()
                    .starts_with("current (v")
            );
            assert!(!descriptor.uninstall(&config).unwrap().is_empty());
            assert!(!first.exists());
        }
    }

    #[test]
    fn json_installers_preserve_foreign_hooks_and_refuse_invalid_configs() {
        for target in [IntegrationTarget::Claude, IntegrationTarget::Codex] {
            let home = tempfile::tempdir().unwrap();
            let descriptor = prepare(home.path(), target);
            let config = descriptor.config_dir(home.path());
            let path = config.join(if target == IntegrationTarget::Claude {
                "settings.json"
            } else {
                "hooks.json"
            });
            fs::write(&path, r#"{"hooks":{"Notification":[{"hooks":[{"type":"command","command":"keep-me"}]}]}}"#).unwrap();
            descriptor.install(&config).unwrap();
            descriptor.uninstall(&config).unwrap();
            let retained = fs::read_to_string(&path).unwrap();
            assert!(retained.contains("keep-me"));
            assert!(!retained.contains(HOOK_NAME));
            fs::write(&path, "{not-json").unwrap();
            assert_eq!(
                descriptor.install(&config).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn codex_feature_edit_is_minimal_and_uninstall_leaves_it_enabled() {
        let home = tempfile::tempdir().unwrap();
        let descriptor = prepare(home.path(), IntegrationTarget::Codex);
        let config = descriptor.config_dir(home.path());
        fs::write(
            config.join("config.toml"),
            "[features]\nhooks = false\nother = true\n",
        )
        .unwrap();
        descriptor.install(&config).unwrap();
        let enabled = fs::read_to_string(config.join("config.toml")).unwrap();
        assert_eq!(enabled, "[features]\nhooks = true\nother = true\n");
        descriptor.uninstall(&config).unwrap();
        assert_eq!(
            fs::read_to_string(config.join("config.toml")).unwrap(),
            enabled
        );
    }
}
