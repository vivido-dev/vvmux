use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use jsonschema::{Draft, Validator};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
pub const MAX_SCHEMA_BYTES: u64 = 64 * 1024;
pub const MAX_SCHEMA_DEPTH: usize = 32;
pub const MAX_WORKFLOWS: usize = 128;
pub const MAX_WORKFLOW_STEPS: usize = 32;
pub const MAX_AGENTS_PER_PLUGIN: usize = 16;
pub const MAX_AGENT_EXECUTABLES: usize = 32;
pub const MAX_AGENT_EXECUTABLE_BYTES: usize = 128;
pub const MAX_AGENT_ARGV_MARKERS: usize = 16;
pub const MAX_AGENT_RULES: usize = 64;
pub const MAX_AGENT_GATE_DEPTH: usize = 8;
pub const MAX_AGENT_MATCHER_BYTES: usize = 4 * 1024;
pub const MAX_INTEGRATIONS_PER_PLUGIN: usize = 4;
pub const MAX_INTEGRATION_FILES: usize = 8;
pub const MAX_INTEGRATION_REGISTRATIONS: usize = 8;
pub const MAX_INTEGRATION_FILE_BYTES: u64 = 1024 * 1024;
/// Segments allowed in a home-relative `config_dir` or a config-relative `dest`.
pub const MAX_INTEGRATION_PATH_SEGMENTS: usize = 4;
pub const MAX_INTEGRATION_NOTICE_BYTES: usize = 512;
pub const MAX_INTEGRATION_ARGS: usize = 8;
pub const MAX_INTEGRATION_ARG_BYTES: usize = 128;

#[derive(Debug)]
pub enum ManifestError {
    Io(std::io::Error),
    Toml(toml::de::Error),
    Invalid(String),
    Schema { path: PathBuf, message: String },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Toml(error) => write!(formatter, "{error}"),
            Self::Invalid(message) => formatter.write_str(message),
            Self::Schema { path, message } => {
                write!(formatter, "schema {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<std::io::Error> for ManifestError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<toml::de::Error> for ManifestError {
    fn from(value: toml::de::Error) -> Self {
        Self::Toml(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub manifest_version: u16,
    pub plugin: Plugin,
    #[serde(default)]
    pub runtime: Option<Runtime>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default)]
    pub events: Vec<EventHook>,
    #[serde(default)]
    pub panes: Vec<Pane>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub workflows: Vec<Workflow>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub integrations: Vec<Integration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub version: Version,
    pub min_vvmux_version: Version,
    pub description: String,
    pub platforms: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    #[serde(rename = "session.read")]
    SessionRead,
    #[serde(rename = "pane.read")]
    PaneRead,
    #[serde(rename = "pane.input")]
    PaneInput,
    #[serde(rename = "pane.create")]
    PaneCreate,
    #[serde(rename = "pane.manage_own")]
    PaneManageOwn,
    #[serde(rename = "pane.manage_any")]
    PaneManageAny,
    #[serde(rename = "layout.read")]
    LayoutRead,
    #[serde(rename = "layout.write")]
    LayoutWrite,
    #[serde(rename = "events.subscribe")]
    EventsSubscribe,
    #[serde(rename = "plugin.invoke")]
    PluginInvoke,
    #[serde(rename = "clipboard.write")]
    ClipboardWrite,
    #[serde(rename = "media.produce")]
    MediaProduce,
    /// Write this plugin's declared lifecycle-adapter files into an agent's own config directory.
    ///
    /// Last in declaration order so the derived `Ord` keeps it last wherever permissions are
    /// listed, and deliberately absent from the session broker's enforceable set: it is an
    /// install-time authority over `$HOME`, not something a running plugin can exercise.
    #[serde(rename = "integration.write")]
    IntegrationWrite,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub kind: RuntimeKind,
    #[serde(default)]
    pub artifact: Option<PathBuf>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default = "default_activation")]
    pub activation: Activation,
    /// Explicit filesystem capabilities for a WebAssembly component.
    #[serde(default)]
    pub preopens: Vec<ComponentPreopen>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ComponentPreopen {
    /// The immutable installed package, mounted at `/package`.
    Package,
    /// User-managed plugin configuration, mounted read-only at `/config`.
    Config,
    /// Plugin-owned durable data, mounted read-write at `/data`.
    Data,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Component,
    Process,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    OnDemand,
    Session,
}

fn default_activation() -> Activation {
    Activation::OnDemand
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub handler: Option<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    pub input_schema: PathBuf,
    pub output_schema: PathBuf,
    #[serde(default)]
    pub agent_visible: bool,
    #[serde(default = "default_action_timeout")]
    pub timeout_ms: u64,
}

fn default_action_timeout() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventHook {
    pub on: String,
    #[serde(default)]
    pub handler: Option<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub include_self: bool,
    #[serde(default = "default_event_timeout")]
    pub timeout_ms: u64,
}

fn default_event_timeout() -> u64 {
    10_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pane {
    pub id: String,
    pub title: String,
    pub placement: Placement,
    pub command: Vec<String>,
    #[serde(default = "default_pane_hold")]
    pub hold_on_exit: bool,
    #[serde(default)]
    pub accept_sync_input: bool,
}

fn default_pane_hold() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    Split,
    Float,
    Tab,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub alias: String,
    pub id: String,
    pub version: VersionReq,
    pub source: String,
}

/// A lifecycle adapter this plugin installs into an agent's own configuration directory.
///
/// Declarative rather than code: the vvmux side is one generic engine, so a provider package adds
/// support for a new agent by describing where its files go and how that agent's configuration
/// registers them, without a vvmux release.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Integration {
    /// Ownership marker written into every managed file as `VVMUX_INTEGRATION_ID=<id>`.
    ///
    /// One id owns one config directory: the engine refuses to replace or remove a file whose
    /// first lines do not carry this marker, which is what keeps a hand-written hook of the user's
    /// own safe from an install.
    pub id: String,
    /// Bumped whenever the managed files change, and matched against
    /// `VVMUX_INTEGRATION_VERSION=<version>` to report an installed adapter as outdated.
    pub version: u32,
    /// Home-relative directory the agent reads its own configuration from.
    pub config_dir: PathBuf,
    /// Environment variable that relocates `config_dir` when the user has set one.
    #[serde(default)]
    pub config_dir_env: Option<String>,
    /// Printed after a successful install, for an agent whose enablement vvmux cannot perform.
    #[serde(default)]
    pub notice: Option<String>,
    #[serde(default)]
    pub files: Vec<IntegrationFile>,
    #[serde(default)]
    pub registrations: Vec<IntegrationRegistration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IntegrationFile {
    /// Package-relative file to copy.
    pub source: PathBuf,
    /// Destination relative to the resolved config directory.
    pub dest: PathBuf,
    /// Platforms this file belongs on; empty means every platform.
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub executable: bool,
}

/// One edit to an agent's own configuration file that makes a managed file take effect.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum IntegrationRegistration {
    /// Merge a command hook into a JSON configuration file, preserving every foreign entry.
    JsonHook {
        /// Config-relative JSON file to edit.
        file: PathBuf,
        event: String,
        #[serde(default)]
        matcher: Option<String>,
        /// The `dest` of a declared file, run as the hook command.
        command_file: PathBuf,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Set one boolean key in a TOML table, leaving every other line untouched.
    TomlFlag {
        /// Config-relative TOML file to edit.
        file: PathBuf,
        section: String,
        key: String,
        value: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub id: String,
    pub title: String,
    #[serde(default = "manual_trigger")]
    pub trigger: String,
    #[serde(default)]
    pub agent_visible: bool,
    #[serde(default)]
    pub input_schema: Option<PathBuf>,
    #[serde(default)]
    pub output_schema: Option<PathBuf>,
    #[serde(default = "default_action_timeout")]
    pub timeout_ms: u64,
    pub output: Value,
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
}

fn manual_trigger() -> String {
    "manual".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStep {
    pub id: String,
    pub uses: String,
    #[serde(default, rename = "with")]
    pub input: Value,
    #[serde(default)]
    pub needs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub process: AgentProcess,
    /// How to start this agent, for providers that support being launched.
    ///
    /// Absent means detection-only: the agent is recognized when a user runs it, but `agent-start`
    /// refuses rather than guessing a command from the detection matchers. Those matchers describe
    /// what a running agent looks like — including wrapper scripts and package paths — which is not
    /// the same thing as what to type to start one.
    #[serde(default)]
    pub launch: Option<AgentLaunch>,
    #[serde(default)]
    pub rules: Vec<AgentRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentLaunch {
    /// The command name to type, resolved by the pane's shell through PATH.
    ///
    /// A bare name, never a path: the whole point is to run whatever the user's environment means
    /// by `claude`, and a manifest cannot know where that is.
    pub executable: String,
    /// Arguments appended to `executable` to reopen a previous conversation.
    ///
    /// Exactly one element carries `{session_id}` or `{session_path}`, substituted with the identity
    /// the agent's own integration reported. Absent means this agent cannot be resumed, and a
    /// restored pane is a plain shell.
    ///
    /// A template rather than a hardcoded table, so a user's own provider plugin can make its agent
    /// resumable without a vvmux release. The grammar is wide enough for every shape the shipped
    /// agents use — `--resume <id>`, `resume <id>`, `--resume=<id>`, `--session <id>` — and no wider.
    #[serde(default)]
    pub resume: Option<Vec<String>>,
}

/// The placeholders a resume template may carry.
pub const RESUME_ID_PLACEHOLDER: &str = "{session_id}";
pub const RESUME_PATH_PLACEHOLDER: &str = "{session_path}";
/// Arguments one resume template may hold.
pub const MAX_AGENT_RESUME_ARGS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentProcess {
    #[serde(default)]
    pub executables: Vec<String>,
    #[serde(default)]
    pub argv_contains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentRule {
    pub id: String,
    pub state: AgentRuleState,
    pub priority: u16,
    #[serde(default = "default_agent_rule_region")]
    pub region: String,
    #[serde(default)]
    pub visible_idle: bool,
    #[serde(default)]
    pub skip_state_update: bool,
    #[serde(flatten)]
    pub gate: AgentGate,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentGate {
    #[serde(default)]
    pub contains: Vec<String>,
    #[serde(default)]
    pub regex: Vec<String>,
    #[serde(default)]
    pub line_regex: Vec<String>,
    #[serde(default)]
    pub all: Vec<AgentGate>,
    #[serde(default)]
    pub any: Vec<AgentGate>,
    #[serde(default, rename = "not")]
    pub not_gate: Vec<AgentGate>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRuleState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

fn default_agent_rule_region() -> String {
    "whole_recent".into()
}

pub struct SchemaDocument {
    pub path: PathBuf,
    pub value: Value,
    validator: Validator,
}

impl fmt::Debug for SchemaDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SchemaDocument")
            .field("path", &self.path)
            .field("value", &self.value)
            .finish_non_exhaustive()
    }
}

impl SchemaDocument {
    pub fn validate(&self, instance: &Value) -> Result<(), Vec<String>> {
        let errors = self
            .validator
            .iter_errors(instance)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

pub fn validate_schema_instance(schema: &Value, instance: &Value) -> Result<(), Vec<String>> {
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(false)
        .build(schema)
        .map_err(|error| vec![error.to_string()])?;
    let errors = validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[derive(Debug)]
pub struct LoadedManifest {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub schemas: BTreeMap<PathBuf, SchemaDocument>,
    pub warnings: Vec<String>,
}

impl LoadedManifest {
    pub fn load(root: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let root = root.as_ref();
        let manifest_path = root.join("vvmux-plugin.toml");
        let metadata = fs::metadata(&manifest_path)?;
        if metadata.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::Invalid("manifest exceeds 1 MiB".into()));
        }
        let source = fs::read_to_string(&manifest_path)?;
        let manifest: Manifest = toml::from_str(&source)?;
        manifest.validate()?;
        if let Some(artifact) = manifest
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.artifact.as_deref())
        {
            let resolved = root.join(artifact);
            ensure_package_file(root, &resolved, artifact)?;
            if fs::metadata(&resolved)?.len() > 32 * 1024 * 1024 {
                return Err(ManifestError::Invalid(
                    "WebAssembly component artifact exceeds 32 MiB".into(),
                ));
            }
        }
        // Integration payloads are read straight out of the package and written into the user's
        // home directory, so they get the same symlink/escape guard and a size bound before the
        // engine ever opens one.
        for file in manifest
            .integrations
            .iter()
            .flat_map(|integration| &integration.files)
        {
            validate_relative_path(&file.source, "integration file source")?;
            let resolved = root.join(&file.source);
            ensure_package_file(root, &resolved, &file.source)?;
            if fs::metadata(&resolved)?.len() > MAX_INTEGRATION_FILE_BYTES {
                return Err(ManifestError::Invalid(format!(
                    "integration file `{}` exceeds 1 MiB",
                    file.source.display()
                )));
            }
        }

        let mut schemas = BTreeMap::new();
        for path in manifest
            .actions
            .iter()
            .flat_map(|action| [&action.input_schema, &action.output_schema])
            .chain(manifest.workflows.iter().flat_map(|workflow| {
                [
                    workflow.input_schema.as_ref(),
                    workflow.output_schema.as_ref(),
                ]
                .into_iter()
                .flatten()
            }))
        {
            if schemas.contains_key(path) {
                continue;
            }
            validate_relative_path(path, "schema")?;
            let resolved = root.join(path);
            ensure_package_file(root, &resolved, path)?;
            let bytes = fs::read(&resolved)?;
            if bytes.len() as u64 > MAX_SCHEMA_BYTES {
                return Err(ManifestError::Schema {
                    path: path.clone(),
                    message: "document exceeds 64 KiB".into(),
                });
            }
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|error| ManifestError::Schema {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
            inspect_schema(&value, 0, path)?;
            let validator = jsonschema::options()
                .with_draft(Draft::Draft202012)
                .should_validate_formats(false)
                .build(&value)
                .map_err(|error| ManifestError::Schema {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
            schemas.insert(
                path.clone(),
                SchemaDocument {
                    path: path.clone(),
                    value,
                    validator,
                },
            );
        }

        let known_events = [
            "pane.opened",
            "pane.exited",
            "pane.closed",
            "pane.screen_changed",
            "agent.status_changed",
            "layout.changed",
            "focus.changed",
            "config.changed",
            "media.changed",
            "plugin.job_completed",
            "plugin.runtime_crashed",
        ];
        let warnings = manifest
            .events
            .iter()
            .filter(|hook| !known_events.contains(&hook.on.as_str()))
            .map(|hook| format!("unknown event `{}` is inactive", hook.on))
            .collect();
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            schemas,
            warnings,
        })
    }

    pub fn action(&self, id: &str) -> Option<&Action> {
        self.manifest.actions.iter().find(|action| action.id == id)
    }

    pub fn validate_input(&self, action: &Action, value: &Value) -> Result<(), Vec<String>> {
        self.schemas[&action.input_schema].validate(value)
    }

    pub fn validate_output(&self, action: &Action, value: &Value) -> Result<(), Vec<String>> {
        self.schemas[&action.output_schema].validate(value)
    }
}

impl Manifest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !matches!(self.manifest_version, 1 | 2) {
            return invalid("unsupported manifest_version (expected 1 or 2)");
        }
        if self.manifest_version == 1 && !self.agents.is_empty() {
            return invalid("agent definitions require manifest_version = 2");
        }
        if self.manifest_version == 1 && !self.integrations.is_empty() {
            return invalid("integration definitions require manifest_version = 2");
        }
        validate_plugin_id(&self.plugin.id)?;
        if self.plugin.name.is_empty() || self.plugin.name.len() > 128 {
            return invalid("plugin name must contain 1 through 128 bytes");
        }
        if self.plugin.description.len() > 4096 {
            return invalid("plugin description exceeds 4096 bytes");
        }
        if self.plugin.platforms.is_empty()
            || self
                .plugin
                .platforms
                .iter()
                .any(|value| !matches!(value.as_str(), "linux" | "macos" | "windows"))
        {
            return invalid("platforms must contain linux, macos, or windows");
        }
        let unique_permissions = self.plugin.permissions.iter().collect::<BTreeSet<_>>();
        if unique_permissions.len() != self.plugin.permissions.len() {
            return invalid("plugin permissions contain duplicates");
        }
        match &self.runtime {
            Some(runtime) => runtime.validate()?,
            None if self.actions.iter().any(|action| action.handler.is_some())
                || self.events.iter().any(|event| event.handler.is_some()) =>
            {
                return invalid("handler entrypoints require a runtime");
            }
            None => {}
        }

        let mut ids = BTreeSet::new();
        for action in &self.actions {
            validate_local_id(&action.id, "action")?;
            if !ids.insert((&action.id, "action")) {
                return invalid(format!("duplicate action id `{}`", action.id));
            }
            exactly_one(&action.handler, &action.command, "action", &action.id)?;
            if let Some(handler) = &action.handler {
                validate_local_id(handler, "action handler")?;
            }
            validate_argv(action.command.as_deref(), "action", &action.id)?;
            if !(1..=24 * 60 * 60 * 1000).contains(&action.timeout_ms) {
                return invalid(format!("action `{}` has an invalid timeout", action.id));
            }
        }
        for event in &self.events {
            exactly_one(&event.handler, &event.command, "event", &event.on)?;
            if let Some(handler) = &event.handler {
                validate_local_id(handler, "event handler")?;
            }
            validate_argv(event.command.as_deref(), "event", &event.on)?;
            if !(1..=24 * 60 * 60 * 1000).contains(&event.timeout_ms) {
                return invalid(format!("event `{}` has an invalid timeout", event.on));
            }
        }
        for pane in &self.panes {
            validate_local_id(&pane.id, "pane")?;
            if !ids.insert((&pane.id, "pane")) {
                return invalid(format!("duplicate pane id `{}`", pane.id));
            }
            if pane.title.is_empty()
                || pane.title.len() > 128
                || pane.title.chars().any(char::is_control)
            {
                return invalid(format!(
                    "pane `{}` title must contain 1 through 128 printable bytes",
                    pane.id
                ));
            }
            validate_argv(Some(&pane.command), "pane", &pane.id)?;
        }
        let mut aliases = BTreeSet::new();
        for dependency in &self.dependencies {
            validate_local_id(&dependency.alias, "dependency alias")?;
            validate_plugin_id(&dependency.id)?;
            if !aliases.insert(dependency.alias.as_str()) {
                return invalid(format!("duplicate dependency alias `{}`", dependency.alias));
            }
            if !dependency.source.starts_with("https://") {
                return invalid(format!(
                    "dependency `{}` source must be HTTPS",
                    dependency.alias
                ));
            }
        }
        let action_ids = self
            .actions
            .iter()
            .map(|action| action.id.as_str())
            .collect::<BTreeSet<_>>();
        if self.workflows.len() > MAX_WORKFLOWS {
            return invalid("plugin exceeds 128 workflows");
        }
        validate_workflows(&self.workflows, &aliases, &action_ids)?;
        validate_agents(&self.agents)?;
        validate_integrations(&self.integrations, &self.plugin.permissions)?;
        Ok(())
    }
}

fn validate_integrations(
    integrations: &[Integration],
    permissions: &[Permission],
) -> Result<(), ManifestError> {
    if integrations.len() > MAX_INTEGRATIONS_PER_PLUGIN {
        return invalid(format!(
            "plugin exceeds {MAX_INTEGRATIONS_PER_PLUGIN} integrations"
        ));
    }
    // The engine writes into the user's own home directory, which no other manifest table does, so
    // the permission is the thing that puts it in front of the install prompt as an added
    // permission rather than as an unannounced side effect of a package update.
    if !integrations.is_empty() && !permissions.contains(&Permission::IntegrationWrite) {
        return invalid("integrations require the `integration.write` permission");
    }
    let mut ids = BTreeSet::new();
    for integration in integrations {
        validate_local_id(&integration.id, "integration")?;
        if !ids.insert(integration.id.as_str()) {
            return invalid(format!("duplicate integration id `{}`", integration.id));
        }
        if integration.version == 0 {
            return invalid(format!(
                "integration `{}` version must be at least 1",
                integration.id
            ));
        }
        validate_integration_path(&integration.config_dir, "config_dir", &integration.id)?;
        if let Some(name) = &integration.config_dir_env
            && !valid_environment_name(name)
        {
            return invalid(format!(
                "integration `{}` config_dir_env must be an uppercase environment variable name",
                integration.id
            ));
        }
        if let Some(notice) = &integration.notice
            && (notice.is_empty()
                || notice.len() > MAX_INTEGRATION_NOTICE_BYTES
                || notice
                    .chars()
                    .any(|character| character.is_control() && character != '\n'))
        {
            return invalid(format!(
                "integration `{}` notice must contain 1 through {MAX_INTEGRATION_NOTICE_BYTES} printable bytes",
                integration.id
            ));
        }
        if integration.files.len() > MAX_INTEGRATION_FILES {
            return invalid(format!(
                "integration `{}` exceeds {MAX_INTEGRATION_FILES} files",
                integration.id
            ));
        }
        let mut destinations = BTreeSet::new();
        for file in &integration.files {
            validate_relative_path(&file.source, "integration file source")?;
            validate_integration_path(&file.dest, "file dest", &integration.id)?;
            if !destinations.insert(file.dest.as_path()) {
                return invalid(format!(
                    "integration `{}` declares `{}` twice",
                    integration.id,
                    file.dest.display()
                ));
            }
            validate_platforms(&file.platforms, &integration.id)?;
        }
        if integration.registrations.len() > MAX_INTEGRATION_REGISTRATIONS {
            return invalid(format!(
                "integration `{}` exceeds {MAX_INTEGRATION_REGISTRATIONS} registrations",
                integration.id
            ));
        }
        for registration in &integration.registrations {
            match registration {
                IntegrationRegistration::JsonHook {
                    file,
                    event,
                    matcher,
                    command_file,
                    args,
                } => {
                    validate_integration_path(file, "registration file", &integration.id)?;
                    validate_integration_word(event, "event", &integration.id)?;
                    if let Some(matcher) = matcher {
                        validate_integration_short_text(matcher, "matcher", &integration.id)?;
                    }
                    // A registration may only name a file this same integration owns: the hook
                    // command is a path that will be executed, and pointing it at anything else
                    // would let a manifest register a command it never declared.
                    if !destinations.contains(command_file.as_path()) {
                        return invalid(format!(
                            "integration `{}` registers undeclared command file `{}`",
                            integration.id,
                            command_file.display()
                        ));
                    }
                    if args.len() > MAX_INTEGRATION_ARGS {
                        return invalid(format!(
                            "integration `{}` registration exceeds {MAX_INTEGRATION_ARGS} arguments",
                            integration.id
                        ));
                    }
                    for argument in args {
                        validate_integration_word(argument, "argument", &integration.id)?;
                    }
                }
                IntegrationRegistration::TomlFlag {
                    file,
                    section,
                    key,
                    value: _,
                } => {
                    validate_integration_path(file, "registration file", &integration.id)?;
                    validate_integration_word(section, "section", &integration.id)?;
                    validate_integration_word(key, "key", &integration.id)?;
                }
            }
        }
    }
    Ok(())
}

/// A bounded, relative, escape-free path with no more than four segments.
///
/// Both kinds this checks are resolved against a directory the user owns — `$HOME` for a
/// `config_dir`, the agent's config directory for a `dest` — so an absolute path or a `..`
/// component would let a manifest choose a destination outside the tree the install is about.
fn validate_integration_path(path: &Path, what: &str, id: &str) -> Result<(), ManifestError> {
    validate_relative_path(path, &format!("integration `{id}` {what}"))?;
    let segments = path.components().count();
    if segments == 0 || segments > MAX_INTEGRATION_PATH_SEGMENTS {
        return invalid(format!(
            "integration `{id}` {what} must contain 1 through {MAX_INTEGRATION_PATH_SEGMENTS} segments"
        ));
    }
    Ok(())
}

/// One shell-safe word: a hook argument is appended to a command line unquoted.
fn validate_integration_word(value: &str, what: &str, id: &str) -> Result<(), ManifestError> {
    if value.is_empty()
        || value.len() > MAX_INTEGRATION_ARG_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'='))
    {
        return invalid(format!(
            "integration `{id}` has an invalid {what} `{value}`"
        ));
    }
    Ok(())
}

fn validate_integration_short_text(value: &str, what: &str, id: &str) -> Result<(), ManifestError> {
    if value.is_empty()
        || value.len() > MAX_INTEGRATION_ARG_BYTES
        || value.chars().any(char::is_control)
    {
        return invalid(format!("integration `{id}` has an invalid {what}"));
    }
    Ok(())
}

fn validate_platforms(platforms: &[String], id: &str) -> Result<(), ManifestError> {
    let mut unique = BTreeSet::new();
    for platform in platforms {
        if !matches!(platform.as_str(), "linux" | "macos" | "windows")
            || !unique.insert(platform.as_str())
        {
            return invalid(format!(
                "integration `{id}` has an invalid platform `{platform}`"
            ));
        }
    }
    Ok(())
}

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_uppercase() || *byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate_agents(agents: &[Agent]) -> Result<(), ManifestError> {
    if agents.len() > MAX_AGENTS_PER_PLUGIN {
        return invalid("plugin exceeds 16 agent definitions");
    }
    let mut agent_ids = BTreeSet::new();
    for agent in agents {
        validate_local_id(&agent.id, "agent")?;
        if !agent_ids.insert(agent.id.as_str()) {
            return invalid(format!("duplicate agent id `{}`", agent.id));
        }
        if agent.name.is_empty()
            || agent.name.len() > 128
            || agent.name.chars().any(char::is_control)
        {
            return invalid(format!(
                "agent `{}` name must contain 1 through 128 printable bytes",
                agent.id
            ));
        }
        if agent.process.executables.is_empty() && agent.process.argv_contains.is_empty() {
            return invalid(format!(
                "agent `{}` requires at least one process matcher",
                agent.id
            ));
        }
        validate_agent_matchers(
            &agent.process.executables,
            MAX_AGENT_EXECUTABLES,
            "executable",
            &agent.id,
        )?;
        validate_agent_matchers(
            &agent.process.argv_contains,
            MAX_AGENT_ARGV_MARKERS,
            "argv marker",
            &agent.id,
        )?;
        if let Some(launch) = &agent.launch {
            let executable = &launch.executable;
            if executable.is_empty()
                || executable.len() > MAX_AGENT_EXECUTABLE_BYTES
                || executable.chars().any(char::is_control)
            {
                return invalid(format!(
                    "agent `{}` launch executable must contain 1 through {MAX_AGENT_EXECUTABLE_BYTES} printable bytes",
                    agent.id
                ));
            }
            // A path would be typed at a shell verbatim, so accepting one would let a manifest
            // choose which binary runs rather than deferring to the user's PATH. Whitespace would
            // split into two arguments once quoted, meaning the name would no longer be one word.
            if executable.contains(['/', '\\']) || executable.chars().any(char::is_whitespace) {
                return invalid(format!(
                    "agent `{}` launch executable must be a bare command name",
                    agent.id
                ));
            }
            if let Some(resume) = &launch.resume {
                validate_resume_template(&agent.id, resume)?;
            }
        }
        if agent.rules.len() > MAX_AGENT_RULES {
            return invalid(format!("agent `{}` exceeds 64 rules", agent.id));
        }
        let mut rule_ids = BTreeSet::new();
        for rule in &agent.rules {
            validate_local_id(&rule.id, "agent rule")?;
            if !rule_ids.insert(rule.id.as_str()) {
                return invalid(format!(
                    "agent `{}` has duplicate rule `{}`",
                    agent.id, rule.id
                ));
            }
            validate_agent_region(&rule.region, &agent.id, &rule.id)?;
            validate_agent_gate(&rule.gate, 0, &agent.id, &rule.id)?;
        }
    }
    Ok(())
}

fn validate_agent_matchers(
    values: &[String],
    limit: usize,
    kind: &str,
    agent: &str,
) -> Result<(), ManifestError> {
    if values.len() > limit {
        return invalid(format!("agent `{agent}` has too many {kind} matchers"));
    }
    let mut unique = BTreeSet::new();
    for value in values {
        if value.is_empty()
            || value.len() > MAX_AGENT_MATCHER_BYTES
            || value.contains('\0')
            || !unique.insert(value.to_ascii_lowercase())
        {
            return invalid(format!("agent `{agent}` has an invalid {kind} matcher"));
        }
    }
    Ok(())
}

fn validate_agent_region(region: &str, agent: &str, rule: &str) -> Result<(), ManifestError> {
    let named = matches!(
        region,
        "osc_title"
            | "osc_progress"
            | "whole_recent"
            | "after_last_prompt_marker"
            | "prompt_box_body"
            | "after_last_horizontal_rule"
    );
    let bottom = region
        .strip_prefix("bottom_non_empty_lines(")
        .and_then(|value| value.strip_suffix(')'))
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|count| (1..=64).contains(&count));
    if named || bottom {
        Ok(())
    } else {
        invalid(format!(
            "agent `{agent}` rule `{rule}` has an invalid region"
        ))
    }
}

fn validate_agent_gate(
    gate: &AgentGate,
    depth: usize,
    agent: &str,
    rule: &str,
) -> Result<(), ManifestError> {
    if depth > MAX_AGENT_GATE_DEPTH {
        return invalid(format!(
            "agent `{agent}` rule `{rule}` exceeds eight nested gate levels"
        ));
    }
    for matcher in gate
        .contains
        .iter()
        .chain(&gate.regex)
        .chain(&gate.line_regex)
    {
        if matcher.is_empty() || matcher.len() > MAX_AGENT_MATCHER_BYTES || matcher.contains('\0') {
            return invalid(format!(
                "agent `{agent}` rule `{rule}` has an invalid matcher"
            ));
        }
    }
    for pattern in gate.regex.iter().chain(&gate.line_regex) {
        regex::Regex::new(pattern).map_err(|error| {
            ManifestError::Invalid(format!(
                "agent `{agent}` rule `{rule}` has an invalid regex: {error}"
            ))
        })?;
    }
    for nested in gate.all.iter().chain(&gate.any).chain(&gate.not_gate) {
        validate_agent_gate(nested, depth + 1, agent, rule)?;
    }
    Ok(())
}

impl Runtime {
    fn validate(&self) -> Result<(), ManifestError> {
        match self.kind {
            RuntimeKind::Component => {
                if self.command.is_some() || self.artifact.is_none() {
                    return invalid("component runtime requires artifact and forbids command");
                }
                validate_relative_path(self.artifact.as_ref().unwrap(), "runtime artifact")?;
                let mut preopens = BTreeSet::new();
                for preopen in &self.preopens {
                    if !preopens.insert(*preopen) {
                        return invalid("component runtime contains a duplicate preopen");
                    }
                }
            }
            RuntimeKind::Process => {
                if self.artifact.is_some() || self.command.is_none() || !self.preopens.is_empty() {
                    return invalid(
                        "process runtime requires command and forbids artifact and preopens",
                    );
                }
                validate_argv(self.command.as_deref(), "runtime", "process")?;
            }
        }
        Ok(())
    }
}

fn validate_workflows(
    workflows: &[Workflow],
    aliases: &BTreeSet<&str>,
    action_ids: &BTreeSet<&str>,
) -> Result<(), ManifestError> {
    let mut workflow_ids = BTreeSet::new();
    for workflow in workflows {
        validate_local_id(&workflow.id, "workflow")?;
        if action_ids.contains(workflow.id.as_str()) {
            return invalid(format!(
                "workflow id `{}` conflicts with an action id",
                workflow.id
            ));
        }
        if !workflow_ids.insert(workflow.id.as_str()) {
            return invalid(format!("duplicate workflow id `{}`", workflow.id));
        }
        if workflow.steps.len() > MAX_WORKFLOW_STEPS {
            return invalid(format!("workflow `{}` exceeds 32 steps", workflow.id));
        }
        if !(1..=24 * 60 * 60 * 1000).contains(&workflow.timeout_ms) {
            return invalid(format!("workflow `{}` has an invalid timeout", workflow.id));
        }
        if let Some(path) = &workflow.input_schema {
            validate_relative_path(path, "workflow input schema")?;
        }
        if let Some(path) = &workflow.output_schema {
            validate_relative_path(path, "workflow output schema")?;
        }
        let mut step_ids = BTreeSet::new();
        let mut resolved = workflow.clone();
        for step in &workflow.steps {
            validate_local_id(&step.id, "workflow step")?;
            if !step_ids.insert(step.id.as_str()) {
                return invalid(format!(
                    "workflow `{}` has duplicate step `{}`",
                    workflow.id, step.id
                ));
            }
            let Some((alias, action)) = step.uses.split_once('/') else {
                return invalid(format!(
                    "workflow step `{}` uses must be dependency/action",
                    step.id
                ));
            };
            if !aliases.contains(alias) {
                return invalid(format!(
                    "workflow step `{}` references undeclared dependency `{alias}`",
                    step.id
                ));
            }
            validate_local_id(action, "dependency action")?;
        }
        for step in &mut resolved.steps {
            for reference in substitution_steps(&step.input, &workflow.id)? {
                if !step.needs.contains(&reference) {
                    step.needs.push(reference);
                }
            }
        }
        let _ = substitution_steps(&workflow.output, &workflow.id)?;
        for step in &resolved.steps {
            for need in &step.needs {
                if !step_ids.contains(need.as_str()) {
                    return invalid(format!(
                        "workflow step `{}` needs unknown step `{need}`",
                        step.id
                    ));
                }
            }
        }
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for id in &step_ids {
            visit_step(id, &resolved, &mut visiting, &mut visited)?;
        }
    }
    Ok(())
}

fn substitution_steps(value: &Value, workflow: &str) -> Result<Vec<String>, ManifestError> {
    fn visit(value: &Value, workflow: &str, result: &mut Vec<String>) -> Result<(), ManifestError> {
        match value {
            Value::String(string) if string.contains("${") => {
                let Some(inner) = string
                    .strip_prefix("${")
                    .and_then(|value| value.strip_suffix('}'))
                else {
                    return invalid(format!(
                        "workflow `{workflow}` contains an ambiguous substitution"
                    ));
                };
                if inner == "trigger" || inner.starts_with("trigger#/") {
                    return Ok(());
                }
                let Some(rest) = inner.strip_prefix("steps.") else {
                    return invalid(format!(
                        "workflow `{workflow}` contains an unknown substitution root"
                    ));
                };
                let Some((step, pointer)) = rest.split_once(".output") else {
                    return invalid(format!(
                        "workflow `{workflow}` step substitution must reference output"
                    ));
                };
                validate_local_id(step, "substitution step")?;
                if !pointer.is_empty() && !pointer.starts_with("#/") {
                    return invalid(format!(
                        "workflow `{workflow}` substitution uses an invalid JSON Pointer"
                    ));
                }
                result.push(step.to_owned());
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, workflow, result)?;
                }
            }
            Value::Object(values) => {
                for value in values.values() {
                    visit(value, workflow, result)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut result = Vec::new();
    visit(value, workflow, &mut result)?;
    Ok(result)
}

fn visit_step<'a>(
    id: &'a str,
    workflow: &'a Workflow,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> Result<(), ManifestError> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return invalid(format!(
            "workflow `{}` contains a cycle at `{id}`",
            workflow.id
        ));
    }
    let step = workflow.steps.iter().find(|step| step.id == id).unwrap();
    for need in &step.needs {
        visit_step(need, workflow, visiting, visited)?;
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

fn exactly_one<T, U>(
    left: &Option<T>,
    right: &Option<U>,
    kind: &str,
    id: &str,
) -> Result<(), ManifestError> {
    if left.is_some() == right.is_some() {
        return invalid(format!(
            "{kind} `{id}` requires exactly one of handler or command"
        ));
    }
    Ok(())
}

fn validate_argv(argv: Option<&[String]>, kind: &str, id: &str) -> Result<(), ManifestError> {
    let Some(argv) = argv else { return Ok(()) };
    if argv.is_empty() || argv[0].is_empty() || argv.len() > 256 {
        return invalid(format!("{kind} `{id}` has an invalid argv"));
    }
    if argv
        .iter()
        .any(|arg| arg.len() > 64 * 1024 || arg.contains('\0'))
    {
        return invalid(format!("{kind} `{id}` argv contains an invalid argument"));
    }
    Ok(())
}

fn validate_plugin_id(id: &str) -> Result<(), ManifestError> {
    if id.len() < 3 || id.len() > 128 || !id.contains('.') {
        return invalid("plugin id must be a reverse-domain name of 3 through 128 bytes");
    }
    if id.split('.').any(|part| !valid_segment(part)) {
        return invalid(format!("invalid plugin id `{id}`"));
    }
    Ok(())
}

/// A resume template must name the session exactly once, and every argument must survive being
/// typed at a shell.
///
/// One placeholder rather than any number: the substitution builds a command line, and a template
/// that repeated the identity or omitted it would either run the wrong command or run one that
/// resumes nothing. The placeholder is either the whole argument or the tail of a `--flag=` form,
/// because those are the only two shapes an agent CLI actually uses and anything else would be a
/// manifest constructing arguments rather than describing them.
fn validate_resume_template(agent: &str, resume: &[String]) -> Result<(), ManifestError> {
    if resume.is_empty() || resume.len() > MAX_AGENT_RESUME_ARGS {
        return invalid(format!(
            "agent `{agent}` resume must contain 1 through {MAX_AGENT_RESUME_ARGS} arguments"
        ));
    }
    let mut placeholders = 0_usize;
    for argument in resume {
        if argument.is_empty()
            || argument.len() > MAX_AGENT_EXECUTABLE_BYTES
            || argument.chars().any(char::is_control)
        {
            return invalid(format!(
                "agent `{agent}` resume arguments must each contain 1 through {MAX_AGENT_EXECUTABLE_BYTES} printable bytes"
            ));
        }
        for placeholder in [RESUME_ID_PLACEHOLDER, RESUME_PATH_PLACEHOLDER] {
            let occurrences = argument.matches(placeholder).count();
            if occurrences == 0 {
                continue;
            }
            placeholders = placeholders.saturating_add(occurrences);
            let whole = argument == placeholder;
            let flag_tail = argument
                .strip_suffix(placeholder)
                .is_some_and(|prefix| prefix.starts_with('-') && prefix.ends_with('='));
            if !whole && !flag_tail {
                return invalid(format!(
                    "agent `{agent}` resume placeholder must be a whole argument or follow `--flag=`"
                ));
            }
        }
    }
    if placeholders != 1 {
        return invalid(format!(
            "agent `{agent}` resume must name the session exactly once"
        ));
    }
    Ok(())
}

fn validate_local_id(id: &str, kind: &str) -> Result<(), ManifestError> {
    if id.is_empty() || id.len() > 64 || id.contains('.') || !valid_segment(id) {
        return invalid(format!("invalid {kind} id `{id}`"));
    }
    Ok(())
}

fn valid_segment(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_relative_path(path: &Path, what: &str) -> Result<(), ManifestError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return invalid(format!("{what} must be a package-relative safe path"));
    }
    Ok(())
}

fn inspect_schema(value: &Value, depth: usize, path: &Path) -> Result<(), ManifestError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(ManifestError::Schema {
            path: path.to_path_buf(),
            message: "document exceeds 32 levels".into(),
        });
    }
    match value {
        Value::Object(object) => {
            if let Some(Value::String(reference)) = object.get("$ref")
                && (reference.contains("://") || !reference.starts_with('#'))
            {
                return Err(ManifestError::Schema {
                    path: path.to_path_buf(),
                    message: "only local fragment $ref values are allowed".into(),
                });
            }
            for child in object.values() {
                inspect_schema(child, depth + 1, path)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                inspect_schema(child, depth + 1, path)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn ensure_package_file(root: &Path, resolved: &Path, display: &Path) -> Result<(), ManifestError> {
    let metadata = fs::symlink_metadata(resolved).map_err(ManifestError::Io)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ManifestError::Schema {
            path: display.to_path_buf(),
            message: "must be a regular package file, not a symlink".into(),
        });
    }
    let canonical_root = fs::canonicalize(root)?;
    let canonical_file = fs::canonicalize(resolved)?;
    if !canonical_file.starts_with(canonical_root) {
        return Err(ManifestError::Schema {
            path: display.to_path_buf(),
            message: "resolves outside the package".into(),
        });
    }
    Ok(())
}

fn invalid<T>(message: impl Into<String>) -> Result<T, ManifestError> {
    Err(ManifestError::Invalid(message.into()))
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    fn agent_with_resume(resume: &str) -> String {
        format!(
            r#"manifest_version = 2
[plugin]
id = "com.example.a"
name = "A"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "d"
platforms = ["linux"]
permissions = []
[[agents]]
id = "demo"
name = "Demo"
process = {{ executables = ["demo"] }}
launch = {{ executable = "demo", resume = {resume} }}
"#
        )
    }

    fn accepted(resume: &str) -> bool {
        let manifest: Manifest = toml::from_str(&agent_with_resume(resume)).unwrap();
        manifest.validate().is_ok()
    }

    /// Every shape the shipped agents actually use, and nothing that would let a manifest build
    /// arguments rather than describe them.
    #[test]
    fn resume_templates_accept_real_agent_shapes_only() {
        for valid in [
            r#"["--resume", "{session_id}"]"#,  // claude, hermes
            r#"["resume", "{session_id}"]"#,    // codex
            r#"["--session", "{session_id}"]"#, // opencode
            r#"["--resume={session_id}"]"#,     // copilot-style flag=value
            r#"["--session", "{session_path}"]"#,
        ] {
            assert!(accepted(valid), "expected {valid} to be accepted");
        }
        for invalid in [
            r#"[]"#,                               // names no session
            r#"["--resume"]"#,                     // no placeholder
            r#"["{session_id}", "{session_id}"]"#, // names it twice
            r#"["{session_id}{session_path}"]"#,   // two kinds in one argument
            r#"["--resume", "id-{session_id}"]"#,  // built rather than described
            r#"["--resume", "{session_id}-suffix"]"#,
            r#"["a","b","c","d","e","f","g","h","{session_id}"]"#, // over the argument bound
        ] {
            assert!(!accepted(invalid), "expected {invalid} to be refused");
        }
    }

    #[test]
    fn resume_arguments_are_bounded_and_printable() {
        let long = "x".repeat(MAX_AGENT_EXECUTABLE_BYTES + 1);
        assert!(!accepted(&format!(r#"["{long}", "{{session_id}}"]"#)));
        assert!(!accepted(r#"["--resume\u0007", "{session_id}"]"#));
    }

    /// Absent means detection-only, which has to stay valid: most agents cannot be resumed.
    #[test]
    fn a_launchable_agent_need_not_be_resumable() {
        let source = agent_with_resume(r#"["--resume", "{session_id}"]"#)
            .replace(r#", resume = ["--resume", "{session_id}"]"#, "");
        let manifest: Manifest = toml::from_str(&source).unwrap();
        manifest.validate().unwrap();
        assert!(manifest.agents[0].launch.as_ref().unwrap().resume.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> Manifest {
        Manifest {
            manifest_version: 1,
            plugin: Plugin {
                id: "dev.example".into(),
                name: "Example".into(),
                version: Version::new(1, 0, 0),
                min_vvmux_version: Version::new(0, 5, 0),
                description: "test".into(),
                platforms: vec!["linux".into()],
                permissions: vec![Permission::PaneRead],
            },
            runtime: Some(Runtime {
                kind: RuntimeKind::Process,
                artifact: None,
                command: Some(vec!["python".into(), "plugin.py".into()]),
                activation: Activation::OnDemand,
                preopens: Vec::new(),
            }),
            actions: vec![Action {
                id: "read".into(),
                title: "Read".into(),
                description: "read".into(),
                handler: Some("read".into()),
                command: None,
                input_schema: "in.json".into(),
                output_schema: "out.json".into(),
                agent_visible: true,
                timeout_ms: 1000,
            }],
            events: Vec::new(),
            panes: Vec::new(),
            dependencies: Vec::new(),
            workflows: Vec::new(),
            agents: Vec::new(),
            integrations: Vec::new(),
        }
    }

    #[test]
    fn validates_handler_runtime_and_ids() {
        valid_manifest().validate().unwrap();
        let mut manifest = valid_manifest();
        manifest.runtime = None;
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("runtime")
        );
        let mut manifest = valid_manifest();
        manifest.actions[0].id = "not.local".into();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn version_two_agents_are_strict_bounded_and_version_gated() {
        let source = r#"
manifest_version = 2
[plugin]
id = "com.example.openclaw"
name = "OpenClaw"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "OpenClaw detection"
platforms = ["linux"]
permissions = []
[[agents]]
id = "openclaw"
name = "OpenClaw"
process = { executables = ["openclaw"], argv_contains = ["@openclaw/cli"] }
[[agents.rules]]
id = "approval"
state = "blocked"
priority = 900
region = "bottom_non_empty_lines(12)"
contains = ["approval required"]
"#;
        let manifest: Manifest = toml::from_str(source).unwrap();
        manifest.validate().unwrap();

        let version_one: Manifest =
            toml::from_str(&source.replace("manifest_version = 2", "manifest_version = 1"))
                .unwrap();
        assert!(
            version_one
                .validate()
                .unwrap_err()
                .to_string()
                .contains("version")
        );

        let bad_region: Manifest =
            toml::from_str(&source.replace("bottom_non_empty_lines(12)", "viewport")).unwrap();
        assert!(
            bad_region
                .validate()
                .unwrap_err()
                .to_string()
                .contains("region")
        );

        let bad_regex: Manifest = toml::from_str(&source.replace(
            "contains = [\"approval required\"]",
            "regex = [\"(unterminated\"]",
        ))
        .unwrap();
        assert!(
            bad_regex
                .validate()
                .unwrap_err()
                .to_string()
                .contains("regex")
        );
    }

    #[test]
    fn agent_launch_metadata_is_optional_and_must_be_a_bare_command_name() {
        let source = r#"
manifest_version = 2
[plugin]
id = "com.example.openclaw"
name = "OpenClaw"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "OpenClaw detection"
platforms = ["linux"]
permissions = []
[[agents]]
id = "openclaw"
name = "OpenClaw"
process = { executables = ["openclaw"] }
"#;
        // Absent means detection-only, which is a valid provider rather than an incomplete one.
        let detection_only: Manifest = toml::from_str(source).unwrap();
        detection_only.validate().unwrap();
        assert!(detection_only.agents[0].launch.is_none());

        let launchable: Manifest = toml::from_str(&format!(
            "{source}launch = {{ executable = \"openclaw\" }}\n"
        ))
        .unwrap();
        launchable.validate().unwrap();
        assert_eq!(
            launchable.agents[0].launch.as_ref().unwrap().executable,
            "openclaw"
        );

        // A path would let a manifest choose which binary runs instead of deferring to the user's
        // PATH; whitespace would split into two arguments once the command line is quoted. Both
        // are rejected rather than normalized, because either would silently run something else.
        // TOML literal strings, so a Windows-style separator reaches the validator rather than
        // failing as an escape at parse time.
        for rejected in ["/usr/local/bin/openclaw", r"..\openclaw", "open claw", ""] {
            let manifest: Manifest = toml::from_str(&format!(
                "{source}launch = {{ executable = '{rejected}' }}\n"
            ))
            .unwrap();
            assert!(
                manifest.validate().is_err(),
                "{rejected:?} should be rejected as a launch executable"
            );
        }
    }

    #[test]
    fn component_preopens_are_explicit_unique_capabilities() {
        let mut manifest = valid_manifest();
        manifest.runtime = Some(Runtime {
            kind: RuntimeKind::Component,
            artifact: Some("plugin.wasm".into()),
            command: None,
            activation: Activation::OnDemand,
            preopens: vec![ComponentPreopen::Package, ComponentPreopen::Data],
        });
        manifest.validate().unwrap();

        manifest
            .runtime
            .as_mut()
            .unwrap()
            .preopens
            .push(ComponentPreopen::Data);
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate preopen")
        );

        let mut process = valid_manifest();
        process
            .runtime
            .as_mut()
            .unwrap()
            .preopens
            .push(ComponentPreopen::Config);
        assert!(
            process
                .validate()
                .unwrap_err()
                .to_string()
                .contains("forbids")
        );
    }

    #[test]
    fn rejects_workflow_cycles() {
        let mut manifest = valid_manifest();
        manifest.dependencies.push(Dependency {
            alias: "dep".into(),
            id: "dev.dependency".into(),
            version: VersionReq::STAR,
            source: "https://example.invalid/dep".into(),
        });
        manifest.workflows.push(Workflow {
            id: "cycle".into(),
            title: "cycle".into(),
            trigger: "manual".into(),
            agent_visible: false,
            input_schema: None,
            output_schema: None,
            timeout_ms: 30_000,
            output: Value::Null,
            steps: vec![
                WorkflowStep {
                    id: "a".into(),
                    uses: "dep/run".into(),
                    input: Value::Null,
                    needs: vec!["b".into()],
                },
                WorkflowStep {
                    id: "b".into(),
                    uses: "dep/run".into(),
                    input: Value::Null,
                    needs: vec!["a".into()],
                },
            ],
        });
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
    }

    #[test]
    fn rejects_more_workflows_than_one_plugin_can_retain() {
        let mut manifest = valid_manifest();
        manifest.dependencies.push(Dependency {
            alias: "dep".into(),
            id: "dev.dependency".into(),
            version: VersionReq::STAR,
            source: "https://example.invalid/dep".into(),
        });
        for index in 0..=MAX_WORKFLOWS {
            manifest.workflows.push(Workflow {
                id: format!("workflow-{index}"),
                title: format!("workflow {index}"),
                trigger: "pane.screen_changed".into(),
                agent_visible: false,
                input_schema: None,
                output_schema: None,
                timeout_ms: 30_000,
                output: Value::Null,
                steps: vec![WorkflowStep {
                    id: "run".into(),
                    uses: "dep/run".into(),
                    input: Value::Null,
                    needs: Vec::new(),
                }],
            });
        }
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("128 workflows")
        );
    }

    #[test]
    fn substitutions_add_edges_and_reject_unknown_steps() {
        let mut manifest = valid_manifest();
        manifest.dependencies.push(Dependency {
            alias: "dep".into(),
            id: "dev.dependency".into(),
            version: VersionReq::STAR,
            source: "https://example.invalid/dep".into(),
        });
        manifest.workflows.push(Workflow {
            id: "derived".into(),
            title: "derived".into(),
            trigger: "manual".into(),
            agent_visible: false,
            input_schema: None,
            output_schema: None,
            timeout_ms: 30_000,
            output: Value::String("${steps.second.output#/value}".into()),
            steps: vec![
                WorkflowStep {
                    id: "first".into(),
                    uses: "dep/run".into(),
                    input: Value::Null,
                    needs: Vec::new(),
                },
                WorkflowStep {
                    id: "second".into(),
                    uses: "dep/run".into(),
                    input: Value::String("${steps.first.output}".into()),
                    needs: Vec::new(),
                },
            ],
        });
        manifest.validate().unwrap();
        manifest.workflows[0].steps[1].input = Value::String("${steps.missing.output}".into());
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("unknown step")
        );
    }

    #[test]
    fn loads_and_validates_local_schemas() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("vvmux-plugin.toml"),
            r#"
manifest_version = 1
[plugin]
id = "dev.example"
name = "Example"
version = "1.0.0"
min_vvmux_version = "0.5.0"
description = "test"
platforms = ["linux"]
permissions = ["pane.read"]
[runtime]
kind = "process"
command = ["python", "plugin.py"]
[[actions]]
id = "read"
title = "Read"
description = "read"
handler = "read"
input_schema = "in.json"
output_schema = "out.json"
agent_visible = true
"#,
        )
        .unwrap();
        fs::write(
            directory.path().join("in.json"),
            r#"{"type":"object","additionalProperties":false}"#,
        )
        .unwrap();
        fs::write(directory.path().join("out.json"), r#"{"type":"string"}"#).unwrap();
        let loaded = LoadedManifest::load(directory.path()).unwrap();
        let action = loaded.action("read").unwrap();
        loaded
            .validate_input(action, &serde_json::json!({}))
            .unwrap();
        assert!(loaded.validate_output(action, &Value::Null).is_err());
    }

    #[test]
    fn plugin_panes_default_to_held_and_sync_input_opt_in() {
        let manifest: Manifest = toml::from_str(
            r#"
manifest_version = 1
[plugin]
id = "dev.example"
name = "Example"
version = "1.0.0"
min_vvmux_version = "0.5.0"
description = "test"
platforms = ["linux"]
permissions = ["pane.create"]
[[panes]]
id = "dashboard"
title = "Dashboard"
placement = "float"
command = ["python", "dashboard.py"]
"#,
        )
        .unwrap();
        manifest.validate().unwrap();
        assert!(manifest.panes[0].hold_on_exit);
        assert!(!manifest.panes[0].accept_sync_input);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    const HEADER: &str = r#"manifest_version = 2
[plugin]
id = "dev.vivido.agent.demo"
name = "Demo"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "d"
platforms = ["linux", "macos", "windows"]
permissions = ["integration.write"]
"#;

    fn parse(body: &str) -> Result<Manifest, ManifestError> {
        let manifest: Manifest = toml::from_str(&format!("{HEADER}{body}"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// The four shapes the first-party provider packages actually declare.
    ///
    /// Claude registers one hook per platform because its command differs between a POSIX shell
    /// script and a PowerShell one; the file each names carries the platform filter that decides
    /// which of the two is live.
    #[test]
    fn accepts_every_first_party_integration_shape() {
        let claude = parse(
            r#"
[[integrations]]
id = "claude"
version = 1
config_dir = ".claude"
config_dir_env = "CLAUDE_CONFIG_DIR"
[[integrations.files]]
source = "integration/claude.sh"
dest = "hooks/vvmux-agent-state.sh"
platforms = ["linux", "macos"]
executable = true
[[integrations.files]]
source = "integration/claude.ps1"
dest = "hooks/vvmux-agent-state.ps1"
platforms = ["windows"]
[[integrations.registrations]]
kind = "json-hook"
file = "settings.json"
event = "SessionStart"
matcher = "*"
command_file = "hooks/vvmux-agent-state.sh"
args = ["session"]
[[integrations.registrations]]
kind = "json-hook"
file = "settings.json"
event = "SessionStart"
matcher = "*"
command_file = "hooks/vvmux-agent-state.ps1"
args = ["session"]
"#,
        )
        .unwrap();
        assert_eq!(claude.integrations[0].files.len(), 2);
        assert_eq!(
            claude.integrations[0].config_dir_env.as_deref(),
            Some("CLAUDE_CONFIG_DIR")
        );

        let codex = parse(
            r#"
[[integrations]]
id = "codex"
version = 1
config_dir = ".codex"
config_dir_env = "CODEX_HOME"
[[integrations.files]]
source = "integration/codex.sh"
dest = "vvmux-agent-state.sh"
executable = true
[[integrations.registrations]]
kind = "json-hook"
file = "hooks.json"
event = "SessionStart"
command_file = "vvmux-agent-state.sh"
args = ["session"]
[[integrations.registrations]]
kind = "toml-flag"
file = "config.toml"
section = "features"
key = "hooks"
value = true
"#,
        )
        .unwrap();
        assert!(codex.integrations[0].files[0].platforms.is_empty());
        assert!(matches!(
            codex.integrations[0].registrations[1],
            IntegrationRegistration::TomlFlag { value: true, .. }
        ));

        // No registrations at all: OpenCode discovers plugin files by directory.
        let opencode = parse(
            r#"
[[integrations]]
id = "opencode"
version = 2
config_dir = ".config/opencode"
[[integrations.files]]
source = "integration/opencode.js"
dest = "plugins/vvmux-agent-state.js"
"#,
        )
        .unwrap();
        assert!(opencode.integrations[0].registrations.is_empty());
        assert!(opencode.integrations[0].config_dir_env.is_none());

        let hermes = parse(
            r#"
[[integrations]]
id = "hermes"
version = 1
config_dir = ".hermes"
config_dir_env = "HERMES_HOME"
notice = "manual enable required"
[[integrations.files]]
source = "integration/hermes_plugin.yaml"
dest = "plugins/vvmux-agent-state/plugin.yaml"
[[integrations.files]]
source = "integration/hermes_plugin.py"
dest = "plugins/vvmux-agent-state/__init__.py"
"#,
        )
        .unwrap();
        assert_eq!(
            hermes.integrations[0].notice.as_deref(),
            Some("manual enable required")
        );
    }

    const MINIMAL: &str = r#"
[[integrations]]
id = "demo"
version = 1
config_dir = ".demo"
[[integrations.files]]
source = "integration/demo.sh"
dest = "hooks/demo.sh"
"#;

    #[test]
    fn refuses_shapes_that_would_escape_the_declared_package_or_config_directory() {
        parse(MINIMAL).unwrap();

        // manifest_version 1 predates the table, so an old binary that ignored it would silently
        // install nothing rather than refusing.
        let version_one =
            format!("{HEADER}{MINIMAL}").replace("manifest_version = 2", "manifest_version = 1");
        let manifest: Manifest = toml::from_str(&version_one).unwrap();
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("manifest_version = 2")
        );

        let unpermitted = format!("{HEADER}{MINIMAL}")
            .replace(r#"permissions = ["integration.write"]"#, "permissions = []");
        let manifest: Manifest = toml::from_str(&unpermitted).unwrap();
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("integration.write")
        );

        for (bad, expected) in [
            (
                MINIMAL.replace(r#"config_dir = ".demo""#, r#"config_dir = "/etc""#),
                "config_dir",
            ),
            (
                MINIMAL.replace(r#"config_dir = ".demo""#, r#"config_dir = "../demo""#),
                "config_dir",
            ),
            (
                MINIMAL.replace(r#"config_dir = ".demo""#, r#"config_dir = "a/b/c/d/e""#),
                "segments",
            ),
            (
                MINIMAL.replace(r#"dest = "hooks/demo.sh""#, r#"dest = "../demo.sh""#),
                "dest",
            ),
            (
                MINIMAL.replace(
                    r#"source = "integration/demo.sh""#,
                    r#"source = "/abs/demo.sh""#,
                ),
                "source",
            ),
            (format!("{MINIMAL}platforms = [\"solaris\"]\n"), "platform"),
            (
                format!("{MINIMAL}{}", MINIMAL.trim_start_matches('\n')),
                "duplicate integration id",
            ),
        ] {
            let manifest: Manifest = toml::from_str(&format!("{HEADER}{bad}")).unwrap();
            let error = manifest.validate().unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "expected {expected:?} in {error:?}"
            );
        }
    }

    #[test]
    fn refuses_registrations_that_name_something_the_manifest_never_declared() {
        let undeclared = format!(
            "{MINIMAL}{}",
            r#"[[integrations.registrations]]
kind = "json-hook"
file = "settings.json"
event = "SessionStart"
command_file = "hooks/other.sh"
"#
        );
        let manifest: Manifest = toml::from_str(&format!("{HEADER}{undeclared}")).unwrap();
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("undeclared command file")
        );

        let unknown_kind = format!(
            "{MINIMAL}{}",
            r#"[[integrations.registrations]]
kind = "yaml-merge"
file = "config.yaml"
"#
        );
        assert!(toml::from_str::<Manifest>(&format!("{HEADER}{unknown_kind}")).is_err());

        // An argument is appended to a command line unquoted, so a word with a space in it would
        // become two arguments rather than one.
        let unquotable = format!(
            "{MINIMAL}{}",
            r#"[[integrations.registrations]]
kind = "json-hook"
file = "settings.json"
event = "SessionStart"
command_file = "hooks/demo.sh"
args = ["session; rm -rf /"]
"#
        );
        let manifest: Manifest = toml::from_str(&format!("{HEADER}{unquotable}")).unwrap();
        assert!(
            manifest
                .validate()
                .unwrap_err()
                .to_string()
                .contains("invalid argument")
        );
    }

    #[test]
    fn loading_a_package_bounds_and_confines_every_declared_integration_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("integration")).unwrap();
        fs::write(root.join("vvmux-plugin.toml"), format!("{HEADER}{MINIMAL}")).unwrap();
        fs::write(
            root.join("integration/demo.sh"),
            "# VVMUX_INTEGRATION_ID=demo\n",
        )
        .unwrap();
        let loaded = LoadedManifest::load(root).unwrap();
        assert_eq!(loaded.manifest.integrations[0].files.len(), 1);

        // A symlink is how a package would otherwise reach a file it does not contain.
        #[cfg(unix)]
        {
            fs::remove_file(root.join("integration/demo.sh")).unwrap();
            std::os::unix::fs::symlink("/etc/hostname", root.join("integration/demo.sh")).unwrap();
            assert!(
                LoadedManifest::load(root)
                    .unwrap_err()
                    .to_string()
                    .contains("symlink")
            );
        }

        fs::remove_file(root.join("integration/demo.sh")).unwrap();
        fs::write(
            root.join("integration/demo.sh"),
            [b'x'; MAX_INTEGRATION_FILE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(
            LoadedManifest::load(root)
                .unwrap_err()
                .to_string()
                .contains("1 MiB")
        );
    }

    /// Sorted last so the install prompt's permission list keeps a stable, reviewable order.
    #[test]
    fn integration_write_sorts_after_every_runtime_permission() {
        let mut permissions = [Permission::IntegrationWrite, Permission::SessionRead];
        permissions.sort();
        assert_eq!(permissions.last(), Some(&Permission::IntegrationWrite));
    }
}
