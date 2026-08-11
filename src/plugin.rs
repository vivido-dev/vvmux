use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use vvmux_plugin_api::{
    ErrorCode, Hello, HostCallResult, Invocation, InvocationContext, LoadedManifest, NativeMessage,
    NativeReply, PROTOCOL_VERSION, Permission, PluginError, RuntimeKind, read_frame, write_frame,
};

const REGISTRY_SCHEMA: u16 = 1;
const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;
const MAX_ACTION_OUTPUT: usize = 1024 * 1024;
const MAX_LOG_OUTPUT: usize = 256 * 1024;

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
    /// Verify dependency constraints and write the reproducible lock.
    Resolve {
        #[arg(long)]
        frozen: bool,
    },
}

#[derive(Debug, Args)]
pub struct CatalogArgs {
    #[arg(long)]
    target: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub struct InvokeArgs {
    /// Plugin and action as ID/ACTION.
    reference: String,
    #[arg(long)]
    target: Option<String>,
    /// JSON, @FILE, or - for stdin.
    #[arg(long, default_value = "{}")]
    input: String,
    #[arg(long)]
    detach: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    schema: u16,
    #[serde(default)]
    plugins: BTreeMap<String, RegistryEntry>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema: REGISTRY_SCHEMA,
            plugins: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

pub fn run(command: PluginCommand) -> io::Result<()> {
    let paths = PluginPaths::new()?;
    match command {
        PluginCommand::Link { path, yes } => install_local(&paths, &path, true, yes, None),
        PluginCommand::Install {
            source,
            git_ref,
            yes,
        } => {
            if source.starts_with("https://") {
                install_git(&paths, &source, git_ref.as_deref(), yes)
            } else {
                install_local(&paths, Path::new(&source), false, yes, None)
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

fn save_registry(paths: &PluginPaths, registry: &Registry) -> io::Result<()> {
    paths.ensure()?;
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

fn install_local(
    paths: &PluginPaths,
    source: &Path,
    linked: bool,
    yes: bool,
    recorded_source: Option<String>,
) -> io::Result<()> {
    let source = fs::canonicalize(source)?;
    let loaded = load_package(&source)?;
    if !loaded
        .manifest
        .plugin
        .platforms
        .iter()
        .any(|platform| platform == current_platform())
    {
        return Err(invalid(format!(
            "plugin `{}` does not support {}",
            loaded.manifest.plugin.id,
            current_platform()
        )));
    }
    preview(
        &loaded,
        if linked {
            "linked native/user package"
        } else {
            "local package"
        },
    );
    confirm_if_needed(&loaded, yes)?;
    paths.ensure()?;

    let id = loaded.manifest.plugin.id.clone();
    let manifest_before = digest_file(&source.join("vvmux-plugin.toml"))?;
    let mut registry = load_registry(paths)?;
    registry.plugins.insert(
        id.clone(),
        entry_for(
            &loaded,
            source.clone(),
            recorded_source
                .clone()
                .unwrap_or_else(|| source.to_string_lossy().into_owned()),
            None,
            digest_tree(&source)?,
            manifest_before.clone(),
            true,
        ),
    );
    validate_dependency_graph(&registry)?;
    let (root, digest, backup) = if linked {
        (source.clone(), digest_tree(&source)?, None)
    } else {
        let destination = paths.package(&id);
        let staging = paths.packages.join(format!(
            ".staging-{}-{}",
            std::process::id(),
            &manifest_before[..16]
        ));
        if staging.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "plugin staging path exists",
            ));
        }
        copy_tree(&source, &staging)?;
        let loaded_after = load_package(&staging)?;
        let manifest_after = digest_file(&staging.join("vvmux-plugin.toml"))?;
        if loaded_after.manifest.plugin.id != id || manifest_after != manifest_before {
            remove_known_tree(&paths.packages, &staging)?;
            return Err(invalid("manifest changed while the package was staged"));
        }
        let digest = digest_tree(&staging)?;
        let backup = atomic_package_swap(&destination, &staging)?;
        (destination, digest, backup)
    };

    let entry = entry_for(
        &loaded,
        root.clone(),
        recorded_source.unwrap_or_else(|| source.to_string_lossy().into_owned()),
        None,
        digest,
        manifest_before,
        linked,
    );
    registry.plugins.insert(id.clone(), entry);
    if let Err(error) = save_registry(paths, &registry) {
        if !linked {
            let _ = remove_known_tree(&paths.packages, &root);
            if let Some(backup) = &backup {
                let _ = fs::rename(backup, &root);
            }
        }
        return Err(error);
    }
    if let Some(backup) = backup {
        remove_known_tree(&paths.packages, &backup)?;
    }
    println!(
        "{} plugin {id}",
        if linked { "linked" } else { "installed" }
    );
    Ok(())
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
    let result = install_local(paths, &checkout, false, yes, Some(source.to_owned()));
    let _ = remove_known_tree(&paths.packages, &checkout);
    if result.is_ok() {
        let mut registry = load_registry(paths)?;
        if let Some(entry) = registry
            .plugins
            .values_mut()
            .find(|entry| entry.source == source)
        {
            entry.commit = Some(commit);
        }
        save_registry(paths, &registry)?;
    }
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
        )
    }
}

fn uninstall(paths: &PluginPaths, id: &str) -> io::Result<()> {
    let mut registry = load_registry(paths)?;
    let entry = registry.plugins.remove(id).ok_or_else(|| not_found(id))?;
    save_registry(paths, &registry)?;
    if !entry.linked && entry.root.exists() {
        remove_known_tree(&paths.packages, &entry.root)?;
    }
    println!("uninstalled plugin {id}");
    Ok(())
}

fn set_enabled(paths: &PluginPaths, id: &str, enabled: bool) -> io::Result<()> {
    let mut registry = load_registry(paths)?;
    let entry = registry.plugins.get_mut(id).ok_or_else(|| not_found(id))?;
    entry.enabled = enabled;
    save_registry(paths, &registry)?;
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

fn catalog(paths: &PluginPaths, args: CatalogArgs) -> io::Result<()> {
    if let Some(target) = &args.target {
        crate::runtime::validate_session_name(target)?;
    }
    let registry = load_registry(paths)?;
    let mut actions = Vec::new();
    for entry in registry.plugins.values().filter(|entry| entry.enabled) {
        let loaded = load_package(&entry.root)?;
        for action in loaded
            .manifest
            .actions
            .iter()
            .filter(|action| action.agent_visible)
        {
            actions.push(serde_json::json!({
                "reference": format!("{}/{}", entry.id, action.id),
                "title": action.title,
                "description": action.description,
                "input_schema": loaded.schemas[&action.input_schema].value,
                "output_schema": loaded.schemas[&action.output_schema].value,
                "permissions": entry.permissions,
                "runtime_tier": entry.runtime_tier,
                "source": entry.source,
                "digest": entry.digest,
                "timeout_ms": action.timeout_ms,
            }));
        }
        for workflow in loaded
            .manifest
            .workflows
            .iter()
            .filter(|workflow| workflow.agent_visible)
        {
            actions.push(serde_json::json!({
                "reference": format!("{}/{}", entry.id, workflow.id),
                "title": workflow.title,
                "description": "manifest workflow",
                "runtime_tier": "workflow",
                "source": entry.source,
                "digest": entry.digest,
            }));
        }
    }
    if args.json {
        print_json(&serde_json::json!({"target": args.target, "actions": actions}))
    } else {
        for action in actions {
            println!(
                "{}\t{}",
                action["reference"].as_str().unwrap(),
                action["title"].as_str().unwrap()
            );
        }
        Ok(())
    }
}

fn invoke(paths: &PluginPaths, args: InvokeArgs) -> io::Result<()> {
    if args.detach {
        return Err(invalid(
            "detached jobs require a live session plugin supervisor",
        ));
    }
    let input = read_json_input(&args.input)?;
    let output = if let Some(target) = args.target.as_deref() {
        invoke_via_session(target, args.reference, input)?
    } else {
        invoke_reference(paths, &args.reference, None, input)?
    };
    print_json(&output)
}

fn invoke_via_session(target: &str, reference: String, input: Value) -> io::Result<Value> {
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
                detach: false,
            }),
        }))?;
    crate::automation::response_result(crate::automation::receive_response(&mut reader, 1)?)
}

fn invoke_reference(
    paths: &PluginPaths,
    reference: &str,
    target: Option<&str>,
    input: Value,
) -> io::Result<Value> {
    let (plugin_id, action_id) = reference
        .split_once('/')
        .ok_or_else(|| invalid("plugin reference must be ID/ACTION"))?;
    if let Some(target) = target {
        crate::runtime::validate_session_name(target)?;
    }
    let registry = load_registry(paths)?;
    let entry = registry
        .plugins
        .get(plugin_id)
        .ok_or_else(|| not_found(plugin_id))?;
    if !entry.enabled {
        return Err(invalid("plugin_disabled: plugin is disabled"));
    }
    let loaded = load_package(&entry.root)?;
    let action = loaded
        .action(action_id)
        .ok_or_else(|| invalid("action_not_found: action does not exist"))?;
    loaded
        .validate_input(action, &input)
        .map_err(|errors| invalid(format!("schema_invalid: {}", errors.join("; "))))?;
    let output = if let Some(argv) = action.command.as_deref() {
        run_one_shot(
            &loaded.root,
            argv,
            &input,
            Duration::from_millis(action.timeout_ms),
            OneShotContext {
                session: target,
                plugin_id,
                cancel: None,
                session_instance: None,
                broker: None,
                permissions: &[],
            },
        )?
    } else if let (Some(handler), Some(runtime)) =
        (action.handler.as_deref(), loaded.manifest.runtime.as_ref())
    {
        match runtime.kind {
            RuntimeKind::Process => run_native_service(
                &loaded.root,
                runtime.command.as_deref().unwrap(),
                plugin_id,
                handler,
                input,
                Duration::from_millis(action.timeout_ms),
                target,
            )?,
            RuntimeKind::Component => {
                return Err(invalid(
                    "runtime_unavailable: component execution is not enabled in this build",
                ));
            }
        }
    } else {
        return Err(invalid(
            "runtime_unavailable: action has no executable runtime",
        ));
    };
    loaded
        .validate_output(action, &output)
        .map_err(|errors| invalid(format!("output_invalid: {}", errors.join("; "))))?;
    Ok(output)
}

pub(crate) struct SessionPluginRuntime {
    paths: PluginPaths,
    session_name: String,
    session_instance: String,
    plugin_id: String,
    broker: crate::plugin_supervisor::HostBroker,
    service: Option<(String, NativeService)>,
    consecutive_crashes: u32,
    retry_at: Option<Instant>,
}

impl SessionPluginRuntime {
    pub(crate) fn new(
        session_name: String,
        session_instance: String,
        plugin_id: String,
        broker: crate::plugin_supervisor::HostBroker,
    ) -> io::Result<Self> {
        Ok(Self {
            paths: PluginPaths::new()?,
            session_name,
            session_instance,
            plugin_id,
            broker,
            service: None,
            consecutive_crashes: 0,
            retry_at: None,
        })
    }

    pub(crate) fn invoke(
        &mut self,
        reference: &str,
        input: Value,
        cancel: &AtomicBool,
    ) -> io::Result<Value> {
        if cancel.load(Ordering::Acquire) {
            return Err(invalid("cancelled: plugin invocation was cancelled"));
        }
        let (plugin_id, action_id) = reference
            .split_once('/')
            .ok_or_else(|| invalid("action_not_found: plugin reference must be ID/ACTION"))?;
        if plugin_id != self.plugin_id {
            return Err(invalid("scope_denied: plugin worker identity mismatch"));
        }
        crate::runtime::validate_session_name(&self.session_name)?;
        let registry = load_registry(&self.paths)?;
        let entry = registry
            .plugins
            .get(plugin_id)
            .ok_or_else(|| not_found(plugin_id))?;
        if !entry.enabled {
            self.service.take();
            return Err(invalid("plugin_disabled: plugin is disabled"));
        }
        let loaded = load_package(&entry.root)?;
        let action = loaded
            .action(action_id)
            .cloned()
            .ok_or_else(|| invalid("action_not_found: action does not exist"))?;
        loaded
            .validate_input(&action, &input)
            .map_err(|errors| invalid(format!("schema_invalid: {}", errors.join("; "))))?;
        let timeout = Duration::from_millis(action.timeout_ms);
        let output = if let Some(argv) = action.command.as_deref() {
            run_one_shot(
                &loaded.root,
                argv,
                &input,
                timeout,
                OneShotContext {
                    session: Some(&self.session_name),
                    plugin_id,
                    cancel: Some(cancel),
                    session_instance: Some(&self.session_instance),
                    broker: Some(&self.broker),
                    permissions: &loaded.manifest.plugin.permissions,
                },
            )?
        } else if let (Some(handler), Some(runtime)) =
            (action.handler.as_deref(), loaded.manifest.runtime.as_ref())
        {
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
                    let artifact_key = format!("{}:{}", entry.digest, entry.manifest_digest);
                    if self
                        .service
                        .as_ref()
                        .is_some_and(|(key, _)| key != &artifact_key)
                    {
                        self.service.take();
                    }
                    if self.service.is_none() {
                        match NativeService::start(
                            &loaded.root,
                            runtime.command.as_deref().unwrap(),
                            timeout,
                            NativeServiceContext {
                                plugin_id,
                                session: Some(&self.session_name),
                                session_instance: &self.session_instance,
                                broker: Some(&self.broker),
                                permissions: &loaded.manifest.plugin.permissions,
                            },
                        ) {
                            Ok(service) => self.service = Some((artifact_key, service)),
                            Err(error) => {
                                self.note_crash();
                                return Err(error);
                            }
                        }
                    }
                    let result = self.service.as_mut().unwrap().1.invoke(
                        handler,
                        input,
                        timeout,
                        &self.session_instance,
                        Some(cancel),
                    );
                    if self.service.as_ref().unwrap().1.healthy {
                        self.consecutive_crashes = 0;
                        self.retry_at = None;
                    } else {
                        self.service.take();
                        self.note_crash();
                    }
                    result?
                }
                RuntimeKind::Component => {
                    return Err(invalid(
                        "runtime_unavailable: component execution is not enabled in this build",
                    ));
                }
            }
        } else {
            return Err(invalid(
                "runtime_unavailable: action has no executable runtime",
            ));
        };
        loaded
            .validate_output(&action, &output)
            .map_err(|errors| invalid(format!("output_invalid: {}", errors.join("; "))))?;
        Ok(output)
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

fn run_native_service(
    root: &Path,
    argv: &[String],
    plugin_id: &str,
    handler: &str,
    input: Value,
    timeout: Duration,
    session: Option<&str>,
) -> io::Result<Value> {
    let session_instance = session.unwrap_or("direct");
    let mut service = NativeService::start(
        root,
        argv,
        timeout,
        NativeServiceContext {
            plugin_id,
            session,
            session_instance,
            broker: None,
            permissions: &[],
        },
    )?;
    service.invoke(handler, input, timeout, session_instance, None)
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
                let frame = read_frame::<NativeReply>(&mut stdout)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()));
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
        session_instance: &str,
        cancel: Option<&AtomicBool>,
    ) -> io::Result<Value> {
        if !self.healthy || self.child.try_wait()?.is_some() {
            self.healthy = false;
            return Err(invalid("runtime_crashed: native plugin process exited"));
        }
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(2);
        let correlation_id = random_id()?;
        let deadline_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_add(timeout.as_millis())
            .min(u128::from(u64::MAX)) as u64;
        self.write(&NativeMessage::Invoke(Invocation {
            request_id,
            action: handler.to_owned(),
            input,
            context: InvocationContext {
                correlation_id: correlation_id.clone(),
                causation_id: correlation_id,
                causation_depth: 0,
                source: "automation".into(),
                session_instance: session_instance.to_owned(),
                pane_id: None,
                tab_id: None,
                deadline_unix_ms,
            },
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

fn random_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(hex(&bytes))
}

fn resolve(paths: &PluginPaths, frozen: bool) -> io::Result<()> {
    let registry = load_registry(paths)?;
    validate_dependency_graph(&registry)?;
    let lock = LockFile {
        lock_version: 1,
        packages: registry
            .plugins
            .values()
            .map(|entry| LockedPackage {
                id: entry.id.clone(),
                version: entry.version.clone(),
                source: entry.source.clone(),
                commit: entry.commit.clone(),
                manifest_digest: entry.manifest_digest.clone(),
                artifact_digest: entry.digest.clone(),
            })
            .collect(),
    };
    let encoded = toml::to_string_pretty(&lock).map_err(io::Error::other)?;
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

fn load_package(root: &Path) -> io::Result<LoadedManifest> {
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
}

fn confirm_if_needed(loaded: &LoadedManifest, yes: bool) -> io::Result<()> {
    if yes {
        return Ok(());
    }
    eprint!("install {}? [y/N] ", loaded.manifest.plugin.id);
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
        permissions: loaded
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
            .collect(),
    }
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
    broker: Option<&'a crate::plugin_supervisor::HostBroker>,
    permissions: &'a [Permission],
}

fn run_one_shot(
    root: &Path,
    argv: &[String],
    input: &Value,
    timeout: Duration,
    context: OneShotContext<'_>,
) -> io::Result<Value> {
    let instance_id = random_id()?;
    let broker_lease = context
        .broker
        .map(|broker| broker.issue(context.plugin_id, &instance_id, context.permissions))
        .transpose()?;
    let mut command = trusted_command(root, argv, context.session, context.plugin_id);
    command.env("VVMUX_PLUGIN_INSTANCE", &instance_id);
    if let Some(session_instance) = context.session_instance {
        command.env("VVMUX_SESSION_INSTANCE", session_instance);
    }
    if let Some(lease) = &broker_lease {
        command.env("VVMUX_PLUGIN_BROKER_TOKEN", lease.token());
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
                broker: None,
                permissions: &[],
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
        install_local(&paths, &source, false, true, None).unwrap();
        let registry = load_registry(&paths).unwrap();
        let entry = &registry.plugins["dev.example"];
        assert!(entry.enabled);
        assert_eq!(entry.runtime_tier, "trusted_native");
        assert_eq!(digest_tree(&entry.root).unwrap(), entry.digest);
        resolve(&paths, false).unwrap();
        assert!(paths.lock.is_file());
    }
}
