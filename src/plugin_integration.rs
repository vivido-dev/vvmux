//! Manifest-driven agent lifecycle adapters.
//!
//! An agent reports what it is doing by running a hook that its own configuration directory
//! registers. vvmux ships no per-agent code for that: a provider plugin declares
//! `[[integrations]]` — where the agent keeps its configuration, which files to place there, and
//! how that agent's own config file registers them — and this engine performs exactly those
//! declared edits. Adding an agent is then a package, not a release.
//!
//! Every managed file carries `VVMUX_INTEGRATION_ID=<id>` and `VVMUX_INTEGRATION_VERSION=<n>` in
//! its first lines. Those two markers are the whole ownership model: a file without the id is
//! somebody else's and is never replaced or removed, and a file whose version differs is reported
//! as outdated. The markers are byte-identical to the ones the previous per-agent installer wrote,
//! so an adapter installed by an older vvmux stays owned rather than becoming foreign.
//!
//! This runs in the CLI process, never in the session actor: it writes into `$HOME` and prompts
//! through the ordinary install approval, neither of which belongs behind a session request.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};
use vvmux_plugin_api::{Integration, IntegrationRegistration, Manifest};

/// Lines of a managed file that may carry its ownership markers.
const MARKER_LINES: usize = 8;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    /// The agent's own configuration directory does not exist, so there is nothing to hook into.
    Skipped,
    NotInstalled,
    Current(u32),
    Outdated,
    /// A managed path holds a file this integration does not own.
    Foreign,
}

impl std::fmt::Display for Status {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Skipped => formatter.write_str("skipped"),
            Self::NotInstalled => formatter.write_str("not installed"),
            Self::Current(version) => write!(formatter, "current (v{version})"),
            Self::Outdated => formatter.write_str("outdated"),
            Self::Foreign => formatter.write_str("foreign file"),
        }
    }
}

/// One integration bound to a package on disk and a resolved configuration directory.
pub(crate) struct Adapter<'a> {
    integration: &'a Integration,
    package_root: PathBuf,
    config: PathBuf,
}

/// One hook entry, resolved and waiting to be merged into its JSON config file.
struct PendingHook {
    event: String,
    matcher: Option<String>,
    command: String,
}

struct ManagedFile {
    path: PathBuf,
    contents: Vec<u8>,
    executable: bool,
}

/// Every integration a package declares, resolved against this user's home directory.
pub(crate) fn adapters<'a>(
    manifest: &'a Manifest,
    package_root: &Path,
    home: &Path,
) -> Vec<Adapter<'a>> {
    manifest
        .integrations
        .iter()
        .map(|integration| Adapter::new(integration, package_root, home))
        .collect()
}

impl<'a> Adapter<'a> {
    fn new(integration: &'a Integration, package_root: &Path, home: &Path) -> Self {
        // The override is read from this process's environment because that is the same variable
        // the agent itself reads: an adapter has to land where the agent will actually look.
        let config = integration
            .config_dir_env
            .as_ref()
            .and_then(std::env::var_os)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(&integration.config_dir));
        Self {
            integration,
            package_root: package_root.to_path_buf(),
            config,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.integration.id
    }

    pub(crate) fn version(&self) -> u32 {
        self.integration.version
    }

    pub(crate) fn config_dir(&self) -> &Path {
        &self.config
    }

    pub(crate) fn config_dir_env(&self) -> Option<&str> {
        self.integration.config_dir_env.as_deref()
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.integration.notice.as_deref()
    }

    /// Where every declared file would land, regardless of platform.
    ///
    /// Cleanup and ownership use this rather than the platform-filtered set: a file left by an
    /// install on another platform in a shared home directory is still this integration's, and
    /// leaving it behind would make a later install refuse.
    pub(crate) fn destinations(&self) -> Vec<PathBuf> {
        self.integration
            .files
            .iter()
            .map(|file| self.config.join(&file.dest))
            .collect()
    }

    /// The declared registration targets, resolved, for inspection.
    pub(crate) fn registration_targets(&self) -> Vec<String> {
        self.integration
            .registrations
            .iter()
            .map(|registration| match registration {
                IntegrationRegistration::JsonHook { file, event, .. } => {
                    format!("{} ({event} hook)", self.config.join(file).display())
                }
                IntegrationRegistration::TomlFlag {
                    file, section, key, ..
                } => format!("{} ([{section}] {key})", self.config.join(file).display()),
            })
            .collect()
    }

    fn marker(&self) -> String {
        format!("VVMUX_INTEGRATION_ID={}", self.integration.id)
    }

    fn version_marker(&self) -> String {
        format!("VVMUX_INTEGRATION_VERSION={}", self.integration.version)
    }

    /// The declared files that belong on the platform this vvmux is running on.
    fn platform_files(&self) -> Vec<&vvmux_plugin_api::IntegrationFile> {
        let platform = crate::plugin::current_platform();
        self.integration
            .files
            .iter()
            .filter(|file| {
                file.platforms.is_empty() || file.platforms.iter().any(|name| name == platform)
            })
            .collect()
    }

    fn managed_files(&self) -> io::Result<Vec<ManagedFile>> {
        self.platform_files()
            .into_iter()
            .map(|file| {
                let contents = fs::read(self.package_root.join(&file.source))?;
                // The engine copies package bytes verbatim rather than stamping a header, so a
                // payload without the marker would install a file its own uninstall would refuse
                // to remove. Refusing here turns that into an install-time error with a name in it.
                if !header_of(&contents).contains(&self.marker()) {
                    return Err(crate::plugin::invalid(format!(
                        "integration `{}` file `{}` is missing the `{}` marker in its first {MARKER_LINES} lines",
                        self.integration.id,
                        file.source.display(),
                        self.marker()
                    )));
                }
                Ok(ManagedFile {
                    path: self.config.join(&file.dest),
                    contents,
                    executable: file.executable,
                })
            })
            .collect()
    }

    /// The registrations whose command file exists on this platform.
    fn platform_registrations(&self) -> Vec<&IntegrationRegistration> {
        let live = self
            .platform_files()
            .into_iter()
            .map(|file| file.dest.as_path())
            .collect::<Vec<_>>();
        self.integration
            .registrations
            .iter()
            .filter(|registration| match registration {
                IntegrationRegistration::JsonHook { command_file, .. } => {
                    live.contains(&command_file.as_path())
                }
                IntegrationRegistration::TomlFlag { .. } => true,
            })
            .collect()
    }

    pub(crate) fn install(&self) -> io::Result<Vec<PathBuf>> {
        if !self.config.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} config directory not found at {}",
                    self.integration.id,
                    self.config.display()
                ),
            ));
        }
        let files = self.managed_files()?;
        self.preflight(io::ErrorKind::AlreadyExists, "overwrite")?;

        // Every JSON edit is computed before anything is written, so an unparsable config file
        // fails the whole install rather than leaving half of one behind.
        let owned = self.destinations();
        let mut json_writes = BTreeMap::new();
        for (file, registrations) in self.json_registrations_by_file() {
            let path = self.config.join(&file);
            let mut root = read_json_object(&path)?;
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "hooks must be an object")
                })?;
            remove_owned_hooks(hooks, &owned);
            for hook in registrations {
                let mut entry =
                    json!({"hooks": [{"type": "command", "command": hook.command, "timeout": 10}]});
                if let Some(matcher) = hook.matcher {
                    entry["matcher"] = Value::String(matcher);
                }
                hooks
                    .entry(hook.event)
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "hook event entries must be an array",
                        )
                    })?
                    .push(entry);
            }
            json_writes.insert(
                path,
                serde_json::to_string_pretty(&Value::Object(root)).map_err(io::Error::other)?,
            );
        }

        for file in &files {
            write_atomic(&file.path, &file.contents, file.executable)?;
        }
        for (path, contents) in json_writes {
            write_atomic(&path, contents.as_bytes(), false)?;
        }
        for registration in self.platform_registrations() {
            if let IntegrationRegistration::TomlFlag {
                file,
                section,
                key,
                value,
            } = registration
            {
                let path = self.config.join(file);
                let existing = read_optional(&path)?;
                let updated = set_toml_flag(&existing, section, key, *value);
                if updated != existing {
                    write_atomic(&path, updated.as_bytes(), false)?;
                }
            }
        }
        Ok(files.into_iter().map(|file| file.path).collect())
    }

    pub(crate) fn uninstall(&self) -> io::Result<Vec<PathBuf>> {
        self.preflight(io::ErrorKind::PermissionDenied, "remove")?;
        let owned = self.destinations();
        for file in self
            .integration
            .registrations
            .iter()
            .filter_map(|registration| match registration {
                IntegrationRegistration::JsonHook { file, .. } => Some(file),
                // A feature flag stays set: it is the agent's own global switch, and other hooks
                // the user installed by hand may depend on it.
                IntegrationRegistration::TomlFlag { .. } => None,
            })
            .collect::<std::collections::BTreeSet<_>>()
        {
            let path = self.config.join(file);
            if let Some(contents) = remove_json_hooks(&path, &owned)? {
                write_atomic(&path, contents.as_bytes(), false)?;
            }
        }
        let mut removed = Vec::new();
        for path in owned {
            match fs::remove_file(&path) {
                Ok(()) => {
                    prune_empty_parents(&self.config, &path);
                    removed.push(path);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(removed)
    }

    pub(crate) fn status(&self) -> io::Result<Status> {
        if !self.config.is_dir() {
            return Ok(Status::Skipped);
        }
        let expected = self
            .platform_files()
            .into_iter()
            .map(|file| self.config.join(&file.dest))
            .collect::<Vec<_>>();
        let present = expected
            .iter()
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        if present.is_empty() {
            return Ok(Status::NotInstalled);
        }
        for path in &present {
            if !file_header(path)?.contains(&self.marker()) {
                return Ok(Status::Foreign);
            }
        }
        if present.len() != expected.len() {
            return Ok(Status::Outdated);
        }
        for path in &present {
            if !file_header(path)?.contains(&self.version_marker()) {
                return Ok(Status::Outdated);
            }
        }
        let owned = self.destinations();
        for (file, registrations) in self.json_registrations_by_file() {
            let path = self.config.join(&file);
            if registrations.len() != count_owned_hooks(&path, &owned)? {
                return Ok(Status::Outdated);
            }
        }
        for registration in self.platform_registrations() {
            if let IntegrationRegistration::TomlFlag {
                file,
                section,
                key,
                value,
            } = registration
                && !toml_flag_matches(
                    &read_optional(&self.config.join(file))?,
                    section,
                    key,
                    *value,
                )
            {
                return Ok(Status::Outdated);
            }
        }
        Ok(Status::Current(self.integration.version))
    }

    /// Refuse to touch a managed path holding a file this integration does not own.
    fn preflight(&self, kind: io::ErrorKind, action: &str) -> io::Result<()> {
        for path in self.destinations() {
            let contents = match fs::read(&path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !header_of(&contents).contains(&self.marker()) {
                return Err(io::Error::new(
                    kind,
                    format!("refusing to {action} unrelated file at {}", path.display()),
                ));
            }
        }
        Ok(())
    }

    /// JSON hook registrations grouped by the file they edit, so each file is rewritten once.
    ///
    /// Grouping matters for ownership: the owned entries are cleared once per file and the live
    /// ones re-added, which a per-registration rewrite would undo for everything but the last.
    fn json_registrations_by_file(&self) -> BTreeMap<PathBuf, Vec<PendingHook>> {
        let mut grouped: BTreeMap<PathBuf, Vec<PendingHook>> = BTreeMap::new();
        for registration in self.platform_registrations() {
            if let IntegrationRegistration::JsonHook {
                file,
                event,
                matcher,
                command_file,
                args,
            } = registration
            {
                grouped.entry(file.clone()).or_default().push(PendingHook {
                    event: event.clone(),
                    matcher: matcher.clone(),
                    command: hook_command(&self.config.join(command_file), args),
                });
            }
        }
        grouped
    }
}

/// The command line an agent's config file runs for this hook.
///
/// PowerShell scripts are not executable files, so a `.ps1` is launched through the interpreter;
/// everything else is run directly with its path quoted for a POSIX shell. Arguments are appended
/// unquoted because the manifest validator restricts them to single shell-safe words.
fn hook_command(path: &Path, args: &[String]) -> String {
    let mut command = if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ps1"))
    {
        format!("powershell -NoProfile -File \"{}\"", path.display())
    } else {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    };
    for argument in args {
        command.push(' ');
        command.push_str(argument);
    }
    command
}

fn header_of(contents: &[u8]) -> String {
    String::from_utf8_lossy(contents)
        .lines()
        .take(MARKER_LINES)
        .collect::<Vec<_>>()
        .join("\n")
}

fn file_header(path: &Path) -> io::Result<String> {
    Ok(header_of(&fs::read(path)?))
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
    let _ = executable;
    Ok(())
}

/// Remove directories the uninstall emptied, stopping at the agent's own config directory.
fn prune_empty_parents(config: &Path, path: &Path) {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == config
            || !directory.starts_with(config)
            || fs::remove_dir(directory).is_err()
        {
            break;
        }
        current = directory.parent();
    }
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

fn remove_json_hooks(path: &Path, owned: &[PathBuf]) -> io::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut root = read_json_object(path)?;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    if !remove_owned_hooks(hooks, owned) {
        return Ok(None);
    }
    serde_json::to_string_pretty(&Value::Object(root))
        .map(Some)
        .map_err(io::Error::other)
}

/// A hook belongs to this integration when its command names one of the files it manages.
///
/// Keying on the resolved path rather than a shared name means two providers can install adapters
/// into the same agent config file without either one removing the other's entries.
fn owns_command(command: &str, owned: &[PathBuf]) -> bool {
    owned
        .iter()
        .any(|path| command.contains(&path.display().to_string()))
}

fn remove_owned_hooks(hooks: &mut Map<String, Value>, owned: &[PathBuf]) -> bool {
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
                    .is_some_and(|command| owns_command(command, owned))
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

fn count_owned_hooks(path: &Path, owned: &[PathBuf]) -> io::Result<usize> {
    if !path.exists() {
        return Ok(0);
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
        .filter(|command| owns_command(command, owned))
        .count())
}

/// Set one boolean key inside one table, touching no other line.
///
/// A parse-and-reserialize would reformat and drop the comments of a file the user owns, so this
/// edits the smallest span that can carry the change.
fn set_toml_flag(contents: &str, section: &str, key: &str, value: bool) -> String {
    let assignment = format!("{key} = {value}");
    let header = format!("[{section}]");
    let mut lines = contents.lines().map(str::to_owned).collect::<Vec<_>>();
    match lines.iter().position(|line| line.trim() == header) {
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|line| line.trim_start().starts_with('['))
                .map_or(lines.len(), |offset| start + 1 + offset);
            if let Some(index) = (start + 1..end).find(|index| {
                lines[*index]
                    .split_once('=')
                    .is_some_and(|(name, _)| name.trim() == key)
            }) {
                lines[index] = assignment;
            } else {
                lines.insert(start + 1, assignment);
            }
        }
        None => {
            if !lines.is_empty() && !lines.last().is_some_and(String::is_empty) {
                lines.push(String::new());
            }
            lines.extend([header, assignment]);
        }
    }
    let mut output = lines.join("\n");
    output.push('\n');
    output
}

fn toml_flag_matches(contents: &str, section: &str, key: &str, value: bool) -> bool {
    toml::from_str::<toml::Value>(contents).is_ok_and(|document| {
        document
            .get(section)
            .and_then(|table| table.get(key))
            .and_then(toml::Value::as_bool)
            == Some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OTHER_PLATFORM: &str = if cfg!(windows) { "linux" } else { "windows" };

    /// A package on disk plus the home directory its adapters resolve against.
    struct Fixture {
        _directory: tempfile::TempDir,
        package: PathBuf,
        home: PathBuf,
        manifest: Manifest,
    }

    impl Fixture {
        /// `body` is appended to a minimal manifest; every `file` named is written with markers.
        fn new(body: &str, files: &[(&str, &str)]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let package = directory.path().join("package");
            let home = directory.path().join("home");
            fs::create_dir_all(package.join("integration")).unwrap();
            fs::create_dir_all(&home).unwrap();
            let source = format!(
                r#"manifest_version = 2
[plugin]
id = "com.example.demo"
name = "Demo"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "d"
platforms = ["linux", "macos", "windows"]
permissions = ["integration.write"]
{body}"#
            );
            fs::write(package.join("vvmux-plugin.toml"), &source).unwrap();
            for (name, contents) in files {
                fs::write(package.join("integration").join(name), contents).unwrap();
            }
            let manifest: Manifest = toml::from_str(&source).unwrap();
            manifest.validate().unwrap();
            Self {
                _directory: directory,
                package,
                home,
                manifest,
            }
        }

        fn adapter(&self) -> Adapter<'_> {
            adapters(&self.manifest, &self.package, &self.home)
                .pop()
                .unwrap()
        }

        fn make_config_dir(&self) -> PathBuf {
            let config = self.adapter().config_dir().to_path_buf();
            fs::create_dir_all(&config).unwrap();
            config
        }
    }

    fn marked(id: &str, version: u32, body: &str) -> String {
        format!("# VVMUX_INTEGRATION_ID={id}\n# VVMUX_INTEGRATION_VERSION={version}\n{body}")
    }

    const SIMPLE: &str = r#"
[[integrations]]
id = "demo"
version = 2
config_dir = ".demo"
[[integrations.files]]
source = "integration/hook.sh"
dest = "hooks/vvmux-agent-state.sh"
executable = true
"#;

    fn simple() -> Fixture {
        Fixture::new(SIMPLE, &[("hook.sh", &marked("demo", 2, "echo hello\n"))])
    }

    /// The whole ownership model in one test: an unmarked file is neither replaced nor removed,
    /// a marked file at the wrong version reads as outdated, and a clean cycle round-trips.
    #[test]
    fn ownership_markers_gate_every_write_and_track_the_installed_version() {
        let fixture = simple();
        let adapter = fixture.adapter();
        let config = fixture.make_config_dir();
        let managed = config.join("hooks/vvmux-agent-state.sh");

        assert_eq!(adapter.status().unwrap(), Status::NotInstalled);

        fs::create_dir_all(managed.parent().unwrap()).unwrap();
        fs::write(&managed, "# somebody else's hook\n").unwrap();
        assert_eq!(adapter.status().unwrap(), Status::Foreign);
        assert_eq!(
            adapter.install().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            adapter.uninstall().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::read_to_string(&managed).unwrap(),
            "# somebody else's hook\n"
        );

        fs::write(&managed, marked("demo", 1, "old\n")).unwrap();
        assert_eq!(adapter.status().unwrap(), Status::Outdated);

        assert_eq!(adapter.install().unwrap(), vec![managed.clone()]);
        assert_eq!(adapter.status().unwrap(), Status::Current(2));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&managed).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(adapter.uninstall().unwrap(), vec![managed.clone()]);
        assert!(!managed.exists());
        // The directory the install created is gone; the config directory itself is not.
        assert!(!config.join("hooks").exists());
        assert!(config.is_dir());
    }

    /// An absent config directory is the ordinary case for an agent the user does not have.
    #[test]
    fn an_absent_config_directory_is_a_skip_rather_than_a_failure() {
        let fixture = simple();
        let adapter = fixture.adapter();
        assert_eq!(adapter.status().unwrap(), Status::Skipped);
        assert_eq!(
            adapter.install().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        // Uninstalling one that was never installed removes nothing and reports nothing.
        assert!(adapter.uninstall().unwrap().is_empty());
    }

    /// A payload with no marker would install a file the uninstall could not remove.
    #[test]
    fn a_package_file_without_the_marker_is_refused_at_install() {
        let fixture = Fixture::new(SIMPLE, &[("hook.sh", "echo hello\n")]);
        fixture.make_config_dir();
        let error = fixture.adapter().install().unwrap_err().to_string();
        assert!(error.contains("VVMUX_INTEGRATION_ID=demo"), "{error}");
    }

    const JSON_HOOK: &str = r#"
[[integrations]]
id = "demo"
version = 1
config_dir = ".demo"
[[integrations.files]]
source = "integration/hook.sh"
dest = "vvmux-agent-state.sh"
[[integrations.registrations]]
kind = "json-hook"
file = "settings.json"
event = "SessionStart"
matcher = "*"
command_file = "vvmux-agent-state.sh"
args = ["session"]
"#;

    #[test]
    fn json_registration_preserves_foreign_hooks_and_refuses_an_unparsable_config() {
        let fixture = Fixture::new(JSON_HOOK, &[("hook.sh", &marked("demo", 1, "echo\n"))]);
        let adapter = fixture.adapter();
        let config = fixture.make_config_dir();
        let settings = config.join("settings.json");
        fs::write(
            &settings,
            r#"{"hooks":{"Notification":[{"hooks":[{"type":"command","command":"keep-me"}]}]}}"#,
        )
        .unwrap();

        adapter.install().unwrap();
        let installed = fs::read_to_string(&settings).unwrap();
        assert!(installed.contains("keep-me"));
        assert!(installed.contains("SessionStart"));
        assert!(installed.contains("vvmux-agent-state.sh' session"));
        assert_eq!(adapter.status().unwrap(), Status::Current(1));

        // A registration removed by hand is what `plugin integrate` exists to repair.
        let mut root: Value = serde_json::from_str(&installed).unwrap();
        root["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("SessionStart");
        fs::write(&settings, serde_json::to_string_pretty(&root).unwrap()).unwrap();
        assert_eq!(adapter.status().unwrap(), Status::Outdated);
        adapter.install().unwrap();
        assert_eq!(adapter.status().unwrap(), Status::Current(1));

        adapter.uninstall().unwrap();
        let retained = fs::read_to_string(&settings).unwrap();
        assert!(retained.contains("keep-me"));
        assert!(!retained.contains("vvmux-agent-state"));

        fs::write(&settings, "{not-json").unwrap();
        assert_eq!(
            adapter.install().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        // Refused whole: the managed file was not written either.
        assert!(!config.join("vvmux-agent-state.sh").exists());
    }

    /// Two providers may adapt the same agent; neither may remove the other's hook.
    #[test]
    fn hook_ownership_is_keyed_on_the_managed_path_not_a_shared_name() {
        let fixture = Fixture::new(JSON_HOOK, &[("hook.sh", &marked("demo", 1, "echo\n"))]);
        let adapter = fixture.adapter();
        let config = fixture.make_config_dir();
        let settings = config.join("settings.json");
        adapter.install().unwrap();
        let mut root: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        root["hooks"]["SessionStart"].as_array_mut().unwrap().push(
            json!({"hooks":[{"type":"command","command":"'/elsewhere/vvmux-agent-state.sh' session"}]}),
        );
        fs::write(&settings, serde_json::to_string_pretty(&root).unwrap()).unwrap();

        adapter.uninstall().unwrap();
        let retained = fs::read_to_string(&settings).unwrap();
        assert!(retained.contains("/elsewhere/vvmux-agent-state.sh"));
        assert!(!retained.contains(&config.join("vvmux-agent-state.sh").display().to_string()));
    }

    const TOML_FLAG: &str = r#"
[[integrations]]
id = "demo"
version = 1
config_dir = ".demo"
[[integrations.files]]
source = "integration/hook.sh"
dest = "vvmux-agent-state.sh"
[[integrations.registrations]]
kind = "toml-flag"
file = "config.toml"
section = "features"
key = "hooks"
value = true
"#;

    #[test]
    fn a_toml_flag_is_a_one_line_edit_and_survives_uninstall() {
        let fixture = Fixture::new(TOML_FLAG, &[("hook.sh", &marked("demo", 1, "echo\n"))]);
        let adapter = fixture.adapter();
        let config = fixture.make_config_dir();
        fs::write(
            config.join("config.toml"),
            "[features]\nhooks = false\nother = true\n",
        )
        .unwrap();

        adapter.install().unwrap();
        let enabled = fs::read_to_string(config.join("config.toml")).unwrap();
        assert_eq!(enabled, "[features]\nhooks = true\nother = true\n");
        assert_eq!(adapter.status().unwrap(), Status::Current(1));

        adapter.uninstall().unwrap();
        assert_eq!(
            fs::read_to_string(config.join("config.toml")).unwrap(),
            enabled
        );
    }

    #[test]
    fn a_missing_toml_table_is_appended_and_a_cleared_flag_reads_as_outdated() {
        assert_eq!(
            set_toml_flag("", "features", "hooks", true),
            "[features]\nhooks = true\n"
        );
        assert_eq!(
            set_toml_flag("model = \"x\"\n", "features", "hooks", true),
            "model = \"x\"\n\n[features]\nhooks = true\n"
        );
        assert_eq!(
            set_toml_flag(
                "[features]\nother = 1\n[other]\nhooks = false\n",
                "features",
                "hooks",
                true
            ),
            "[features]\nhooks = true\nother = 1\n[other]\nhooks = false\n"
        );
        assert!(toml_flag_matches(
            "[features]\nhooks = true\n",
            "features",
            "hooks",
            true
        ));
        assert!(!toml_flag_matches(
            "[features]\nhooks = false\n",
            "features",
            "hooks",
            true
        ));
        assert!(!toml_flag_matches("{not toml", "features", "hooks", true));
    }

    /// A file declared for another platform is neither written nor expected here, and the hook
    /// that names it is skipped with it.
    #[test]
    fn platform_filtering_selects_both_the_file_and_the_registration_that_names_it() {
        let body = format!(
            r#"
[[integrations]]
id = "demo"
version = 1
config_dir = ".demo"
[[integrations.files]]
source = "integration/here.sh"
dest = "here.sh"
platforms = ["{here}"]
[[integrations.files]]
source = "integration/there.ps1"
dest = "there.ps1"
platforms = ["{there}"]
[[integrations.registrations]]
kind = "json-hook"
file = "settings.json"
event = "SessionStart"
command_file = "here.sh"
[[integrations.registrations]]
kind = "json-hook"
file = "settings.json"
event = "SessionStart"
command_file = "there.ps1"
"#,
            here = crate::plugin::current_platform(),
            there = OTHER_PLATFORM,
        );
        let fixture = Fixture::new(
            &body,
            &[
                ("here.sh", &marked("demo", 1, "echo\n")),
                ("there.ps1", &marked("demo", 1, "echo\n")),
            ],
        );
        let adapter = fixture.adapter();
        let config = fixture.make_config_dir();
        adapter.install().unwrap();
        assert!(config.join("here.sh").exists());
        assert!(!config.join("there.ps1").exists());
        assert_eq!(adapter.status().unwrap(), Status::Current(1));

        let settings = fs::read_to_string(config.join("settings.json")).unwrap();
        assert_eq!(settings.matches("\"timeout\"").count(), 1);
        assert!(settings.contains("here.sh"));
        assert!(!settings.contains("there.ps1"));
    }

    /// A PowerShell script is not executable, so it is launched through the interpreter.
    #[test]
    fn hook_commands_quote_a_posix_path_and_interpret_a_powershell_one() {
        assert_eq!(
            hook_command(Path::new("/home/it's me/hook.sh"), &["session".into()]),
            "'/home/it'\\''s me/hook.sh' session"
        );
        assert_eq!(
            hook_command(Path::new("/home/me/hook.ps1"), &["session".into()]),
            "powershell -NoProfile -File \"/home/me/hook.ps1\" session"
        );
    }

    /// The notice is the manifest's, verbatim: it names a manual step vvmux will not take.
    #[test]
    fn a_notice_is_carried_through_from_the_manifest() {
        let fixture = Fixture::new(
            &SIMPLE.replace(
                "config_dir = \".demo\"",
                "config_dir = \".demo\"\nnotice = \"enable it by hand\"",
            ),
            &[("hook.sh", &marked("demo", 2, "echo\n"))],
        );
        assert_eq!(fixture.adapter().notice(), Some("enable it by hand"));
        assert_eq!(simple().adapter().notice(), None);
    }

    /// Nested directories the install created are removed; ones holding anything else are not.
    #[test]
    fn uninstall_prunes_only_the_directories_it_emptied() {
        let fixture = Fixture::new(
            r#"
[[integrations]]
id = "demo"
version = 1
config_dir = ".demo"
[[integrations.files]]
source = "integration/a.py"
dest = "plugins/vvmux-agent-state/__init__.py"
[[integrations.files]]
source = "integration/b.yaml"
dest = "plugins/vvmux-agent-state/plugin.yaml"
"#,
            &[
                ("a.py", &marked("demo", 1, "pass\n")),
                ("b.yaml", &marked("demo", 1, "name: x\n")),
            ],
        );
        let adapter = fixture.adapter();
        let config = fixture.make_config_dir();
        adapter.install().unwrap();
        assert!(config.join("plugins/vvmux-agent-state").is_dir());
        adapter.uninstall().unwrap();
        assert!(!config.join("plugins").exists());

        adapter.install().unwrap();
        fs::write(config.join("plugins/keep-me.js"), "// mine\n").unwrap();
        adapter.uninstall().unwrap();
        assert!(!config.join("plugins/vvmux-agent-state").exists());
        assert!(config.join("plugins/keep-me.js").exists());
    }
}
