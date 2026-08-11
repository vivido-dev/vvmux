use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use vvmux_plugin_api::{
    Activation, ErrorCode, Event, EventHook, FrameError, Hello, HostCallResult, Invocation,
    InvocationContext, LoadedManifest, NativeMessage, NativeReply, PROTOCOL_VERSION, Permission,
    PluginError, RuntimeKind, read_frame, write_frame,
};

const REGISTRY_SCHEMA: u16 = 1;
const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;
const MAX_ACTION_OUTPUT: usize = 1024 * 1024;
const MAX_LOG_OUTPUT: usize = 256 * 1024;

fn known_plugin_event(name: &str) -> bool {
    matches!(
        name,
        "pane.opened"
            | "pane.exited"
            | "pane.closed"
            | "pane.screen_changed"
            | "layout.changed"
            | "focus.changed"
            | "config.changed"
            | "media.changed"
            | "plugin.job_completed"
            | "plugin.runtime_crashed"
    )
}

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Link a development directory without copying it.
    Link {
        path: PathBuf,
        #[arg(long)]
        yes: bool,
    },
    /// Install a local directory or HTTPS Git repository.
    Install {
        source: String,
        #[arg(long = "ref")]
        git_ref: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Reinstall an installed plugin from its recorded source.
    Update {
        id: String,
        #[arg(long = "ref")]
        git_ref: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Remove a plugin installation.
    Uninstall { id: String },
    /// Enable an installed plugin.
    Enable { id: String },
    /// Disable an installed plugin.
    Disable { id: String },
    /// List installed plugins.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Inspect one plugin and its declared entrypoints.
    Inspect {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Validate the registry, packages, manifests, and schemas.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Show the permissions and runtime trust tier for a plugin.
    Permissions { id: String },
    /// Print agent-visible actions with schemas and provenance.
    Catalog(CatalogArgs),
    /// Invoke one schema-described action.
    Invoke(InvokeArgs),
    /// Inspect or control a retained detached job.
    Job {
        #[command(subcommand)]
        command: PluginJobCommand,
    },
    /// Open a manifest-declared native PTY pane in a live session.
    Pane {
        #[command(subcommand)]
        command: PluginPaneCommand,
    },
    /// Stream bounded, sanitized session plugin events as newline-delimited JSON.
    Events {
        #[arg(long)]
        target: String,
        #[arg(long)]
        after: Option<u64>,
    },
    /// Verify dependency constraints and write the reproducible lock.
    Resolve {
        #[arg(long)]
        frozen: bool,
    },
}

#[derive(Debug, Args)]
pub struct CatalogArgs {
    #[arg(long)]
    target: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct InvokeArgs {
    /// Plugin and action as ID/ACTION.
    reference: String,
    #[arg(long)]
    target: String,
    /// JSON, @FILE, or - for stdin.
    #[arg(long, default_value = "{}")]
    input: String,
    #[arg(long)]
    detach: bool,
}

#[derive(Debug, Subcommand)]
pub enum PluginJobCommand {
    /// Show retained job state and result.
    Status { job_id: String },
    /// Cancel an active job; completed cancellation is idempotent.
    Cancel { job_id: String },
    /// Show bounded retained stdout and stderr.
    Logs { job_id: String },
}

#[derive(Debug, Subcommand)]
pub enum PluginPaneCommand {
    /// Open a pane entrypoint, identified as PLUGIN_ID/PANE_ID.
    Open {
        reference: String,
        #[arg(long)]
        target: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Registry {
    schema: u16,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    plugins: BTreeMap<String, RegistryEntry>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema: REGISTRY_SCHEMA,
            generation: 0,
            plugins: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RegistryEntry {
    id: String,
    version: String,
    root: PathBuf,
    source: String,
    #[serde(default)]
    commit: Option<String>,
    digest: String,
    manifest_digest: String,
    enabled: bool,
    linked: bool,
    runtime_tier: String,
    permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LockFile {
    lock_version: u16,
    packages: Vec<LockedPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LockedPackage {
    id: String,
    version: String,
    source: String,
    commit: Option<String>,
    manifest_digest: String,
    artifact_digest: String,
}

struct PluginPaths {
    root: PathBuf,
    registry: PathBuf,
    packages: PathBuf,
    lock: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuntimePlugin {
    pub(crate) id: String,
    pub(crate) version: String,
    pub(crate) source: String,
    pub(crate) root: PathBuf,
    pub(crate) digest: String,
    pub(crate) manifest_digest: String,
    pub(crate) enabled: bool,
    pub(crate) permissions: Vec<Permission>,
    pub(crate) panes: Vec<vvmux_plugin_api::Pane>,
    pub(crate) activation: Activation,
    pub(crate) events: Vec<EventHook>,
    pub(crate) workflows: Vec<RuntimeWorkflow>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuntimeWorkflow {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) trigger: String,
    pub(crate) agent_visible: bool,
    pub(crate) input_schema: Value,
    pub(crate) output_schema: Value,
    pub(crate) timeout_ms: u64,
    pub(crate) output: Value,
    pub(crate) steps: Vec<RuntimeWorkflowStep>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuntimeWorkflowStep {
    pub(crate) id: String,
    pub(crate) reference: String,
    pub(crate) input: Value,
    pub(crate) needs: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct RegistryCandidate {
    pub(crate) generation: u64,
    pub(crate) plugins: BTreeMap<String, RuntimePlugin>,
    /// Agent-visible actions grouped by plugin so draining runtimes can be omitted atomically.
    pub(crate) catalog: BTreeMap<String, Vec<Value>>,
    pub(crate) failed: BTreeMap<String, String>,
}

pub fn run(command: PluginCommand) -> io::Result<()> {
    let paths = PluginPaths::new()?;
    match command {
        PluginCommand::Link { path, yes } => install_local(&paths, &path, true, yes, None, None),
        PluginCommand::Install {
            source,
            git_ref,
            yes,
        } => {
            if source.starts_with("https://") {
                install_git(&paths, &source, git_ref.as_deref(), yes)
            } else {
                install_local(&paths, Path::new(&source), false, yes, None, None)
            }
        }
        PluginCommand::Update { id, git_ref, yes } => update(&paths, &id, git_ref.as_deref(), yes),
        PluginCommand::Uninstall { id } => uninstall(&paths, &id),
        PluginCommand::Enable { id } => set_enabled(&paths, &id, true),
        PluginCommand::Disable { id } => set_enabled(&paths, &id, false),
        PluginCommand::List { json } => list(&paths, json),
        PluginCommand::Inspect { id, json } => inspect(&paths, &id, json),
        PluginCommand::Doctor { json } => doctor(&paths, json),
        PluginCommand::Permissions { id } => permissions(&paths, &id),
        PluginCommand::Catalog(args) => catalog(&paths, args),
        PluginCommand::Invoke(args) => invoke(&paths, args),
        PluginCommand::Job { command } => job(command),
        PluginCommand::Pane { command } => pane(command),
        PluginCommand::Events { target, after } => events(&target, after),
        PluginCommand::Resolve { frozen } => resolve(&paths, frozen),
    }
}

impl PluginPaths {
    fn new() -> io::Result<Self> {
        let root = crate::config::config_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no user config directory"))?
            .join("plugins");
        Ok(Self {
            registry: root.join("registry.json"),
            packages: root.join("packages"),
            lock: root.join("vvmux-plugin.lock"),
            root,
        })
    }

    fn ensure(&self) -> io::Result<()> {
        create_private_dir(&self.root)?;
        create_private_dir(&self.packages)
    }

    fn package(&self, id: &str) -> PathBuf {
        let digest = Sha256::digest(id.as_bytes());
        self.packages.join(format!("p-{}", hex(&digest[..16])))
    }

    fn package_version(&self, id: &str, artifact_digest: &str) -> PathBuf {
        self.packages.join(format!(
            "{}-{}",
            self.package(id).file_name().unwrap().to_string_lossy(),
            &artifact_digest[..16]
        ))
    }
}

pub(crate) fn registry_path() -> io::Result<PathBuf> {
    Ok(PluginPaths::new()?.registry)
}

pub(crate) fn load_registry_candidate() -> io::Result<RegistryCandidate> {
    let paths = PluginPaths::new()?;
    let registry = load_registry(&paths)?;
    registry_candidate(&registry)
}

fn registry_candidate(registry: &Registry) -> io::Result<RegistryCandidate> {
    let mut plugins = BTreeMap::new();
    let mut catalog = BTreeMap::new();
    let mut failed = BTreeMap::new();
    for entry in registry.plugins.values() {
        if !entry.enabled {
            plugins.insert(
                entry.id.clone(),
                RuntimePlugin {
                    id: entry.id.clone(),
                    version: entry.version.clone(),
                    source: entry.source.clone(),
                    root: entry.root.clone(),
                    digest: entry.digest.clone(),
                    manifest_digest: entry.manifest_digest.clone(),
                    enabled: false,
                    permissions: Vec::new(),
                    panes: Vec::new(),
                    activation: Activation::OnDemand,
                    events: Vec::new(),
                    workflows: Vec::new(),
                },
            );
            continue;
        }
        let validated = (|| {
            let loaded = load_package(&entry.root)?;
            if loaded.manifest.plugin.id != entry.id {
                return Err(invalid("manifest ID differs from registry ID"));
            }
            let actual_digest = digest_tree(&entry.root)?;
            if !entry.linked && actual_digest != entry.digest {
                return Err(invalid("installed package digest differs from registry"));
            }
            let actual_manifest = digest_file(&entry.root.join("vvmux-plugin.toml"))?;
            if !entry.linked && actual_manifest != entry.manifest_digest {
                return Err(invalid("installed manifest digest differs from registry"));
            }
            let enforceable = crate::session::plugin_enforceable_capabilities()
                .into_iter()
                .collect::<BTreeSet<_>>();
            let effective_permissions = entry
                .permissions
                .iter()
                .filter(|permission| enforceable.contains(*permission))
                .cloned()
                .collect::<Vec<_>>();
            let enforceable_permissions = crate::session::plugin_enforceable_permissions()
                .into_iter()
                .collect::<BTreeSet<_>>();
            let runtime_permissions = loaded
                .manifest
                .plugin
                .permissions
                .iter()
                .filter(|permission| enforceable_permissions.contains(*permission))
                .copied()
                .collect::<Vec<_>>();
            let actions = loaded
                .manifest
                .actions
                .iter()
                .filter(|action| action.agent_visible)
                .map(|action| {
                    serde_json::json!({
                        "reference": format!("{}/{}", entry.id, action.id),
                        "title": action.title,
                        "description": action.description,
                        "input_schema": loaded.schemas[&action.input_schema].value,
                        "output_schema": loaded.schemas[&action.output_schema].value,
                        "permissions": effective_permissions,
                        "declared_permissions": entry.permissions,
                        "runtime_tier": entry.runtime_tier,
                        "source": entry.source,
                        "digest": actual_digest,
                        "manifest_digest": actual_manifest,
                        "timeout_ms": action.timeout_ms,
                    })
                })
                .collect::<Vec<_>>();
            Ok((
                RuntimePlugin {
                    id: entry.id.clone(),
                    version: entry.version.clone(),
                    source: entry.source.clone(),
                    root: entry.root.clone(),
                    digest: actual_digest,
                    manifest_digest: actual_manifest,
                    enabled: true,
                    permissions: runtime_permissions,
                    panes: loaded.manifest.panes.clone(),
                    activation: loaded
                        .manifest
                        .runtime
                        .as_ref()
                        .map_or(Activation::OnDemand, |runtime| runtime.activation),
                    events: loaded
                        .manifest
                        .events
                        .iter()
                        .filter(|hook| known_plugin_event(&hook.on))
                        .cloned()
                        .collect(),
                    workflows: Vec::new(),
                },
                actions,
            ))
        })();
        match validated {
            Ok((plugin, actions)) => {
                plugins.insert(entry.id.clone(), plugin);
                catalog.insert(entry.id.clone(), actions);
            }
            Err(error) => {
                failed.insert(entry.id.clone(), error.to_string());
            }
        }
    }
    compile_registry_workflows(&mut plugins, &mut catalog, &mut failed);
    Ok(RegistryCandidate {
        generation: registry.generation,
        plugins,
        catalog,
        failed,
    })
}

fn compile_registry_workflows(
    plugins: &mut BTreeMap<String, RuntimePlugin>,
    catalog: &mut BTreeMap<String, Vec<Value>>,
    failed: &mut BTreeMap<String, String>,
) {
    let ids = plugins.keys().cloned().collect::<Vec<_>>();
    for id in ids {
        let compiled = (|| -> io::Result<Vec<RuntimeWorkflow>> {
            let plugin = plugins
                .get(&id)
                .ok_or_else(|| invalid("plugin disappeared while workflows were compiled"))?;
            if !plugin.enabled {
                return Ok(Vec::new());
            }
            let loaded = load_package(&plugin.root)?;
            if loaded
                .manifest
                .workflows
                .iter()
                .all(|workflow| workflow.steps.is_empty())
            {
                return Ok(Vec::new());
            }
            if !plugin.permissions.contains(&Permission::PluginInvoke) {
                return Err(invalid(format!(
                    "capability_denied: plugin `{id}` declares workflows without `plugin.invoke`"
                )));
            }
            let aliases = loaded
                .manifest
                .dependencies
                .iter()
                .map(|dependency| (dependency.alias.as_str(), dependency.id.as_str()))
                .collect::<BTreeMap<_, _>>();
            let mut workflows = Vec::new();
            for workflow in &loaded.manifest.workflows {
                if workflow.steps.is_empty() {
                    continue;
                }
                if workflow.trigger != "manual" && !known_plugin_event(&workflow.trigger) {
                    return Err(invalid(format!(
                        "workflow `{}` has unknown trigger `{}`",
                        workflow.id, workflow.trigger
                    )));
                }
                if workflow.trigger != "manual"
                    && !plugin.permissions.contains(&Permission::EventsSubscribe)
                {
                    return Err(invalid(format!(
                        "capability_denied: event workflow `{}` requires `events.subscribe`",
                        workflow.id
                    )));
                }
                let input_schema = workflow.input_schema.as_ref().map_or_else(
                    || serde_json::json!({}),
                    |path| loaded.schemas[path].value.clone(),
                );
                let output_schema = workflow.output_schema.as_ref().map_or_else(
                    || serde_json::json!({}),
                    |path| loaded.schemas[path].value.clone(),
                );
                let mut steps = Vec::new();
                let mut step_output_schemas = BTreeMap::new();
                let mut step_input_schemas = BTreeMap::new();
                for step in &workflow.steps {
                    let (alias, action_id) = step.uses.split_once('/').unwrap();
                    let dependency_id = aliases[alias];
                    let dependency = plugins.get(dependency_id).ok_or_else(|| {
                        invalid(format!(
                            "dependency_failed: workflow `{}` requires `{dependency_id}`",
                            workflow.id
                        ))
                    })?;
                    if !dependency.enabled {
                        return Err(invalid(format!(
                            "dependency_failed: workflow `{}` dependency `{dependency_id}` is disabled",
                            workflow.id
                        )));
                    }
                    let dependency_package = load_package(&dependency.root)?;
                    let action = dependency_package.action(action_id).ok_or_else(|| {
                        invalid(format!(
                            "action_not_found: workflow `{}` step `{}` uses {dependency_id}/{action_id}",
                            workflow.id, step.id
                        ))
                    })?;
                    step_output_schemas.insert(
                        step.id.clone(),
                        dependency_package.schemas[&action.output_schema]
                            .value
                            .clone(),
                    );
                    step_input_schemas.insert(
                        step.id.clone(),
                        dependency_package.schemas[&action.input_schema]
                            .value
                            .clone(),
                    );
                }
                for step in &workflow.steps {
                    validate_workflow_template(
                        &step.input,
                        &input_schema,
                        &step_output_schemas,
                        &workflow.id,
                    )?;
                    validate_workflow_schema_links(
                        &step.input,
                        &step_input_schemas[&step.id],
                        &input_schema,
                        &step_output_schemas,
                        &workflow.id,
                    )?;
                    if !contains_workflow_substitution(&step.input) {
                        validate_schema_value(
                            &step_input_schemas[&step.id],
                            &step.input,
                            "schema_invalid",
                        )?;
                    }
                    let mut needs = step.needs.clone();
                    for reference in workflow_template_step_references(&step.input)? {
                        if !needs.contains(&reference) {
                            needs.push(reference);
                        }
                    }
                    steps.push(RuntimeWorkflowStep {
                        id: step.id.clone(),
                        reference: {
                            let (alias, action) = step.uses.split_once('/').unwrap();
                            format!("{}/{}", aliases[alias], action)
                        },
                        input: step.input.clone(),
                        needs,
                    });
                }
                validate_workflow_template(
                    &workflow.output,
                    &input_schema,
                    &step_output_schemas,
                    &workflow.id,
                )?;
                validate_workflow_schema_links(
                    &workflow.output,
                    &output_schema,
                    &input_schema,
                    &step_output_schemas,
                    &workflow.id,
                )?;
                if !contains_workflow_substitution(&workflow.output) {
                    validate_schema_value(&output_schema, &workflow.output, "output_invalid")?;
                }
                workflows.push(RuntimeWorkflow {
                    id: workflow.id.clone(),
                    title: workflow.title.clone(),
                    trigger: workflow.trigger.clone(),
                    agent_visible: workflow.agent_visible,
                    input_schema,
                    output_schema,
                    timeout_ms: workflow.timeout_ms,
                    output: workflow.output.clone(),
                    steps,
                });
            }
            Ok(workflows)
        })();
        match compiled {
            Ok(workflows) => {
                let Some(plugin) = plugins.get_mut(&id) else {
                    continue;
                };
                for workflow in workflows.iter().filter(|workflow| workflow.agent_visible) {
                    catalog
                        .entry(id.clone())
                        .or_default()
                        .push(serde_json::json!({
                            "reference": format!("{}/{}", id, workflow.id),
                            "title": workflow.title,
                            "description": format!("Workflow: {}", workflow.title),
                            "input_schema": workflow.input_schema,
                            "output_schema": workflow.output_schema,
                            "permissions": plugin.permissions,
                            "declared_permissions": plugin.permissions,
                            "runtime_tier": "workflow",
                            "source": plugin.source,
                            "digest": plugin.digest,
                            "manifest_digest": plugin.manifest_digest,
                            "timeout_ms": workflow.timeout_ms,
                        }));
                }
                plugin.workflows = workflows;
            }
            Err(error) => {
                plugins.remove(&id);
                catalog.remove(&id);
                failed.insert(id, error.to_string());
            }
        }
    }
}

fn validate_schema_value(schema: &Value, value: &Value, code: &str) -> io::Result<()> {
    vvmux_plugin_api::validate_schema_instance(schema, value)
        .map_err(|errors| invalid(format!("{code}: {}", errors.join("; "))))
}

fn contains_workflow_substitution(value: &Value) -> bool {
    match value {
        Value::String(value) => value.starts_with("${") && value.ends_with('}'),
        Value::Array(values) => values.iter().any(contains_workflow_substitution),
        Value::Object(values) => values.values().any(contains_workflow_substitution),
        _ => false,
    }
}

fn workflow_template_step_references(value: &Value) -> io::Result<Vec<String>> {
    let mut references = Vec::new();
    visit_workflow_substitutions(value, &mut |root, _| {
        if let Some(step) = root
            .strip_prefix("steps.")
            .and_then(|root| root.strip_suffix(".output"))
        {
            references.push(step.to_owned());
        }
        Ok(())
    })?;
    Ok(references)
}

fn validate_workflow_template(
    value: &Value,
    trigger_schema: &Value,
    step_schemas: &BTreeMap<String, Value>,
    workflow: &str,
) -> io::Result<()> {
    visit_workflow_substitutions(value, &mut |root, pointer| {
        let schema = if root == "trigger" {
            trigger_schema
        } else if let Some(step) = root
            .strip_prefix("steps.")
            .and_then(|root| root.strip_suffix(".output"))
        {
            step_schemas.get(step).ok_or_else(|| {
                invalid(format!(
                    "workflow `{workflow}` references unknown step `{step}`"
                ))
            })?
        } else {
            return Err(invalid(format!(
                "workflow `{workflow}` contains an unknown substitution root"
            )));
        };
        if let Some(pointer) = pointer {
            ensure_schema_pointer(schema, pointer).map_err(|error| {
                invalid(format!(
                    "workflow `{workflow}` substitution {root}#{pointer} is invalid: {error}"
                ))
            })?;
        }
        Ok(())
    })
}

fn visit_workflow_substitutions(
    value: &Value,
    visitor: &mut impl FnMut(&str, Option<&str>) -> io::Result<()>,
) -> io::Result<()> {
    match value {
        Value::String(value) if value.contains("${") => {
            let inner = value
                .strip_prefix("${")
                .and_then(|value| value.strip_suffix('}'))
                .ok_or_else(|| invalid("ambiguous workflow substitution"))?;
            let (root, pointer) = inner
                .split_once('#')
                .map_or((inner, None), |(root, pointer)| (root, Some(pointer)));
            if pointer.is_some_and(|pointer| !pointer.starts_with('/')) {
                return Err(invalid("workflow substitution has an invalid JSON Pointer"));
            }
            visitor(root, pointer)?;
        }
        Value::Array(values) => {
            for value in values {
                visit_workflow_substitutions(value, visitor)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                visit_workflow_substitutions(value, visitor)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn ensure_schema_pointer(schema: &Value, pointer: &str) -> io::Result<()> {
    schema_at_pointer(schema, pointer).map(|_| ())
}

fn schema_at_pointer<'a>(schema: &'a Value, pointer: &str) -> io::Result<&'a Value> {
    let mut current = dereference_schema(schema, schema)?;
    for component in pointer.split('/').skip(1) {
        let component = component.replace("~1", "/").replace("~0", "~");
        if current.as_object().is_none_or(|schema| schema.is_empty()) {
            return Ok(current);
        }
        current = current
            .get("properties")
            .and_then(|properties| properties.get(&component))
            .or_else(|| current.get("items"))
            .ok_or_else(|| invalid(format!("schema has no path component `{component}`")))?;
        current = dereference_schema(schema, current)?;
    }
    Ok(current)
}

fn dereference_schema<'a>(document: &'a Value, mut schema: &'a Value) -> io::Result<&'a Value> {
    for _ in 0..32 {
        let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
            return Ok(schema);
        };
        let Some(pointer) = reference.strip_prefix('#') else {
            return Err(invalid("workflow schema contains a remote reference"));
        };
        schema = document
            .pointer(pointer)
            .ok_or_else(|| invalid(format!("schema reference `{reference}` does not exist")))?;
    }
    Err(invalid("workflow schema references exceed depth 32"))
}

fn validate_workflow_schema_links(
    template: &Value,
    target_schema: &Value,
    trigger_schema: &Value,
    step_schemas: &BTreeMap<String, Value>,
    workflow: &str,
) -> io::Result<()> {
    fn visit(
        template: &Value,
        target_document: &Value,
        target: &Value,
        trigger_schema: &Value,
        step_schemas: &BTreeMap<String, Value>,
        workflow: &str,
    ) -> io::Result<()> {
        let target = dereference_schema(target_document, target)?;
        if let Value::String(value) = template
            && let Some(inner) = value
                .strip_prefix("${")
                .and_then(|value| value.strip_suffix('}'))
        {
            let (root, pointer) = inner
                .split_once('#')
                .map_or((inner, ""), |(root, pointer)| (root, pointer));
            let source_document = if root == "trigger" {
                trigger_schema
            } else if let Some(step) = root
                .strip_prefix("steps.")
                .and_then(|root| root.strip_suffix(".output"))
            {
                step_schemas.get(step).ok_or_else(|| {
                    invalid(format!(
                        "workflow `{workflow}` references unknown step `{step}`"
                    ))
                })?
            } else {
                return Err(invalid(format!(
                    "workflow `{workflow}` contains an unknown substitution root"
                )));
            };
            let source = schema_at_pointer(source_document, pointer)?;
            if !schema_output_fits(source_document, source, target_document, target)? {
                return Err(invalid(format!(
                    "schema_invalid: workflow `{workflow}` substitution `{inner}` is incompatible with its destination"
                )));
            }
            return Ok(());
        }
        match template {
            Value::Object(values) => {
                for (key, value) in values {
                    let child = target
                        .get("properties")
                        .and_then(|properties| properties.get(key))
                        .or_else(|| {
                            target
                                .get("additionalProperties")
                                .filter(|additional| additional.is_object())
                        });
                    if target.get("additionalProperties") == Some(&Value::Bool(false))
                        && child.is_none()
                    {
                        return Err(invalid(format!(
                            "schema_invalid: workflow `{workflow}` supplies unknown property `{key}`"
                        )));
                    }
                    if let Some(child) = child {
                        visit(
                            value,
                            target_document,
                            child,
                            trigger_schema,
                            step_schemas,
                            workflow,
                        )?;
                    }
                }
            }
            Value::Array(values) => {
                if let Some(items) = target.get("items") {
                    for value in values {
                        visit(
                            value,
                            target_document,
                            items,
                            trigger_schema,
                            step_schemas,
                            workflow,
                        )?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    visit(
        template,
        target_schema,
        target_schema,
        trigger_schema,
        step_schemas,
        workflow,
    )
}

fn schema_output_fits(
    source_document: &Value,
    source: &Value,
    target_document: &Value,
    target: &Value,
) -> io::Result<bool> {
    let source = dereference_schema(source_document, source)?;
    let target = dereference_schema(target_document, target)?;
    let source_types = schema_types(source);
    let target_types = schema_types(target);
    if target_types.is_empty() {
        return Ok(true);
    }
    if source_types.is_empty() {
        return Ok(false);
    }
    if !source_types.iter().all(|source_type| {
        target_types.contains(source_type)
            || (*source_type == "integer" && target_types.contains("number"))
    }) {
        return Ok(false);
    }
    if source_types.contains("object") && target_types.contains("object") {
        let source_properties = source.get("properties").and_then(Value::as_object);
        let target_properties = target.get("properties").and_then(Value::as_object);
        let source_required = source
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<BTreeSet<_>>();
        for required in target
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !source_required.contains(required) {
                return Ok(false);
            }
            let Some(source_property) = source_properties.and_then(|values| values.get(required))
            else {
                return Ok(false);
            };
            let Some(target_property) = target_properties.and_then(|values| values.get(required))
            else {
                continue;
            };
            if !schema_output_fits(
                source_document,
                source_property,
                target_document,
                target_property,
            )? {
                return Ok(false);
            }
        }
        if target.get("additionalProperties") == Some(&Value::Bool(false)) {
            if source.get("additionalProperties") != Some(&Value::Bool(false)) {
                return Ok(false);
            }
            if source_properties.is_some_and(|properties| {
                properties
                    .keys()
                    .any(|key| target_properties.is_none_or(|target| !target.contains_key(key)))
            }) {
                return Ok(false);
            }
        }
    }
    if source_types.contains("array")
        && target_types.contains("array")
        && let Some(target_items) = target.get("items")
    {
        let Some(source_items) = source.get("items") else {
            return Ok(false);
        };
        if !schema_output_fits(source_document, source_items, target_document, target_items)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn schema_types(schema: &Value) -> BTreeSet<&str> {
    match schema.get("type") {
        Some(Value::String(value)) => [value.as_str()].into_iter().collect(),
        Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
        _ => {
            if let Some(value) = schema.get("const") {
                return [json_schema_type(value)].into_iter().collect();
            }
            if let Some(values) = schema.get("enum").and_then(Value::as_array) {
                return values.iter().map(json_schema_type).collect();
            }
            if schema.get("properties").is_some() || schema.get("required").is_some() {
                return ["object"].into_iter().collect();
            }
            if schema.get("items").is_some() {
                return ["array"].into_iter().collect();
            }
            for keyword in ["oneOf", "anyOf"] {
                if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
                    let mut types = BTreeSet::new();
                    for branch in branches {
                        let branch_types = schema_types(branch);
                        if branch_types.is_empty() {
                            return BTreeSet::new();
                        }
                        types.extend(branch_types);
                    }
                    return types;
                }
            }
            BTreeSet::new()
        }
    }
}

fn json_schema_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub(crate) fn resolve_workflow_template(
    value: &Value,
    trigger: &Value,
    outputs: &BTreeMap<String, Value>,
) -> io::Result<Value> {
    match value {
        Value::String(value) if value.contains("${") => {
            let inner = value
                .strip_prefix("${")
                .and_then(|value| value.strip_suffix('}'))
                .ok_or_else(|| invalid("ambiguous workflow substitution"))?;
            let (root, pointer) = inner
                .split_once('#')
                .map_or((inner, None), |(root, pointer)| (root, Some(pointer)));
            let source = if root == "trigger" {
                trigger
            } else if let Some(step) = root
                .strip_prefix("steps.")
                .and_then(|root| root.strip_suffix(".output"))
            {
                outputs.get(step).ok_or_else(|| {
                    invalid(format!("dependency_failed: step `{step}` has no output"))
                })?
            } else {
                return Err(invalid("dependency_failed: unknown substitution root"));
            };
            pointer.map_or_else(
                || Ok(source.clone()),
                |pointer| {
                    source.pointer(pointer).cloned().ok_or_else(|| {
                        invalid(format!(
                            "dependency_failed: substitution pointer `{pointer}` does not exist"
                        ))
                    })
                },
            )
        }
        Value::Array(values) => values
            .iter()
            .map(|value| resolve_workflow_template(value, trigger, outputs))
            .collect::<io::Result<Vec<_>>>()
            .map(Value::Array),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| {
                resolve_workflow_template(value, trigger, outputs).map(|value| (key.clone(), value))
            })
            .collect::<io::Result<serde_json::Map<_, _>>>()
            .map(Value::Object),
        _ => Ok(value.clone()),
    }
}

fn load_registry(paths: &PluginPaths) -> io::Result<Registry> {
    match fs::metadata(&paths.registry) {
        Ok(metadata) if metadata.len() > MAX_REGISTRY_BYTES => {
            return Err(invalid("plugin registry exceeds 1 MiB"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
        Err(error) => return Err(error),
    }
    let registry: Registry = serde_json::from_slice(&fs::read(&paths.registry)?)
        .map_err(|error| invalid(format!("invalid plugin registry: {error}")))?;
    if registry.schema != REGISTRY_SCHEMA {
        return Err(invalid("unsupported plugin registry schema"));
    }
    for (id, entry) in &registry.plugins {
        if id != &entry.id || !safe_registry_root(&paths.packages, &entry.root, entry.linked) {
            return Err(invalid(format!("unsafe registry entry `{id}`")));
        }
    }
    Ok(registry)
}

fn save_registry(paths: &PluginPaths, registry: &mut Registry) -> io::Result<()> {
    paths.ensure()?;
    registry.generation = registry
        .generation
        .checked_add(1)
        .ok_or_else(|| invalid("plugin registry generation exhausted"))?;
    let bytes = serde_json::to_vec_pretty(registry).map_err(io::Error::other)?;
    let temporary = paths
        .registry
        .with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = private_new_file(&temporary)?;
    let result = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    if result.is_ok() {
        crate::runtime::atomic_replace(&temporary, &paths.registry)?;
    } else {
        let _ = fs::remove_file(&temporary);
        result?;
    }
    Ok(())
}

fn commit_registry(
    paths: &PluginPaths,
    previous: &Registry,
    next: &mut Registry,
) -> io::Result<()> {
    commit_registry_with_reload(paths, previous, next, reload_live_sessions)
}

fn commit_registry_with_reload(
    paths: &PluginPaths,
    previous: &Registry,
    next: &mut Registry,
    mut reload: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let current = load_registry(paths)?;
    if current.generation != previous.generation || current.plugins != previous.plugins {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "plugin registry changed concurrently; retry the operation",
        ));
    }
    paths.ensure()?;
    let previous_lock = match fs::read(&paths.lock) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let next_lock = encode_lock(next)?;
    write_private_atomic(&paths.lock, next_lock.as_bytes())?;
    if let Err(error) = save_registry(paths, next) {
        restore_lock(paths, previous_lock.as_deref())?;
        return Err(error);
    }
    if let Err(apply_error) = reload() {
        let mut rollback = previous.clone();
        rollback.generation = next.generation;
        save_registry(paths, &mut rollback)?;
        restore_lock(paths, previous_lock.as_deref())?;
        if let Err(rollback_error) = reload() {
            return Err(io::Error::other(format!(
                "live plugin reload failed ({apply_error}); rollback generation {} was published but session acknowledgement failed: {rollback_error}",
                rollback.generation
            )));
        }
        return Err(io::Error::other(format!(
            "live plugin reload failed; registry rolled back at generation {}: {apply_error}",
            rollback.generation
        )));
    }
    Ok(())
}

fn restore_lock(paths: &PluginPaths, contents: Option<&[u8]>) -> io::Result<()> {
    match contents {
        Some(contents) => write_private_atomic(&paths.lock, contents),
        None => match fs::remove_file(&paths.lock) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

#[cfg(not(test))]
fn reload_live_sessions() -> io::Result<()> {
    let mut failures = Vec::new();
    for session in crate::runtime::list_registries()? {
        match plugin_session_request(&session.name, crate::ipc::PluginMethod::Reload) {
            Ok(report) => {
                if let Some(failed) = report["failed"].as_object()
                    && !failed.is_empty()
                {
                    failures.push(format!(
                        "{} rejected entries: {}",
                        session.name,
                        serde_json::to_string(failed).unwrap_or_else(|_| "invalid report".into())
                    ));
                }
            }
            Err(error) => failures.push(format!("{}: {error}", session.name)),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(failures.join("; ")))
    }
}

#[cfg(test)]
fn reload_live_sessions() -> io::Result<()> {
    Ok(())
}

fn install_local(
    paths: &PluginPaths,
    source: &Path,
    linked: bool,
    yes: bool,
    recorded_source: Option<String>,
    recorded_commit: Option<String>,
) -> io::Result<()> {
    let source = fs::canonicalize(source)?;
    paths.ensure()?;
    let previous_registry = load_registry(paths)?;
    let loaded = load_package(&source)?;
    ensure_current_platform(&loaded)?;
    let id = loaded.manifest.plugin.id.clone();
    let mut pending = BTreeMap::new();
    pending.insert(
        id.clone(),
        PendingPackage {
            id: id.clone(),
            source_dir: source.clone(),
            source: recorded_source.unwrap_or_else(|| source.to_string_lossy().into_owned()),
            commit: recorded_commit,
            linked,
            approved_manifest: digest_file(&source.join("vvmux-plugin.toml"))?,
        },
    );
    let mut constraints = BTreeMap::new();
    let mut temporary = Vec::new();
    let acquisition = collect_dependency_sources(
        paths,
        &id,
        &previous_registry,
        &mut pending,
        &mut constraints,
        &mut temporary,
        &mut BTreeSet::new(),
        1,
    );
    if let Err(error) = acquisition {
        cleanup_temporary(paths, &temporary);
        return Err(error);
    }
    for package in pending.values() {
        let loaded = load_package(&package.source_dir)?;
        preview(
            &loaded,
            if package.linked {
                "linked native/user package"
            } else {
                &package.source
            },
        );
    }
    for package in pending.values() {
        let loaded = load_package(&package.source_dir)?;
        let delta = approval_delta(
            previous_registry.plugins.get(&package.id),
            &loaded,
            &package.source,
        );
        print_approval_delta(&delta);
        confirm_if_needed(&loaded, &delta, yes)?;
    }
    let result = commit_package_graph(paths, &previous_registry, &pending);
    cleanup_temporary(paths, &temporary);
    result?;
    println!(
        "{} plugin {id}",
        if linked { "linked" } else { "installed" }
    );
    Ok(())
}

struct PendingPackage {
    id: String,
    source_dir: PathBuf,
    source: String,
    commit: Option<String>,
    linked: bool,
    approved_manifest: String,
}

fn ensure_current_platform(loaded: &LoadedManifest) -> io::Result<()> {
    if loaded
        .manifest
        .plugin
        .platforms
        .iter()
        .any(|platform| platform == current_platform())
    {
        Ok(())
    } else {
        Err(invalid(format!(
            "plugin `{}` does not support {}",
            loaded.manifest.plugin.id,
            current_platform()
        )))
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_dependency_sources(
    paths: &PluginPaths,
    id: &str,
    registry: &Registry,
    pending: &mut BTreeMap<String, PendingPackage>,
    constraints: &mut BTreeMap<String, (String, Vec<semver::VersionReq>)>,
    temporary: &mut Vec<PathBuf>,
    visiting: &mut BTreeSet<String>,
    depth: usize,
) -> io::Result<()> {
    if depth > 8 {
        return Err(invalid("dependency graph exceeds depth 8"));
    }
    let package_count = registry
        .plugins
        .keys()
        .chain(pending.keys())
        .collect::<BTreeSet<_>>()
        .len();
    if package_count > 64 {
        return Err(invalid("dependency graph exceeds 64 packages"));
    }
    if !visiting.insert(id.to_owned()) {
        return Err(invalid(format!("dependency cycle at `{id}`")));
    }
    let root = pending
        .get(id)
        .map(|package| package.source_dir.as_path())
        .or_else(|| registry.plugins.get(id).map(|entry| entry.root.as_path()))
        .ok_or_else(|| invalid(format!("dependency_failed: package `{id}` is unavailable")))?;
    let loaded = load_package(root)?;
    ensure_current_platform(&loaded)?;
    for dependency in &loaded.manifest.dependencies {
        if visiting.contains(&dependency.id) {
            return Err(invalid(format!("dependency cycle at `{}`", dependency.id)));
        }
        let constraint = constraints
            .entry(dependency.id.clone())
            .or_insert_with(|| (dependency.source.clone(), Vec::new()));
        if constraint.0 != dependency.source {
            return Err(invalid(format!(
                "dependency_failed: conflicting sources for {}",
                dependency.id
            )));
        }
        constraint.1.push(dependency.version.clone());
        let usable_pending = pending.get(&dependency.id).is_some_and(|package| {
            package.source == dependency.source
                && load_package(&package.source_dir).is_ok_and(|loaded| {
                    constraint
                        .1
                        .iter()
                        .all(|required| required.matches(&loaded.manifest.plugin.version))
                })
        });
        let usable_installed = registry.plugins.get(&dependency.id).is_some_and(|entry| {
            entry.source == dependency.source
                && load_package(&entry.root).is_ok_and(|loaded| {
                    constraint
                        .1
                        .iter()
                        .all(|required| required.matches(&loaded.manifest.plugin.version))
                })
        });
        if pending.contains_key(&dependency.id) && !usable_pending {
            return Err(invalid(format!(
                "dependency_failed: {} does not satisfy all version constraints",
                dependency.id
            )));
        }
        if !usable_pending && !usable_installed {
            let (checkout, commit) = clone_dependency(paths, &dependency.source, &dependency.id)?;
            temporary.push(checkout.clone());
            let dependency_package = load_package(&checkout)?;
            if dependency_package.manifest.plugin.id != dependency.id {
                return Err(invalid(format!(
                    "dependency_failed: source for {} contains {}",
                    dependency.id, dependency_package.manifest.plugin.id
                )));
            }
            if !constraint
                .1
                .iter()
                .all(|required| required.matches(&dependency_package.manifest.plugin.version))
            {
                return Err(invalid(format!(
                    "dependency_failed: {} does not satisfy all version constraints",
                    dependency.id
                )));
            }
            ensure_current_platform(&dependency_package)?;
            pending.insert(
                dependency.id.clone(),
                PendingPackage {
                    id: dependency.id.clone(),
                    source_dir: checkout.clone(),
                    source: dependency.source.clone(),
                    commit: Some(commit),
                    linked: false,
                    approved_manifest: digest_file(&checkout.join("vvmux-plugin.toml"))?,
                },
            );
        }
        collect_dependency_sources(
            paths,
            &dependency.id,
            registry,
            pending,
            constraints,
            temporary,
            visiting,
            depth + 1,
        )?;
    }
    visiting.remove(id);
    Ok(())
}

fn clone_dependency(paths: &PluginPaths, source: &str, id: &str) -> io::Result<(PathBuf, String)> {
    if !source.starts_with("https://") {
        return Err(invalid("dependency sources must use HTTPS"));
    }
    let identity = hex(&Sha256::digest(format!("{id}\0{source}").as_bytes())[..12]);
    let checkout = paths
        .packages
        .join(format!(".dep-{}-{identity}", std::process::id()));
    if checkout.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "dependency staging path exists",
        ));
    }
    let status = Command::new("git")
        .args(["clone", "--quiet", "--depth", "1", source])
        .arg(&checkout)
        .status()?;
    if !status.success() {
        let _ = remove_known_tree(&paths.packages, &checkout);
        return Err(io::Error::other(format!(
            "dependency_failed: could not clone {id}"
        )));
    }
    let output = Command::new("git")
        .args([
            "-C",
            checkout.to_str().unwrap_or_default(),
            "rev-parse",
            "HEAD",
        ])
        .output()?;
    if !output.status.success() {
        let _ = remove_known_tree(&paths.packages, &checkout);
        return Err(io::Error::other(format!(
            "dependency_failed: could not resolve {id}"
        )));
    }
    Ok((
        checkout,
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

fn commit_package_graph(
    paths: &PluginPaths,
    previous: &Registry,
    pending: &BTreeMap<String, PendingPackage>,
) -> io::Result<()> {
    let mut registry = previous.clone();
    let mut installed_new = Vec::new();
    let mut installed_roots = BTreeMap::new();
    for package in pending.values() {
        let current_manifest = digest_file(&package.source_dir.join("vvmux-plugin.toml"))?;
        if current_manifest != package.approved_manifest {
            cleanup_temporary(paths, &installed_new);
            return Err(invalid("manifest changed after dependency approval"));
        }
        let loaded = load_package(&package.source_dir)?;
        let (root, digest) = if package.linked {
            (
                package.source_dir.clone(),
                digest_tree(&package.source_dir)?,
            )
        } else {
            let staging = paths.packages.join(format!(
                ".staging-{}-{}",
                std::process::id(),
                &package.approved_manifest[..16]
            ));
            if staging.exists() {
                cleanup_temporary(paths, &installed_new);
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "plugin staging path exists",
                ));
            }
            copy_tree(&package.source_dir, &staging)?;
            let staged = load_package(&staging)?;
            let staged_manifest = digest_file(&staging.join("vvmux-plugin.toml"))?;
            if staged.manifest.plugin.id != package.id
                || staged_manifest != package.approved_manifest
            {
                remove_known_tree(&paths.packages, &staging)?;
                cleanup_temporary(paths, &installed_new);
                return Err(invalid("manifest changed while the package was staged"));
            }
            let digest = digest_tree(&staging)?;
            let destination = paths.package_version(&package.id, &digest);
            if destination.exists() {
                if digest_tree(&destination)? != digest {
                    remove_known_tree(&paths.packages, &staging)?;
                    cleanup_temporary(paths, &installed_new);
                    return Err(invalid("installed version path has an unexpected digest"));
                }
                remove_known_tree(&paths.packages, &staging)?;
            } else {
                let backup = atomic_package_swap(&destination, &staging)?;
                debug_assert!(backup.is_none());
                installed_new.push(destination.clone());
            }
            (destination, digest)
        };
        let entry = entry_for(
            &loaded,
            root.clone(),
            package.source.clone(),
            package.commit.clone(),
            digest,
            package.approved_manifest.clone(),
            package.linked,
        );
        installed_roots.insert(package.id.clone(), root);
        registry.plugins.insert(package.id.clone(), entry);
    }
    if let Err(error) = validate_dependency_graph(&registry)
        .and_then(|_| validate_registry_for_install(&registry))
        .and_then(|_| commit_registry(paths, previous, &mut registry))
    {
        cleanup_temporary(paths, &installed_new);
        return Err(error);
    }
    for (id, root) in installed_roots {
        if let Some(old) = previous.plugins.get(&id)
            && !old.linked
            && old.root != root
            && old.root.exists()
        {
            remove_known_tree(&paths.packages, &old.root)?;
        }
    }
    Ok(())
}

fn validate_registry_for_install(registry: &Registry) -> io::Result<()> {
    let candidate = registry_candidate(registry)?;
    if candidate.failed.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "dependency_failed: registry validation failed: {}",
            candidate
                .failed
                .iter()
                .map(|(id, error)| format!("{id}: {error}"))
                .collect::<Vec<_>>()
                .join("; ")
        )))
    }
}

fn cleanup_temporary(paths: &PluginPaths, paths_to_remove: &[PathBuf]) {
    for path in paths_to_remove {
        let _ = remove_known_tree(&paths.packages, path);
    }
}

fn install_git(
    paths: &PluginPaths,
    source: &str,
    git_ref: Option<&str>,
    yes: bool,
) -> io::Result<()> {
    if !source.starts_with("https://") {
        return Err(invalid("Git plugin sources must use HTTPS"));
    }
    paths.ensure()?;
    let source_hash = hex(&Sha256::digest(source.as_bytes())[..12]);
    let checkout = paths
        .packages
        .join(format!(".git-{}-{source_hash}", std::process::id()));
    if checkout.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Git staging path exists",
        ));
    }
    let mut command = Command::new("git");
    command.args(["clone", "--quiet", "--depth", "1"]);
    if let Some(git_ref) = git_ref {
        validate_git_ref(git_ref)?;
        command.args(["--branch", git_ref]);
    }
    let status = command.arg(source).arg(&checkout).status()?;
    if !status.success() {
        let _ = remove_known_tree(&paths.packages, &checkout);
        return Err(io::Error::other("git clone failed"));
    }
    let output = Command::new("git")
        .args([
            "-C",
            checkout.to_str().unwrap_or_default(),
            "rev-parse",
            "HEAD",
        ])
        .output()?;
    if !output.status.success() {
        remove_known_tree(&paths.packages, &checkout)?;
        return Err(io::Error::other("git rev-parse failed"));
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let result = install_local(
        paths,
        &checkout,
        false,
        yes,
        Some(source.to_owned()),
        Some(commit),
    );
    let _ = remove_known_tree(&paths.packages, &checkout);
    result
}

fn update(paths: &PluginPaths, id: &str, git_ref: Option<&str>, yes: bool) -> io::Result<()> {
    let registry = load_registry(paths)?;
    let entry = registry
        .plugins
        .get(id)
        .ok_or_else(|| not_found(id))?
        .clone();
    if entry.source.starts_with("https://") {
        install_git(paths, &entry.source, git_ref, yes)
    } else {
        install_local(
            paths,
            Path::new(&entry.source),
            entry.linked,
            yes,
            Some(entry.source.clone()),
            entry.commit.clone(),
        )
    }
}

fn uninstall(paths: &PluginPaths, id: &str) -> io::Result<()> {
    let previous = load_registry(paths)?;
    let mut registry = previous.clone();
    let entry = registry.plugins.remove(id).ok_or_else(|| not_found(id))?;
    commit_registry(paths, &previous, &mut registry)?;
    if !entry.linked && entry.root.exists() {
        remove_known_tree(&paths.packages, &entry.root)?;
    }
    println!("uninstalled plugin {id}");
    Ok(())
}

fn set_enabled(paths: &PluginPaths, id: &str, enabled: bool) -> io::Result<()> {
    let previous = load_registry(paths)?;
    let mut registry = previous.clone();
    let entry = registry.plugins.get_mut(id).ok_or_else(|| not_found(id))?;
    entry.enabled = enabled;
    commit_registry(paths, &previous, &mut registry)?;
    println!(
        "{} plugin {id}",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

fn list(paths: &PluginPaths, json: bool) -> io::Result<()> {
    let registry = load_registry(paths)?;
    if json {
        print_json(&registry.plugins.values().collect::<Vec<_>>())
    } else {
        for entry in registry.plugins.values() {
            println!(
                "{}\t{}\t{}\t{}",
                entry.id,
                entry.version,
                if entry.enabled { "enabled" } else { "disabled" },
                entry.runtime_tier
            );
        }
        Ok(())
    }
}

fn inspect(paths: &PluginPaths, id: &str, json: bool) -> io::Result<()> {
    let registry = load_registry(paths)?;
    let entry = registry.plugins.get(id).ok_or_else(|| not_found(id))?;
    let loaded = load_package(&entry.root)?;
    let value = serde_json::json!({
        "registry": entry,
        "manifest": loaded.manifest,
        "warnings": loaded.warnings,
        "trust": trust_text(&loaded),
    });
    if json {
        print_json(&value)
    } else {
        println!("{} {} ({})", entry.id, entry.version, entry.runtime_tier);
        println!("source: {}", entry.source);
        println!("digest: {}", entry.digest);
        println!("trust: {}", trust_text(&loaded));
        for action in &loaded.manifest.actions {
            println!("action: {}/{} — {}", entry.id, action.id, action.title);
        }
        for pane in &loaded.manifest.panes {
            println!(
                "pane: {}/{} — {} ({:?}, hold={}, sync={})",
                entry.id,
                pane.id,
                pane.title,
                pane.placement,
                pane.hold_on_exit,
                pane.accept_sync_input
            );
        }
        for warning in loaded.warnings {
            println!("warning: {warning}");
        }
        Ok(())
    }
}

fn doctor(paths: &PluginPaths, json: bool) -> io::Result<()> {
    let registry = load_registry(paths)?;
    let mut reports = Vec::new();
    for entry in registry.plugins.values() {
        let result = load_package(&entry.root).and_then(|loaded| {
            if loaded.manifest.plugin.id != entry.id {
                Err(invalid("manifest ID differs from registry ID"))
            } else if !entry.linked && digest_tree(&entry.root)? != entry.digest {
                Err(invalid("installed package digest differs from registry"))
            } else {
                Ok(loaded.warnings)
            }
        });
        match result {
            Ok(warnings) => {
                reports.push(serde_json::json!({"id": entry.id, "ok": true, "warnings": warnings}))
            }
            Err(error) => reports
                .push(serde_json::json!({"id": entry.id, "ok": false, "error": error.to_string()})),
        }
    }
    let ok = reports.iter().all(|report| report["ok"] == true);
    if json {
        print_json(&serde_json::json!({"ok": ok, "plugins": reports}))?;
    } else {
        for report in &reports {
            println!(
                "{}\t{}",
                report["id"].as_str().unwrap(),
                if report["ok"] == true { "ok" } else { "failed" }
            );
        }
    }
    if ok {
        Ok(())
    } else {
        Err(invalid("one or more plugins failed validation"))
    }
}

fn permissions(paths: &PluginPaths, id: &str) -> io::Result<()> {
    let registry = load_registry(paths)?;
    let entry = registry.plugins.get(id).ok_or_else(|| not_found(id))?;
    println!("runtime: {}", entry.runtime_tier);
    if entry.runtime_tier == "trusted_native" {
        println!("warning: native plugins run as you with your full OS authority");
    }
    for permission in &entry.permissions {
        println!("{permission}");
    }
    Ok(())
}

fn catalog(_paths: &PluginPaths, args: CatalogArgs) -> io::Result<()> {
    let capabilities = session_capabilities(&args.target)?;
    let plugins = capabilities
        .get("plugins")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("target session returned no plugin capabilities"))?;
    if plugins.get("enabled") != Some(&Value::Bool(true)) {
        return Err(invalid(
            "plugin_disabled: plugins are disabled in the target session",
        ));
    }
    let actions = plugins
        .get("actions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let output = serde_json::json!({
        "target": args.target,
        "session_instance": plugins.get("session_instance"),
        "generation": plugins.get("applied_generation"),
        "failed": plugins.get("failed"),
        "actions": actions,
    });
    if args.json {
        print_json(&output)
    } else {
        for action in output["actions"].as_array().into_iter().flatten() {
            println!(
                "{}\t{}",
                action["reference"].as_str().unwrap(),
                action["title"].as_str().unwrap()
            );
        }
        Ok(())
    }
}

fn invoke(_paths: &PluginPaths, args: InvokeArgs) -> io::Result<()> {
    let input = read_json_input(&args.input)?;
    let output = invoke_via_session(&args.target, args.reference, input, args.detach)?;
    print_json(&output)
}

fn session_capabilities(target: &str) -> io::Result<Value> {
    use crate::ipc::{AutomationMethod, AutomationRequest, ClientMessage};

    crate::runtime::validate_session_name(target)?;
    let (mut reader, writer) = crate::server::connect(target)?;
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(&ClientMessage::Automation(AutomationRequest {
            id: 1,
            pane_id: None,
            allow_focused: false,
            method: AutomationMethod::Capabilities,
        }))?;
    crate::automation::response_result(crate::automation::receive_response(&mut reader, 1)?)
}

fn invoke_via_session(
    target: &str,
    reference: String,
    input: Value,
    detach: bool,
) -> io::Result<Value> {
    use crate::ipc::{AutomationMethod, AutomationRequest, ClientMessage, PluginMethod};

    crate::runtime::validate_session_name(target)?;
    let (mut reader, writer) = crate::server::connect(target)?;
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(&ClientMessage::Automation(AutomationRequest {
            id: 1,
            pane_id: None,
            allow_focused: false,
            method: AutomationMethod::Plugin(PluginMethod::Invoke {
                reference,
                input,
                detach,
            }),
        }))?;
    crate::automation::response_result(crate::automation::receive_response(&mut reader, 1)?)
}

fn job(command: PluginJobCommand) -> io::Result<()> {
    use crate::ipc::PluginMethod;

    let (job_id, operation) = match command {
        PluginJobCommand::Status { job_id } => {
            let operation = PluginMethod::JobStatus {
                job_id: job_id.clone(),
            };
            (job_id, operation)
        }
        PluginJobCommand::Cancel { job_id } => {
            let operation = PluginMethod::JobCancel {
                job_id: job_id.clone(),
            };
            (job_id, operation)
        }
        PluginJobCommand::Logs { job_id } => {
            let operation = PluginMethod::JobLogs {
                job_id: job_id.clone(),
            };
            (job_id, operation)
        }
    };
    let target = job_target(&job_id)?;
    print_json(&plugin_session_request(target, operation)?)
}

fn pane(command: PluginPaneCommand) -> io::Result<()> {
    let (target, operation) = match command {
        PluginPaneCommand::Open { reference, target } => {
            (target, crate::ipc::PluginMethod::PaneOpen { reference })
        }
    };
    crate::runtime::validate_session_name(&target)?;
    print_json(&plugin_session_request(&target, operation)?)
}

fn events(target: &str, after_sequence: Option<u64>) -> io::Result<()> {
    use crate::ipc::{
        AutomationMethod, AutomationRequest, ClientMessage, PluginMethod, ServerMessage,
    };

    crate::runtime::validate_session_name(target)?;
    let (mut reader, writer) = crate::server::connect(target)?;
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(&ClientMessage::Automation(AutomationRequest {
            id: 1,
            pane_id: None,
            allow_focused: false,
            method: AutomationMethod::Plugin(PluginMethod::EventSubscribe { after_sequence }),
        }))?;
    let mut subscribed = false;
    loop {
        match reader.recv_server()? {
            ServerMessage::Automation(response) if response.id == 1 => {
                crate::automation::response_result(response)?;
                subscribed = true;
            }
            ServerMessage::PluginEvent { envelope, .. } if subscribed => {
                println!(
                    "{}",
                    serde_json::to_string(&envelope).map_err(io::Error::other)?
                );
            }
            _ => {}
        }
    }
}

fn job_target(job_id: &str) -> io::Result<&str> {
    let (target, _) = job_id
        .split_once('/')
        .ok_or_else(|| invalid("plugin job ID must include its target session"))?;
    if !valid_job_id(job_id) {
        return Err(invalid("invalid plugin job ID"));
    }
    Ok(target)
}

pub(crate) fn valid_job_id(job_id: &str) -> bool {
    let Some((target, opaque)) = job_id.split_once('/') else {
        return false;
    };
    let Some((instance, counter)) = opaque.split_once('-') else {
        return false;
    };
    job_id.len() <= 256
        && crate::runtime::validate_session_name(target).is_ok()
        && instance.len() == 32
        && instance.bytes().all(|byte| byte.is_ascii_hexdigit())
        && counter.len() == 16
        && counter.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn plugin_session_request(target: &str, method: crate::ipc::PluginMethod) -> io::Result<Value> {
    use crate::ipc::{AutomationMethod, AutomationRequest, ClientMessage};

    let (mut reader, writer) = crate::server::connect(target)?;
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(&ClientMessage::Automation(AutomationRequest {
            id: 1,
            pane_id: None,
            allow_focused: false,
            method: AutomationMethod::Plugin(method),
        }))?;
    crate::automation::response_result(crate::automation::receive_response(&mut reader, 1)?)
}

pub(crate) struct SessionPluginRuntime {
    session_name: String,
    session_instance: String,
    plugin: RuntimePlugin,
    loaded: LoadedManifest,
    broker: crate::plugin_supervisor::HostBroker,
    service: Option<NativeService>,
    component: Option<crate::plugin_component::ComponentRuntime>,
    consecutive_crashes: u32,
    retry_at: Option<Instant>,
    last_logs: RuntimeLogs,
}

#[derive(Default)]
pub(crate) struct RuntimeLogs {
    pub(crate) stderr: String,
    pub(crate) stderr_truncated: bool,
}

impl SessionPluginRuntime {
    pub(crate) fn new(
        session_name: String,
        session_instance: String,
        plugin: RuntimePlugin,
        broker: crate::plugin_supervisor::HostBroker,
    ) -> io::Result<Self> {
        if !plugin.enabled {
            return Err(invalid("plugin_disabled: plugin is disabled"));
        }
        let loaded = load_package(&plugin.root)?;
        if loaded.manifest.plugin.id != plugin.id {
            return Err(invalid("scope_denied: plugin manifest identity mismatch"));
        }
        Ok(Self {
            session_name,
            session_instance,
            plugin,
            loaded,
            broker,
            service: None,
            component: None,
            consecutive_crashes: 0,
            retry_at: None,
            last_logs: RuntimeLogs::default(),
        })
    }

    pub(crate) fn invoke(
        &mut self,
        reference: &str,
        input: Value,
        cancel: Arc<AtomicBool>,
        context: Option<InvocationContext>,
    ) -> io::Result<Value> {
        self.last_logs = RuntimeLogs::default();
        if cancel.load(Ordering::Acquire) {
            return Err(invalid("cancelled: plugin invocation was cancelled"));
        }
        let (plugin_id, action_id) = reference
            .split_once('/')
            .ok_or_else(|| invalid("action_not_found: plugin reference must be ID/ACTION"))?;
        if plugin_id != self.plugin.id {
            return Err(invalid("scope_denied: plugin worker identity mismatch"));
        }
        crate::runtime::validate_session_name(&self.session_name)?;
        let action = self
            .loaded
            .action(action_id)
            .cloned()
            .ok_or_else(|| invalid("action_not_found: action does not exist"))?;
        self.loaded
            .validate_input(&action, &input)
            .map_err(|errors| invalid(format!("schema_invalid: {}", errors.join("; "))))?;
        let action_timeout = Duration::from_millis(action.timeout_ms);
        let deadline_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_add(action_timeout.as_millis())
            .min(u128::from(u64::MAX)) as u64;
        let mut context = match context {
            Some(context) => context,
            None => {
                let correlation_id = random_id()?;
                InvocationContext {
                    correlation_id: correlation_id.clone(),
                    causation_id: correlation_id,
                    causation_depth: 0,
                    source: "automation".into(),
                    session_instance: self.session_instance.clone(),
                    pane_id: None,
                    tab_id: None,
                    deadline_unix_ms,
                }
            }
        };
        context.session_instance.clone_from(&self.session_instance);
        if context.deadline_unix_ms == 0 {
            context.deadline_unix_ms = deadline_unix_ms;
        } else {
            context.deadline_unix_ms = context.deadline_unix_ms.min(deadline_unix_ms);
        }
        let remaining_ms = context
            .deadline_unix_ms
            .saturating_sub(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            )
            .max(1);
        let timeout = action_timeout.min(Duration::from_millis(remaining_ms));
        let output = if let Some(argv) = action.command.as_deref() {
            run_one_shot(
                &self.loaded.root,
                argv,
                &input,
                timeout,
                OneShotContext {
                    session: Some(&self.session_name),
                    plugin_id,
                    cancel: Some(&cancel),
                    session_instance: Some(&self.session_instance),
                },
            )?
        } else if let (Some(handler), Some(runtime)) = (
            action.handler.as_deref(),
            self.loaded.manifest.runtime.as_ref(),
        ) {
            match runtime.kind {
                RuntimeKind::Process => {
                    if self
                        .retry_at
                        .is_some_and(|retry_at| retry_at > Instant::now())
                    {
                        return Err(invalid(
                            "runtime_unavailable: native plugin is in crash backoff",
                        ));
                    }
                    if self.service.is_none() {
                        match NativeService::start(
                            &self.loaded.root,
                            runtime.command.as_deref().unwrap(),
                            timeout,
                            NativeServiceContext {
                                plugin_id,
                                session: Some(&self.session_name),
                                session_instance: &self.session_instance,
                                broker: Some(&self.broker),
                                permissions: &self.loaded.manifest.plugin.permissions,
                            },
                        ) {
                            Ok(service) => self.service = Some(service),
                            Err(error) => {
                                self.note_crash();
                                return Err(error);
                            }
                        }
                    }
                    let result = self.service.as_mut().unwrap().invoke(
                        handler,
                        input,
                        timeout,
                        context,
                        Some(&cancel),
                    );
                    if self.service.as_ref().unwrap().healthy {
                        self.consecutive_crashes = 0;
                        self.retry_at = None;
                    } else {
                        self.service.take();
                        self.note_crash();
                    }
                    result?
                }
                RuntimeKind::Component => {
                    if self
                        .retry_at
                        .is_some_and(|retry_at| retry_at > Instant::now())
                    {
                        return Err(invalid(
                            "runtime_unavailable: component plugin is in crash backoff",
                        ));
                    }
                    let deadline = Instant::now() + timeout;
                    let mut component_context =
                        serde_json::to_value(&context).map_err(io::Error::other)?;
                    component_context["session"] = Value::String(self.session_name.clone());
                    if self.component.is_none() {
                        match crate::plugin_component::ComponentRuntime::start(
                            &self.loaded.root,
                            runtime.artifact.as_deref().unwrap(),
                            plugin_id,
                            Some(&self.session_instance),
                            Some(&self.broker),
                            &self.loaded.manifest.plugin.permissions,
                            &runtime.preopens,
                            cancel.clone(),
                            deadline,
                        ) {
                            Ok(component) => self.component = Some(component),
                            Err(error) => {
                                self.note_crash();
                                return Err(error);
                            }
                        }
                    }
                    let result = self.component.as_mut().unwrap().invoke(
                        handler,
                        &input,
                        &component_context,
                        &context,
                        cancel,
                        deadline,
                    );
                    let (stderr, stderr_truncated) = self.component.as_mut().unwrap().take_logs();
                    self.last_logs = RuntimeLogs {
                        stderr,
                        stderr_truncated,
                    };
                    match result {
                        Ok(output) => {
                            self.consecutive_crashes = 0;
                            self.retry_at = None;
                            output
                        }
                        Err(error) => {
                            let message = error.to_string();
                            if message.starts_with("runtime_crashed")
                                || message.starts_with("timeout")
                                || message.starts_with("cancelled")
                            {
                                self.component.take();
                                self.note_crash();
                            }
                            return Err(error);
                        }
                    }
                }
            }
        } else {
            return Err(invalid(
                "runtime_unavailable: action has no executable runtime",
            ));
        };
        self.loaded
            .validate_output(&action, &output)
            .map_err(|errors| invalid(format!("output_invalid: {}", errors.join("; "))))?;
        Ok(output)
    }

    pub(crate) fn activate(&mut self) -> io::Result<()> {
        let Some(runtime) = self.loaded.manifest.runtime.clone() else {
            return Ok(());
        };
        let timeout = Duration::from_secs(10);
        match runtime.kind {
            RuntimeKind::Process if self.service.is_none() => {
                self.service = Some(NativeService::start(
                    &self.loaded.root,
                    runtime.command.as_deref().unwrap(),
                    timeout,
                    NativeServiceContext {
                        plugin_id: &self.plugin.id,
                        session: Some(&self.session_name),
                        session_instance: &self.session_instance,
                        broker: Some(&self.broker),
                        permissions: &self.loaded.manifest.plugin.permissions,
                    },
                )?);
            }
            RuntimeKind::Component if self.component.is_none() => {
                let deadline = Instant::now() + timeout;
                self.component = Some(crate::plugin_component::ComponentRuntime::start(
                    &self.loaded.root,
                    runtime.artifact.as_deref().unwrap(),
                    &self.plugin.id,
                    Some(&self.session_instance),
                    Some(&self.broker),
                    &self.loaded.manifest.plugin.permissions,
                    &runtime.preopens,
                    Arc::new(AtomicBool::new(false)),
                    deadline,
                )?);
            }
            RuntimeKind::Process | RuntimeKind::Component => {}
        }
        self.consecutive_crashes = 0;
        self.retry_at = None;
        Ok(())
    }

    pub(crate) fn on_event(
        &mut self,
        hook: &EventHook,
        event: Event,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<()> {
        self.last_logs = RuntimeLogs::default();
        let timeout = Duration::from_millis(hook.timeout_ms);
        if cancel.load(Ordering::Acquire) {
            return Err(invalid("cancelled: plugin event was cancelled"));
        }
        if let Some(argv) = hook.command.as_deref() {
            let input = serde_json::to_value(&event).map_err(io::Error::other)?;
            let _ = run_one_shot(
                &self.loaded.root,
                argv,
                &input,
                timeout,
                OneShotContext {
                    session: Some(&self.session_name),
                    plugin_id: &self.plugin.id,
                    cancel: Some(&cancel),
                    session_instance: Some(&self.session_instance),
                },
            )?;
            return Ok(());
        }
        let handler = hook
            .handler
            .as_deref()
            .ok_or_else(|| invalid("runtime_unavailable: event hook has no handler"))?;
        self.activate()?;
        match self
            .loaded
            .manifest
            .runtime
            .as_ref()
            .map(|runtime| runtime.kind)
        {
            Some(RuntimeKind::Process) => {
                let result = self
                    .service
                    .as_mut()
                    .expect("activation starts native service")
                    .on_event(handler, event, timeout, &cancel);
                if !self.service.as_ref().unwrap().healthy {
                    self.service.take();
                    self.note_crash();
                }
                result
            }
            Some(RuntimeKind::Component) => {
                let deadline = Instant::now() + timeout;
                let payload = serde_json::json!({
                    "event": event.name,
                    "sequence": event.sequence,
                    "payload": event.payload,
                });
                let result = self
                    .component
                    .as_mut()
                    .expect("activation starts component")
                    .on_event(handler, &payload, &event.context, cancel, deadline);
                let (stderr, stderr_truncated) = self.component.as_mut().unwrap().take_logs();
                self.last_logs = RuntimeLogs {
                    stderr,
                    stderr_truncated,
                };
                if result.is_err() {
                    self.component.take();
                    self.note_crash();
                }
                result
            }
            None => Err(invalid("runtime_unavailable: event handler has no runtime")),
        }
    }

    pub(crate) fn take_logs(&mut self) -> RuntimeLogs {
        std::mem::take(&mut self.last_logs)
    }

    fn note_crash(&mut self) {
        self.consecutive_crashes = self.consecutive_crashes.saturating_add(1);
        self.retry_at = Some(Instant::now() + crash_backoff(self.consecutive_crashes));
    }
}

fn crash_backoff(consecutive_crashes: u32) -> Duration {
    let exponent = consecutive_crashes.saturating_sub(1).min(8);
    Duration::from_millis(100_u64.saturating_mul(1_u64 << exponent)).min(Duration::from_secs(30))
}

struct NativeService {
    plugin_id: String,
    child: std::process::Child,
    process_id: u32,
    writer: Option<std::process::ChildStdin>,
    receiver: std::sync::mpsc::Receiver<io::Result<NativeReply>>,
    reader: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<io::Result<CappedOutput>>>,
    next_request_id: u64,
    healthy: bool,
    broker_lease: Option<crate::plugin_supervisor::BrokerLease>,
}

struct NativeServiceContext<'a> {
    plugin_id: &'a str,
    session: Option<&'a str>,
    session_instance: &'a str,
    broker: Option<&'a crate::plugin_supervisor::HostBroker>,
    permissions: &'a [Permission],
}

impl NativeService {
    fn start(
        root: &Path,
        argv: &[String],
        timeout: Duration,
        context: NativeServiceContext<'_>,
    ) -> io::Result<Self> {
        let instance_id = random_id()?;
        let broker_lease = context
            .broker
            .map(|broker| broker.issue(context.plugin_id, &instance_id, context.permissions))
            .transpose()?;
        let mut command = trusted_command(root, argv, context.session, context.plugin_id);
        command
            .env("VVMUX_PLUGIN_INSTANCE", &instance_id)
            .env("VVMUX_SESSION_INSTANCE", context.session_instance);
        if let Some(lease) = &broker_lease {
            command.env("VVMUX_PLUGIN_BROKER_TOKEN", lease.token());
        }
        let mut child = command.spawn()?;
        let process_id = child.id();
        let writer = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(8);
        let reader = thread::spawn(move || {
            loop {
                let frame = read_frame::<NativeReply>(&mut stdout).map_err(|error| match error {
                    FrameError::Io(error) => io::Error::other(format!(
                        "runtime_crashed: native plugin protocol closed: {error}"
                    )),
                    FrameError::TooLarge(_) | FrameError::Json(_) => {
                        invalid(format!("protocol_error: {error}"))
                    }
                });
                let stop = frame.is_err();
                if sender.send(frame).is_err() || stop {
                    break;
                }
            }
        });
        let stderr_reader = thread::spawn(move || read_capped(stderr, MAX_LOG_OUTPUT));
        let mut service = Self {
            plugin_id: context.plugin_id.to_owned(),
            child,
            process_id,
            writer: Some(writer),
            receiver,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            next_request_id: 2,
            healthy: true,
            broker_lease,
        };
        let deadline = Instant::now() + timeout;
        let hello = service.receive(deadline)?;
        if !matches!(
            hello,
            NativeReply::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                plugin_id: actual_plugin,
                instance_id: actual_instance,
                ..
            }) if actual_plugin == context.plugin_id && actual_instance == instance_id
        ) {
            service.healthy = false;
            return Err(invalid("runtime_unavailable: invalid native plugin hello"));
        }
        service.write(&NativeMessage::Initialize { request_id: 1 })?;
        if !matches!(
            service.receive(deadline)?,
            NativeReply::Ready { request_id: 1 }
        ) {
            service.healthy = false;
            return Err(invalid("protocol_error: native plugin did not initialize"));
        }
        Ok(service)
    }

    fn invoke(
        &mut self,
        handler: &str,
        input: Value,
        timeout: Duration,
        mut context: InvocationContext,
        cancel: Option<&AtomicBool>,
    ) -> io::Result<Value> {
        if !self.healthy || self.child.try_wait()?.is_some() {
            self.healthy = false;
            return Err(invalid("runtime_crashed: native plugin process exited"));
        }
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(2);
        let deadline_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_add(timeout.as_millis())
            .min(u128::from(u64::MAX)) as u64;
        context.deadline_unix_ms = context.deadline_unix_ms.min(deadline_unix_ms);
        let _cause = self
            .broker_lease
            .as_ref()
            .map(|lease| lease.enter_event(&context));
        self.write(&NativeMessage::Invoke(Invocation {
            request_id,
            action: handler.to_owned(),
            input,
            context,
        }))?;
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
                return self.cancel(request_id, "cancelled: plugin invocation was cancelled");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.cancel(request_id, "timeout: native plugin exceeded its deadline");
            }
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(20)))
            {
                Ok(Ok(NativeReply::Result(result))) if result.request_id == request_id => {
                    return Ok(result.result);
                }
                Ok(Ok(NativeReply::Error(error))) if error.request_id == request_id => {
                    return Err(invalid(format!(
                        "{}: {}",
                        serde_json::to_value(error.code).unwrap().as_str().unwrap(),
                        error.message
                    )));
                }
                Ok(Ok(NativeReply::HostCall(call))) => {
                    let host_request_id = call.request_id;
                    if host_request_id == 0 {
                        self.healthy = false;
                        return Err(invalid(
                            "protocol_error: host-call request ID must be nonzero",
                        ));
                    }
                    let reply = match &self.broker_lease {
                        Some(lease) => match lease.call(call, deadline) {
                            Ok(result) => NativeMessage::HostCallResult(HostCallResult {
                                request_id: host_request_id,
                                result,
                            }),
                            Err(error) => NativeMessage::HostCallError(plugin_error_from_io(
                                host_request_id,
                                &error,
                            )),
                        },
                        None => NativeMessage::HostCallError(PluginError {
                            request_id: host_request_id,
                            code: ErrorCode::CapabilityDenied,
                            message: "host calls require a live session plugin supervisor".into(),
                        }),
                    };
                    self.write(&reply)?;
                }
                Ok(Ok(_)) => {
                    self.healthy = false;
                    return Err(invalid("protocol_error: unexpected native plugin reply"));
                }
                Ok(Err(error)) => {
                    self.healthy = false;
                    return Err(error);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.healthy = false;
                    return Err(invalid("runtime_crashed: native plugin protocol closed"));
                }
            }
        }
    }

    fn on_event(
        &mut self,
        handler: &str,
        mut event: Event,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> io::Result<()> {
        if !self.healthy || self.child.try_wait()?.is_some() {
            self.healthy = false;
            return Err(invalid("runtime_crashed: native plugin process exited"));
        }
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(2);
        event.request_id = request_id;
        event.payload = serde_json::json!({
            "event": event.name,
            "payload": event.payload,
        });
        event.name = handler.to_owned();
        let _cause = self
            .broker_lease
            .as_ref()
            .map(|lease| lease.enter_event(&event.context));
        self.write(&NativeMessage::Event(event))?;
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::Acquire) {
                let _ = self.cancel(request_id, "cancelled: plugin event was cancelled");
                return Err(invalid("cancelled: plugin event was cancelled"));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.cancel(request_id, "timeout: plugin event exceeded its deadline");
                return Err(invalid("timeout: plugin event exceeded its deadline"));
            }
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(20)))
            {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.healthy = false;
                    return Err(invalid("runtime_crashed: native plugin protocol closed"));
                }
                Ok(Err(error)) => {
                    self.healthy = false;
                    return Err(error);
                }
                Ok(Ok(reply)) => match reply {
                    NativeReply::Ready {
                        request_id: reply_id,
                    } if reply_id == request_id => return Ok(()),
                    NativeReply::Error(error) if error.request_id == request_id => {
                        return Err(invalid(format!(
                            "{}: {}",
                            serde_json::to_value(error.code).unwrap().as_str().unwrap(),
                            error.message
                        )));
                    }
                    NativeReply::HostCall(call) => {
                        let host_request_id = call.request_id;
                        let reply = match &self.broker_lease {
                            Some(lease) => match lease.call(call, deadline) {
                                Ok(result) => NativeMessage::HostCallResult(HostCallResult {
                                    request_id: host_request_id,
                                    result,
                                }),
                                Err(error) => NativeMessage::HostCallError(plugin_error_from_io(
                                    host_request_id,
                                    &error,
                                )),
                            },
                            None => NativeMessage::HostCallError(PluginError {
                                request_id: host_request_id,
                                code: ErrorCode::CapabilityDenied,
                                message: "host calls require a live session plugin supervisor"
                                    .into(),
                            }),
                        };
                        self.write(&reply)?;
                    }
                    _ => {
                        self.healthy = false;
                        return Err(invalid(
                            "protocol_error: unexpected native plugin event reply",
                        ));
                    }
                },
            }
        }
    }

    fn write(&mut self, message: &NativeMessage) -> io::Result<()> {
        write_frame(self.writer.as_mut().unwrap(), message)
            .map_err(|error| invalid(format!("protocol_error: {error}")))
            .inspect_err(|_| self.healthy = false)
    }

    fn receive(&mut self, deadline: Instant) -> io::Result<NativeReply> {
        match self
            .receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.healthy = false;
                Err(invalid("timeout: native plugin exceeded its deadline"))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.healthy = false;
                Err(invalid("runtime_crashed: native plugin protocol closed"))
            }
        }
    }

    fn cancel(&mut self, request_id: u64, message: &'static str) -> io::Result<Value> {
        let _ = self.write(&NativeMessage::Cancel { request_id });
        let grace = Instant::now() + Duration::from_secs(2);
        match self
            .receiver
            .recv_timeout(grace.saturating_duration_since(Instant::now()))
        {
            Ok(Ok(NativeReply::Cancelled {
                request_id: reply_id,
            })) if reply_id == request_id => Err(invalid(message)),
            Ok(Ok(NativeReply::Result(result))) if result.request_id == request_id => {
                Err(invalid(message))
            }
            Ok(Ok(NativeReply::Error(error))) if error.request_id == request_id => {
                Err(invalid(message))
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.healthy = false;
                Err(invalid(message))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.healthy = false;
                terminate_process_tree(&mut self.child, self.process_id);
                Err(invalid(message))
            }
        }
    }

    fn shutdown(&mut self) {
        if self.healthy {
            let request_id = self.next_request_id;
            let _ = self.write(&NativeMessage::Shutdown { request_id });
            let deadline = Instant::now() + Duration::from_secs(2);
            if wait_until(&mut self.child, deadline)
                .ok()
                .flatten()
                .is_none()
            {
                terminate_process_tree(&mut self.child, self.process_id);
            }
        } else if self.child.try_wait().ok().flatten().is_none() {
            terminate_process_tree(&mut self.child, self.process_id);
        }
        self.writer.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(stderr_reader) = self.stderr_reader.take()
            && let Ok(Ok(logs)) = stderr_reader.join()
            && logs.truncated
        {
            eprintln!(
                "vvmux: plugin {} stderr truncated at {MAX_LOG_OUTPUT} bytes",
                self.plugin_id
            );
        }
    }
}

fn plugin_error_from_io(request_id: u64, error: &io::Error) -> PluginError {
    let message = error.to_string();
    let code = if message.starts_with("capability_denied") {
        ErrorCode::CapabilityDenied
    } else if message.starts_with("scope_denied") {
        ErrorCode::ScopeDenied
    } else if message.starts_with("busy") {
        ErrorCode::Busy
    } else if message.starts_with("timeout") {
        ErrorCode::Timeout
    } else if message.starts_with("cancelled") {
        ErrorCode::Cancelled
    } else if message.starts_with("action_not_found") {
        ErrorCode::ActionNotFound
    } else if message.starts_with("plugin_not_found") {
        ErrorCode::PluginNotFound
    } else if message.starts_with("plugin_disabled") {
        ErrorCode::PluginDisabled
    } else if message.starts_with("schema_invalid") {
        ErrorCode::SchemaInvalid
    } else if message.starts_with("runtime_crashed") {
        ErrorCode::RuntimeCrashed
    } else if message.starts_with("dependency_failed") {
        ErrorCode::DependencyFailed
    } else if message.starts_with("output_invalid") {
        ErrorCode::OutputInvalid
    } else if message.starts_with("protocol_error") {
        ErrorCode::ProtocolError
    } else {
        ErrorCode::RuntimeUnavailable
    };
    PluginError {
        request_id,
        code,
        message,
    }
}

impl Drop for NativeService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn random_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(hex(&bytes))
}

fn resolve(paths: &PluginPaths, frozen: bool) -> io::Result<()> {
    let registry = load_registry(paths)?;
    validate_dependency_graph(&registry)?;
    let encoded = encode_lock(&registry)?;
    let lock: LockFile = toml::from_str(&encoded).map_err(io::Error::other)?;
    if frozen {
        let existing = fs::read_to_string(&paths.lock)
            .map_err(|error| invalid(format!("--frozen requires an existing lock: {error}")))?;
        let existing: LockFile = toml::from_str(&existing).map_err(io::Error::other)?;
        if existing != lock {
            return Err(invalid("--frozen lock does not match the registry"));
        }
    } else {
        paths.ensure()?;
        write_private_atomic(&paths.lock, encoded.as_bytes())?;
    }
    println!("resolved {} plugins", lock.packages.len());
    Ok(())
}

fn encode_lock(registry: &Registry) -> io::Result<String> {
    let mut packages = Vec::with_capacity(registry.plugins.len());
    for entry in registry.plugins.values() {
        let loaded = load_package(&entry.root)?;
        packages.push(LockedPackage {
            id: entry.id.clone(),
            version: loaded.manifest.plugin.version.to_string(),
            source: entry.source.clone(),
            commit: entry.commit.clone(),
            manifest_digest: digest_file(&entry.root.join("vvmux-plugin.toml"))?,
            artifact_digest: digest_tree(&entry.root)?,
        });
    }
    let lock = LockFile {
        lock_version: 1,
        packages,
    };
    toml::to_string_pretty(&lock).map_err(io::Error::other)
}

fn validate_dependency_graph(registry: &Registry) -> io::Result<()> {
    if registry.plugins.len() > 64 {
        return Err(invalid("dependency graph exceeds 64 packages"));
    }
    for entry in registry.plugins.values() {
        let loaded = load_package(&entry.root)?;
        for dependency in &loaded.manifest.dependencies {
            let installed = registry.plugins.get(&dependency.id).ok_or_else(|| {
                invalid(format!(
                    "dependency_failed: {} requires {}",
                    entry.id, dependency.id
                ))
            })?;
            let version = semver::Version::parse(&installed.version).map_err(io::Error::other)?;
            if !dependency.version.matches(&version) {
                return Err(invalid(format!(
                    "dependency_failed: {} requires {} {}, installed {}",
                    entry.id, dependency.id, dependency.version, installed.version
                )));
            }
        }
    }
    detect_dependency_cycles(registry)?;
    Ok(())
}

pub(crate) fn load_package(root: &Path) -> io::Result<LoadedManifest> {
    LoadedManifest::load(root).map_err(|error| invalid(error.to_string()))
}

fn preview(loaded: &LoadedManifest, source: &str) {
    println!(
        "plugin: {} {}",
        loaded.manifest.plugin.id, loaded.manifest.plugin.version
    );
    println!("source: {source}");
    println!("runtime: {}", runtime_tier(loaded));
    if runtime_tier(loaded) == "trusted_native" {
        println!("warning: native plugins run as you with your full OS authority");
    }
    if !loaded.manifest.plugin.permissions.is_empty() {
        println!("permissions: {:?}", loaded.manifest.plugin.permissions);
    }
    for action in &loaded.manifest.actions {
        println!("action: {} — {}", action.id, action.title);
    }
    for pane in &loaded.manifest.panes {
        println!(
            "pane: {} — {} ({:?}, hold={}, sync={}) argv={:?}",
            pane.id,
            pane.title,
            pane.placement,
            pane.hold_on_exit,
            pane.accept_sync_input,
            pane.command
        );
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ApprovalDelta {
    source_change: Option<(String, String)>,
    runtime_tier_change: Option<(String, String)>,
    added_permissions: Vec<String>,
}

impl ApprovalDelta {
    fn requires_fresh_approval(&self) -> bool {
        self.source_change.is_some()
            || self.runtime_tier_change.is_some()
            || !self.added_permissions.is_empty()
    }
}

fn approval_delta(
    previous: Option<&RegistryEntry>,
    loaded: &LoadedManifest,
    source: &str,
) -> ApprovalDelta {
    let Some(previous) = previous else {
        return ApprovalDelta::default();
    };
    let next_tier = runtime_tier(loaded).to_owned();
    let next_permissions = permission_names(loaded);
    ApprovalDelta {
        source_change: (previous.source != source)
            .then(|| (previous.source.clone(), source.to_owned())),
        runtime_tier_change: (previous.runtime_tier != next_tier)
            .then(|| (previous.runtime_tier.clone(), next_tier)),
        added_permissions: next_permissions
            .into_iter()
            .filter(|permission| !previous.permissions.contains(permission))
            .collect(),
    }
}

fn print_approval_delta(delta: &ApprovalDelta) {
    if let Some((previous, next)) = &delta.source_change {
        println!("approval change: source {previous} -> {next}");
    }
    if let Some((previous, next)) = &delta.runtime_tier_change {
        println!("approval change: runtime {previous} -> {next}");
    }
    if !delta.added_permissions.is_empty() {
        println!(
            "approval change: added permissions {:?}",
            delta.added_permissions
        );
    }
}

fn confirm_if_needed(loaded: &LoadedManifest, delta: &ApprovalDelta, yes: bool) -> io::Result<()> {
    if yes {
        return Ok(());
    }
    if delta.requires_fresh_approval() {
        eprint!(
            "approve security-relevant changes to {}? [y/N] ",
            loaded.manifest.plugin.id
        );
    } else {
        eprint!("install {}? [y/N] ", loaded.manifest.plugin.id);
    }
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "installation cancelled",
        ));
    }
    Ok(())
}

fn entry_for(
    loaded: &LoadedManifest,
    root: PathBuf,
    source: String,
    commit: Option<String>,
    digest: String,
    manifest_digest: String,
    linked: bool,
) -> RegistryEntry {
    RegistryEntry {
        id: loaded.manifest.plugin.id.clone(),
        version: loaded.manifest.plugin.version.to_string(),
        root,
        source,
        commit,
        digest,
        manifest_digest,
        enabled: true,
        linked,
        runtime_tier: runtime_tier(loaded).into(),
        permissions: permission_names(loaded),
    }
}

fn permission_names(loaded: &LoadedManifest) -> Vec<String> {
    loaded
        .manifest
        .plugin
        .permissions
        .iter()
        .map(|permission| {
            serde_json::to_value(permission)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn runtime_tier(loaded: &LoadedManifest) -> &'static str {
    match loaded.manifest.runtime.as_ref().map(|runtime| runtime.kind) {
        Some(RuntimeKind::Component) => "sandboxed_component",
        Some(RuntimeKind::Process) => "trusted_native",
        None if loaded
            .manifest
            .actions
            .iter()
            .any(|action| action.command.is_some())
            || !loaded.manifest.panes.is_empty() =>
        {
            "trusted_native"
        }
        None => "workflow",
    }
}

fn trust_text(loaded: &LoadedManifest) -> &'static str {
    if runtime_tier(loaded) == "sandboxed_component" {
        "WebAssembly Component with declared host capabilities"
    } else if runtime_tier(loaded) == "trusted_native" {
        "trusted user code with the user's full OS authority; broker capabilities are not a sandbox"
    } else {
        "TOML-only workflow bundle"
    }
}

struct OneShotContext<'a> {
    session: Option<&'a str>,
    plugin_id: &'a str,
    cancel: Option<&'a AtomicBool>,
    session_instance: Option<&'a str>,
}

fn run_one_shot(
    root: &Path,
    argv: &[String],
    input: &Value,
    timeout: Duration,
    context: OneShotContext<'_>,
) -> io::Result<Value> {
    let instance_id = random_id()?;
    let mut command = trusted_command(root, argv, context.session, context.plugin_id);
    command.env("VVMUX_PLUGIN_INSTANCE", &instance_id);
    if let Some(session_instance) = context.session_instance {
        command.env("VVMUX_SESSION_INSTANCE", session_instance);
    }
    let mut child = command.spawn()?;
    let process_id = child.id();
    let mut stdin = child.stdin.take().unwrap();
    let body = serde_json::to_vec(input).map_err(io::Error::other)?;
    if body.len() > MAX_ACTION_OUTPUT {
        terminate_process_tree(&mut child, process_id);
        return Err(invalid("schema_invalid: action input exceeds 1 MiB"));
    }
    stdin.write_all(&body)?;
    drop(stdin);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdout_reader = thread::spawn(move || read_capped(stdout, MAX_ACTION_OUTPUT));
    let stderr_reader = thread::spawn(move || read_capped(stderr, MAX_LOG_OUTPUT));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if context
            .cancel
            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
        {
            terminate_process_tree(&mut child, process_id);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(invalid("cancelled: plugin invocation was cancelled"));
        }
        if Instant::now() >= deadline {
            terminate_process_tree(&mut child, process_id);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(invalid("timeout: plugin action exceeded its deadline"));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))??;
    if !status.success() {
        return Err(io::Error::other(format!(
            "runtime_crashed: plugin exited with {status}: {}",
            String::from_utf8_lossy(&stderr.bytes)
        )));
    }
    if stdout.truncated {
        return Err(invalid("output_invalid: plugin output exceeds 1 MiB"));
    }
    serde_json::from_slice(&stdout.bytes)
        .map_err(|error| invalid(format!("output_invalid: {error}")))
}

fn trusted_command(
    root: &Path,
    argv: &[String],
    session: Option<&str>,
    plugin_id: &str,
) -> Command {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(root).env_clear();
    for key in [
        "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR", "TEMP", "TMP",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("VVMUX_PLUGIN_ID", plugin_id);
    if let Some(session) = session {
        command.env("VVMUX_SESSION", session);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    command
}

fn wait_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> io::Result<Option<std::process::ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

struct CappedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_capped(mut reader: impl Read, limit: usize) -> io::Result<CappedOutput> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    Ok(CappedOutput { bytes, truncated })
}

fn terminate_process_tree(child: &mut std::process::Child, process_id: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(process_id as i32), libc::SIGTERM);
        let grace = Instant::now() + Duration::from_secs(2);
        while Instant::now() < grace {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        libc::kill(-(process_id as i32), libc::SIGKILL);
        let _ = child.wait();
    }
    #[cfg(windows)]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn read_json_input(argument: &str) -> io::Result<Value> {
    let bytes = if argument == "-" {
        let mut bytes = Vec::new();
        io::stdin()
            .take((MAX_ACTION_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes)?;
        bytes
    } else if let Some(path) = argument.strip_prefix('@') {
        fs::read(path)?
    } else {
        argument.as_bytes().to_vec()
    };
    if bytes.len() > MAX_ACTION_OUTPUT {
        return Err(invalid("action input exceeds 1 MiB"));
    }
    serde_json::from_slice(&bytes).map_err(|error| invalid(format!("invalid JSON input: {error}")))
}

fn atomic_package_swap(destination: &Path, staging: &Path) -> io::Result<Option<PathBuf>> {
    let backup = destination.with_extension(format!("previous.{}", std::process::id()));
    if destination.exists() {
        fs::rename(destination, &backup)?;
    }
    if let Err(error) = fs::rename(staging, destination) {
        if backup.exists() {
            let _ = fs::rename(&backup, destination);
        }
        return Err(error);
    }
    Ok(backup.exists().then_some(backup))
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    create_private_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(invalid(format!(
                "package contains symlink {}",
                entry.path().display()
            )));
        } else if file_type.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target)?;
        } else {
            return Err(invalid("package contains a non-file entry"));
        }
    }
    Ok(())
}

fn digest_tree(root: &Path) -> io::Result<String> {
    fn visit(root: &Path, current: &Path, hasher: &mut Sha256) -> io::Result<()> {
        let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if entry.file_name() == ".git" {
                continue;
            }
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err(invalid("package digests do not follow symlinks"));
            }
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            hasher.update((relative.len() as u64).to_be_bytes());
            hasher.update(relative.as_bytes());
            if ty.is_dir() {
                hasher.update(b"d");
                visit(root, &entry.path(), hasher)?;
            } else if ty.is_file() {
                hasher.update(b"f");
                let bytes = fs::read(entry.path())?;
                hasher.update((bytes.len() as u64).to_be_bytes());
                hasher.update(bytes);
            }
        }
        Ok(())
    }
    let mut hasher = Sha256::new();
    visit(root, root, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

fn digest_file(path: &Path) -> io::Result<String> {
    Ok(hex(&Sha256::digest(fs::read(path)?)))
}

fn detect_dependency_cycles(registry: &Registry) -> io::Result<()> {
    fn visit(
        id: &str,
        registry: &Registry,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        depth: usize,
    ) -> io::Result<()> {
        if depth > 8 {
            return Err(invalid("dependency graph exceeds depth 8"));
        }
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id.to_owned()) {
            return Err(invalid(format!("dependency cycle at `{id}`")));
        }
        let entry = &registry.plugins[id];
        let loaded = load_package(&entry.root)?;
        for dependency in &loaded.manifest.dependencies {
            visit(&dependency.id, registry, visiting, visited, depth + 1)?;
        }
        visiting.remove(id);
        visited.insert(id.to_owned());
        Ok(())
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for id in registry.plugins.keys() {
        visit(id, registry, &mut visiting, &mut visited, 1)?;
    }
    Ok(())
}

fn validate_git_ref(value: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > 256 || value.starts_with('-') || value.contains('\0') {
        Err(invalid("invalid Git ref"))
    } else {
        Ok(())
    }
}

fn safe_registry_root(packages: &Path, root: &Path, linked: bool) -> bool {
    linked
        || root.parent() == Some(packages)
            && root
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("p-"))
}

fn remove_known_tree(packages: &Path, path: &Path) -> io::Result<()> {
    if path.parent() != Some(packages) || path.file_name().is_none() {
        return Err(invalid(
            "refusing to remove a path outside the plugin package directory",
        ));
    }
    fs::remove_dir_all(path)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_new_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!(
        "{}.{}.tmp",
        path.extension()
            .and_then(|value| value.to_str())
            .unwrap_or("file"),
        std::process::id()
    ));
    let mut file = private_new_file(&temporary)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if result.is_ok() {
        crate::runtime::atomic_replace(&temporary, path)
    } else {
        let _ = fs::remove_file(&temporary);
        result
    }
}

fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

fn print_json(value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(io::stdout().lock(), value).map_err(io::Error::other)?;
    println!();
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(TABLE[(byte >> 4) as usize] as char);
        result.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    result
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn not_found(id: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("plugin_not_found: `{id}` is not installed"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_package(root: &Path) {
        fs::create_dir_all(root.join("schemas")).unwrap();
        fs::write(
            root.join("vvmux-plugin.toml"),
            r#"
manifest_version = 1
[plugin]
id = "dev.example"
name = "Example"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "test"
platforms = ["linux", "macos", "windows"]
permissions = ["pane.read"]
[[actions]]
id = "echo"
title = "Echo"
description = "echo"
command = ["echo"]
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
agent_visible = true
"#,
        )
        .unwrap();
        fs::write(
            root.join("schemas/input.json"),
            r#"{"type":"object","additionalProperties":false}"#,
        )
        .unwrap();
        fs::write(root.join("schemas/output.json"), r#"{"type":"object"}"#).unwrap();
    }

    fn write_named_test_package(root: &Path, id: &str, version: &str) {
        write_test_package(root);
        let manifest = fs::read_to_string(root.join("vvmux-plugin.toml"))
            .unwrap()
            .replace("dev.example", id)
            .replace("version = \"1.0.0\"", &format!("version = \"{version}\""));
        fs::write(root.join("vvmux-plugin.toml"), manifest).unwrap();
    }

    fn test_registry_entry(root: &Path, id: &str, version: &str) -> RegistryEntry {
        RegistryEntry {
            id: id.into(),
            version: version.into(),
            root: root.into(),
            source: format!("https://example.invalid/{id}"),
            commit: Some("a".repeat(40)),
            digest: digest_tree(root).unwrap(),
            manifest_digest: digest_file(&root.join("vvmux-plugin.toml")).unwrap(),
            enabled: true,
            linked: false,
            runtime_tier: "trusted_native".into(),
            permissions: vec!["pane.read".into()],
        }
    }

    #[test]
    fn package_paths_are_collision_resistant_and_windows_safe() {
        let paths = PluginPaths {
            root: "/tmp/plugins".into(),
            registry: "/tmp/plugins/registry.json".into(),
            packages: "/tmp/plugins/packages".into(),
            lock: "/tmp/plugins/lock".into(),
        };
        let first = paths.package("dev.one");
        let second = paths.package("dev.two");
        assert_ne!(first, second);
        let name = first.file_name().unwrap().to_string_lossy();
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
        let version = paths.package_version("dev.one", &"a".repeat(64));
        assert!(
            version
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(&"a".repeat(16))
        );
    }

    #[test]
    fn capped_reader_reports_truncation() {
        let output = read_capped(&b"abcdef"[..], 3).unwrap();
        assert_eq!(output.bytes, b"abc");
        assert!(output.truncated);
    }

    #[test]
    fn crash_backoff_is_capped_and_exponential() {
        assert_eq!(crash_backoff(1), Duration::from_millis(100));
        assert_eq!(crash_backoff(2), Duration::from_millis(200));
        assert_eq!(crash_backoff(9), Duration::from_millis(25_600));
        assert_eq!(crash_backoff(u32::MAX), Duration::from_millis(25_600));
    }

    #[test]
    fn detached_job_ids_route_to_their_exact_session() {
        let id = "work/0123456789abcdef0123456789abcdef-0000000000000001";
        assert_eq!(job_target(id).unwrap(), "work");
        assert!(job_target("missing-session-component").is_err());
        assert!(job_target("../0123456789abcdef0123456789abcdef-0000000000000001").is_err());
        assert!(job_target("work/").is_err());
        assert!(job_target("work/0123456789abcdef-0000000000000001").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn one_shot_actions_receive_no_broker_authority() {
        let output = run_one_shot(
            Path::new("/"),
            &[
                "sh".into(),
                "-c".into(),
                "if test -n \"$VVMUX_PLUGIN_BROKER_TOKEN\"; then printf '{\"token\":true}'; else printf '{\"token\":false}'; fi".into(),
            ],
            &serde_json::json!({}),
            Duration::from_secs(5),
            OneShotContext {
                session: Some("test"),
                plugin_id: "dev.example",
                cancel: None,
                session_instance: Some("session-instance"),
            },
        )
        .unwrap();
        assert_eq!(output, serde_json::json!({"token": false}));
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_one_shot_terminates_its_process_group() {
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let setter = cancelled.clone();
        let cancel_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            setter.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = run_one_shot(
            Path::new("/"),
            &["sh".into(), "-c".into(), "sleep 30".into()],
            &serde_json::json!({}),
            Duration::from_secs(30),
            OneShotContext {
                session: Some("test"),
                plugin_id: "dev.example",
                cancel: Some(cancelled.as_ref()),
                session_instance: Some("session-instance"),
            },
        );
        cancel_thread.join().unwrap();
        assert!(result.unwrap_err().to_string().starts_with("cancelled:"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn safe_removal_requires_a_direct_package_child() {
        let root = Path::new("/config/plugins/packages");
        assert!(!safe_registry_root(
            root,
            Path::new("/elsewhere/p-x"),
            false
        ));
        assert!(safe_registry_root(
            root,
            Path::new("/config/plugins/packages/p-x"),
            false
        ));
    }

    #[test]
    fn local_install_publishes_a_digest_checked_registry_entry() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        write_test_package(&source);
        let root = directory.path().join("config/plugins");
        let paths = PluginPaths {
            registry: root.join("registry.json"),
            packages: root.join("packages"),
            lock: root.join("vvmux-plugin.lock"),
            root,
        };
        install_local(&paths, &source, false, true, None, None).unwrap();
        let registry = load_registry(&paths).unwrap();
        let entry = &registry.plugins["dev.example"];
        assert!(entry.enabled);
        assert_eq!(entry.runtime_tier, "trusted_native");
        assert_eq!(digest_tree(&entry.root).unwrap(), entry.digest);
        resolve(&paths, false).unwrap();
        assert!(paths.lock.is_file());
    }

    #[test]
    fn approval_delta_identifies_only_security_relevant_increases() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("package");
        write_test_package(&package);
        let loaded = load_package(&package).unwrap();
        let mut previous = test_registry_entry(&package, "dev.example", "1.0.0");
        previous.source = "https://old.invalid/example".into();
        previous.runtime_tier = "workflow".into();
        previous.permissions.clear();

        let delta = approval_delta(Some(&previous), &loaded, "https://new.invalid/example");
        assert_eq!(
            delta.source_change,
            Some((
                "https://old.invalid/example".into(),
                "https://new.invalid/example".into()
            ))
        );
        assert_eq!(
            delta.runtime_tier_change,
            Some(("workflow".into(), "trusted_native".into()))
        );
        assert_eq!(delta.added_permissions, vec!["pane.read"]);
        assert!(delta.requires_fresh_approval());

        let mut trusted = test_registry_entry(&package, "dev.example", "1.0.0");
        trusted.permissions.push("pane.write".into());
        let reduction = approval_delta(Some(&trusted), &loaded, &trusted.source);
        assert_eq!(reduction, ApprovalDelta::default());
        assert!(!reduction.requires_fresh_approval());
    }

    #[test]
    fn failed_live_reload_publishes_a_new_rollback_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("config/plugins");
        let paths = PluginPaths {
            registry: root.join("registry.json"),
            packages: root.join("packages"),
            lock: root.join("vvmux-plugin.lock"),
            root,
        };
        let package = paths.packages.join("p-example");
        write_test_package(&package);
        let previous = Registry::default();
        let mut next = previous.clone();
        next.plugins.insert(
            "dev.example".into(),
            RegistryEntry {
                id: "dev.example".into(),
                version: "1.0.0".into(),
                root: package.clone(),
                source: "test".into(),
                commit: None,
                digest: digest_tree(&package).unwrap(),
                manifest_digest: digest_file(&package.join("vvmux-plugin.toml")).unwrap(),
                enabled: true,
                linked: false,
                runtime_tier: "trusted_native".into(),
                permissions: Vec::new(),
            },
        );
        let mut reloads = 0;
        let error = commit_registry_with_reload(&paths, &previous, &mut next, || {
            reloads += 1;
            if reloads == 1 {
                let published = load_registry(&paths).unwrap();
                assert_eq!(published.generation, 1);
                assert!(published.plugins.contains_key("dev.example"));
                Err(io::Error::other("session rejected generation"))
            } else {
                let restored = load_registry(&paths).unwrap();
                assert_eq!(restored.generation, 2);
                assert!(restored.plugins.is_empty());
                Ok(())
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("rolled back at generation 2"));
        let restored = load_registry(&paths).unwrap();
        assert_eq!(restored.generation, 2);
        assert!(restored.plugins.is_empty());
        assert_eq!(reloads, 2);
    }

    #[test]
    fn failed_multi_package_update_restores_the_complete_graph_and_lock() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("config/plugins");
        let paths = PluginPaths {
            registry: root.join("registry.json"),
            packages: root.join("packages"),
            lock: root.join("vvmux-plugin.lock"),
            root,
        };
        paths.ensure().unwrap();
        let old_one = paths.packages.join("p-one-old");
        let old_two = paths.packages.join("p-two-old");
        let new_one = paths.packages.join("p-one-new");
        let new_two = paths.packages.join("p-two-new");
        write_named_test_package(&old_one, "dev.one", "1.0.0");
        write_named_test_package(&old_two, "dev.two", "1.0.0");
        write_named_test_package(&new_one, "dev.one", "2.0.0");
        write_named_test_package(&new_two, "dev.two", "2.0.0");

        let mut previous = Registry::default();
        previous.plugins.insert(
            "dev.one".into(),
            test_registry_entry(&old_one, "dev.one", "1.0.0"),
        );
        previous.plugins.insert(
            "dev.two".into(),
            test_registry_entry(&old_two, "dev.two", "1.0.0"),
        );
        save_registry(&paths, &mut previous).unwrap();
        let old_lock = encode_lock(&previous).unwrap();
        write_private_atomic(&paths.lock, old_lock.as_bytes()).unwrap();

        let mut next = previous.clone();
        next.plugins.insert(
            "dev.one".into(),
            test_registry_entry(&new_one, "dev.one", "2.0.0"),
        );
        next.plugins.insert(
            "dev.two".into(),
            test_registry_entry(&new_two, "dev.two", "2.0.0"),
        );
        let mut reloads = 0;
        let error = commit_registry_with_reload(&paths, &previous, &mut next, || {
            reloads += 1;
            let published = load_registry(&paths).unwrap();
            if reloads == 1 {
                assert_eq!(published.plugins["dev.one"].version, "2.0.0");
                assert_eq!(published.plugins["dev.two"].version, "2.0.0");
                Err(io::Error::other("second session rejected graph"))
            } else {
                assert_eq!(published.plugins["dev.one"].version, "1.0.0");
                assert_eq!(published.plugins["dev.two"].version, "1.0.0");
                Ok(())
            }
        })
        .unwrap_err();

        assert!(error.to_string().contains("rolled back at generation 3"));
        assert_eq!(reloads, 2);
        let restored = load_registry(&paths).unwrap();
        assert_eq!(restored.generation, 3);
        assert_eq!(restored.plugins, previous.plugins);
        assert_eq!(fs::read_to_string(&paths.lock).unwrap(), old_lock);
    }
}
