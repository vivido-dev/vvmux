//! Sandboxed WebAssembly Component runtime for session plugins.

use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use vvmux_plugin_api::{ComponentPreopen, HostCall, Permission};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{
    Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline,
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::plugin_supervisor::{BrokerLease, HostBroker};

const MAX_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const MAX_TABLE_ELEMENTS: usize = 100_000;
const MAX_STORAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LOG_BYTES: usize = 256 * 1024;
// Epoch interruption is the primary deadline/cancellation mechanism. Keep a generous independent
// fuel ceiling as a deterministic backstop without letting a valid call exhaust before the
// supervisor can deliver a cancellation from another process.
const FUEL_PER_CALL: u64 = 1_000_000_000;
const EPOCH_TICK: Duration = Duration::from_millis(10);
const CACHE_MAGIC: &[u8; 8] = b"VVCWASM1";
const CACHE_KEY_VERSION: &str = "vvmux-component-cache-v1";

mod bindings {
    wasmtime::component::bindgen!({
        path: "vvmux-plugin-api/wit",
        world: "plugin",
    });
}

pub(crate) struct ComponentRuntime {
    store: Store<ComponentState>,
    guest: bindings::Plugin,
}

struct ComponentState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: ComponentLimits,
    broker: Option<BrokerLease>,
    storage: PathBuf,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    invocation_logs: String,
    logs_truncated: bool,
}

impl WasiView for ComponentState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl bindings::vivido::vvmux_plugin::host::Host for ComponentState {
    fn call(
        &mut self,
        method: String,
        params_json: Vec<u8>,
    ) -> Result<Vec<u8>, bindings::vivido::vvmux_plugin::host::PluginError> {
        if params_json.len() > MAX_PAYLOAD_BYTES {
            return Err(plugin_error(
                "schema_invalid",
                "host-call payload exceeds 1 MiB",
            ));
        }
        let Some(lease) = self.broker.as_ref() else {
            return Err(plugin_error(
                "capability_denied",
                "component has no session broker",
            ));
        };
        let params = match serde_json::from_slice(&params_json) {
            Ok(params) => params,
            Err(error) => return Err(plugin_error("schema_invalid", &error.to_string())),
        };
        let call = HostCall {
            request_id: 0,
            method,
            params,
        };
        match lease.call(call, self.deadline) {
            Ok(result) => match serde_json::to_vec(&result) {
                Ok(bytes) if bytes.len() <= MAX_PAYLOAD_BYTES => Ok(bytes),
                Ok(_) => Err(plugin_error("output_invalid", "host result exceeds 1 MiB")),
                Err(error) => Err(plugin_error("output_invalid", &error.to_string())),
            },
            Err(error) => Err(io_plugin_error(&error)),
        }
    }

    fn log(&mut self, level: String, message: String) {
        if self.invocation_logs.len() >= MAX_LOG_BYTES {
            self.logs_truncated = true;
            return;
        }
        let level = match level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => level,
            _ => "info".to_owned(),
        };
        let mut safe = message.replace(['\r', '\n'], " ");
        let mut entry = component_log_entry(&level, &safe, false);
        entry.push('\n');
        let remaining = MAX_LOG_BYTES - self.invocation_logs.len();
        if entry.len() > remaining {
            self.logs_truncated = true;
            loop {
                entry = component_log_entry(&level, &safe, true);
                entry.push('\n');
                if entry.len() <= remaining {
                    break;
                }
                if safe.is_empty() {
                    return;
                }
                let excess = entry.len() - remaining;
                let mut end = safe.len().saturating_sub(excess.max(1));
                while !safe.is_char_boundary(end) {
                    end -= 1;
                }
                safe.truncate(end);
            }
        }
        self.invocation_logs.push_str(&entry);
    }

    fn storage_get(
        &mut self,
        key: String,
    ) -> Result<Option<Vec<u8>>, bindings::vivido::vvmux_plugin::host::PluginError> {
        let path = match storage_path(&self.storage, &key) {
            Ok(path) => path,
            Err(error) => return Err(io_plugin_error(&error)),
        };
        match read_limited(
            &path,
            MAX_PAYLOAD_BYTES as u64,
            "output_invalid: stored value exceeds 1 MiB",
        ) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_plugin_error(&error)),
        }
    }

    fn storage_set(
        &mut self,
        key: String,
        value: Vec<u8>,
    ) -> Result<(), bindings::vivido::vvmux_plugin::host::PluginError> {
        if value.len() > MAX_PAYLOAD_BYTES {
            return Err(plugin_error("schema_invalid", "stored value exceeds 1 MiB"));
        }
        let path = match storage_path(&self.storage, &key) {
            Ok(path) => path,
            Err(error) => return Err(io_plugin_error(&error)),
        };
        if let Err(error) = enforce_storage_limit(&self.storage, &path, value.len() as u64)
            .and_then(|()| atomic_write(&path, &value))
        {
            return Err(io_plugin_error(&error));
        }
        Ok(())
    }
}

fn component_log_entry(level: &str, message: &str, truncated: bool) -> String {
    serde_json::to_string(&serde_json::json!({
        "runtime": "component",
        "level": level,
        "message": message,
        "truncated": truncated,
    }))
    .unwrap_or_else(|_| {
        r#"{"runtime":"component","level":"error","message":"log serialization failed","truncated":true}"#
            .to_owned()
    })
}

impl ComponentRuntime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        root: &Path,
        artifact: &Path,
        plugin_id: &str,
        session_instance: Option<&str>,
        broker: Option<&HostBroker>,
        permissions: &[Permission],
        preopens: &[ComponentPreopen],
        cancel: Arc<AtomicBool>,
        deadline: Instant,
    ) -> io::Result<Self> {
        let paths = ComponentPaths::new(plugin_id)?;
        paths.ensure()?;
        let artifact_path = root.join(artifact);
        let bytes = read_limited(
            &artifact_path,
            MAX_ARTIFACT_BYTES,
            "runtime_unavailable: component artifact exceeds 32 MiB",
        )?;
        let engine = component_engine()?;
        let component = load_component(engine, &paths.cache, &bytes)?;
        let mut linker = Linker::new(engine);
        bindings::Plugin::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
            &mut linker,
            |state| state,
        )
        .map_err(component_error)?;
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(component_error)?;

        let mut wasi = WasiCtxBuilder::new();
        wasi.allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false);
        for preopen in preopens {
            match preopen {
                ComponentPreopen::Package => {
                    wasi.preopened_dir(root, "/package", DirPerms::READ, FilePerms::READ)
                        .map_err(component_error)?;
                }
                ComponentPreopen::Config => {
                    wasi.preopened_dir(&paths.config, "/config", DirPerms::READ, FilePerms::READ)
                        .map_err(component_error)?;
                }
                ComponentPreopen::Data => {
                    wasi.preopened_dir(
                        &paths.data,
                        "/data",
                        DirPerms::READ | DirPerms::MUTATE,
                        FilePerms::READ | FilePerms::WRITE,
                    )
                    .map_err(component_error)?;
                }
            }
        }
        let instance_id = format!("component-{}", crate::plugin::random_id()?);
        let lease = match (broker, session_instance) {
            (Some(broker), Some(_)) => Some(broker.issue(plugin_id, &instance_id, permissions)?),
            _ => None,
        };
        let limits = ComponentLimits::new();
        let state = ComponentState {
            wasi: wasi.build(),
            table: ResourceTable::new(),
            limits,
            broker: lease,
            storage: paths.storage,
            deadline,
            cancel,
            invocation_logs: String::new(),
            logs_truncated: false,
        };
        let mut store = Store::new(engine, state);
        store.limiter(|state| &mut state.limits);
        configure_call(&mut store, deadline)?;
        let guest = bindings::Plugin::instantiate(&mut store, &component, &linker)
            .map_err(component_error)?;
        let context = serde_json::to_vec(&serde_json::json!({
            "plugin_id": plugin_id,
            "session_instance": session_instance,
            "instance_id": instance_id,
        }))
        .map_err(io::Error::other)?;
        guest
            .vivido_vvmux_plugin_guest()
            .call_initialize(&mut store, &context)
            .map_err(component_error)?
            .map_err(guest_error)?;
        Ok(Self { store, guest })
    }

    pub(crate) fn invoke(
        &mut self,
        action: &str,
        input: &Value,
        context: &Value,
        cancel: Arc<AtomicBool>,
        deadline: Instant,
    ) -> io::Result<Value> {
        self.store.data_mut().invocation_logs.clear();
        self.store.data_mut().logs_truncated = false;
        let input = serde_json::to_vec(input).map_err(io::Error::other)?;
        let context = serde_json::to_vec(context).map_err(io::Error::other)?;
        if input.len() > MAX_PAYLOAD_BYTES || context.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("schema_invalid: component request exceeds 1 MiB"));
        }
        self.store.data_mut().cancel = cancel;
        self.store.data_mut().deadline = deadline;
        configure_call(&mut self.store, deadline)?;
        let call = self.guest.vivido_vvmux_plugin_guest().call_invoke(
            &mut self.store,
            action,
            &input,
            &context,
        );
        let bytes = match call {
            Ok(result) => result.map_err(guest_error)?,
            Err(_error) if self.store.data().cancel.load(Ordering::Acquire) => {
                return Err(invalid("cancelled: component invocation was cancelled"));
            }
            Err(error) if Instant::now() >= self.store.data().deadline => {
                return Err(invalid(format!(
                    "timeout: component invocation expired: {error}"
                )));
            }
            Err(error) => return Err(component_trap(error)),
        };
        if self.store.data().cancel.load(Ordering::Acquire) {
            return Err(invalid("cancelled: component invocation was cancelled"));
        }
        if Instant::now() >= self.store.data().deadline {
            return Err(invalid("timeout: component invocation expired"));
        }
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("output_invalid: component result exceeds 1 MiB"));
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            invalid(format!(
                "output_invalid: component returned invalid JSON: {error}"
            ))
        })
    }

    pub(crate) fn on_event(
        &mut self,
        name: &str,
        payload: &Value,
        context: &vvmux_plugin_api::InvocationContext,
        cancel: Arc<AtomicBool>,
        deadline: Instant,
    ) -> io::Result<()> {
        self.store.data_mut().invocation_logs.clear();
        self.store.data_mut().logs_truncated = false;
        let payload = serde_json::to_vec(payload).map_err(io::Error::other)?;
        let _cause = self
            .store
            .data()
            .broker
            .as_ref()
            .map(|lease| lease.enter_event(context));
        let context = serde_json::to_vec(context).map_err(io::Error::other)?;
        if payload.len() > MAX_PAYLOAD_BYTES || context.len() > MAX_PAYLOAD_BYTES {
            return Err(invalid("schema_invalid: component event exceeds 1 MiB"));
        }
        self.store.data_mut().cancel = cancel;
        self.store.data_mut().deadline = deadline;
        configure_call(&mut self.store, deadline)?;
        let call = self.guest.vivido_vvmux_plugin_guest().call_on_event(
            &mut self.store,
            name,
            &payload,
            &context,
        );
        match call {
            Ok(result) => result.map_err(guest_error),
            Err(_error) if self.store.data().cancel.load(Ordering::Acquire) => {
                Err(invalid("cancelled: component event was cancelled"))
            }
            Err(error) if Instant::now() >= self.store.data().deadline => Err(invalid(format!(
                "timeout: component event expired: {error}"
            ))),
            Err(error) => Err(component_trap(error)),
        }
    }

    pub(crate) fn take_logs(&mut self) -> (String, bool) {
        let state = self.store.data_mut();
        (
            std::mem::take(&mut state.invocation_logs),
            std::mem::take(&mut state.logs_truncated),
        )
    }
}

impl Drop for ComponentRuntime {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        self.store.data_mut().cancel = Arc::new(AtomicBool::new(false));
        self.store.data_mut().deadline = deadline;
        let _ = configure_call(&mut self.store, deadline);
        let _ = self
            .guest
            .vivido_vvmux_plugin_guest()
            .call_shutdown(&mut self.store);
    }
}

fn configure_call(store: &mut Store<ComponentState>, deadline: Instant) -> io::Result<()> {
    store.set_fuel(FUEL_PER_CALL).map_err(component_error)?;
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|context| {
        let state = context.data();
        if state.cancel.load(Ordering::Acquire) || Instant::now() >= state.deadline {
            Err(wasmtime::Error::msg("component invocation interrupted"))
        } else {
            Ok(UpdateDeadline::Continue(1))
        }
    });
    if deadline <= Instant::now() {
        return Err(invalid("timeout: component deadline expired"));
    }
    Ok(())
}

fn component_engine() -> io::Result<&'static Engine> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    let engine = ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        config.epoch_interruption(true);
        Engine::new(&config).map_err(|error| error.to_string())
    });
    let engine = engine
        .as_ref()
        .map_err(|error| invalid(format!("runtime_unavailable: Wasmtime engine: {error}")))?;
    static TICKER: OnceLock<()> = OnceLock::new();
    static TICKER_ERROR: OnceLock<String> = OnceLock::new();
    TICKER.get_or_init(|| {
        let engine = engine.clone();
        if let Err(error) = thread::Builder::new()
            .name("vvmux-component-epoch".into())
            .spawn(move || {
                loop {
                    thread::sleep(EPOCH_TICK);
                    engine.increment_epoch();
                }
            })
        {
            let _ = TICKER_ERROR.set(error.to_string());
        }
    });
    if let Some(error) = TICKER_ERROR.get() {
        return Err(invalid(format!(
            "runtime_unavailable: component epoch ticker: {error}"
        )));
    }
    Ok(engine)
}

fn load_component(engine: &Engine, cache_dir: &Path, bytes: &[u8]) -> io::Result<Component> {
    let key = cache_key(engine, bytes);
    let cache_path = cache_dir.join(format!("{key}.cwasm"));
    if let Ok(serialized) = read_cache(&cache_path, &key) {
        // SAFETY: `read_cache` accepts only an artifact written by this cache format whose full
        // engine/config/source key and serialized-byte digest both match. Plugin cache directories
        // are private to the current OS user, the same authority that installed the source Wasm.
        if let Ok(component) = unsafe { Component::deserialize(engine, &serialized) } {
            return Ok(component);
        }
    }
    let component = Component::from_binary(engine, bytes).map_err(component_error)?;
    let serialized = component.serialize().map_err(component_error)?;
    write_cache(&cache_path, &key, &serialized)?;
    Ok(component)
}

fn cache_key(engine: &Engine, bytes: &[u8]) -> String {
    let mut compatibility = CompatibilityHasher(Sha256::new());
    engine
        .precompile_compatibility_hash()
        .hash(&mut compatibility);
    let mut digest = Sha256::new();
    digest.update(CACHE_KEY_VERSION);
    digest.update(env!("CARGO_PKG_VERSION"));
    digest.update("wasmtime-36.0.13");
    digest.update(std::env::consts::ARCH);
    digest.update(std::env::consts::OS);
    digest.update(compatibility.0.finalize());
    digest.update(Sha256::digest(bytes));
    hex(&digest.finalize())
}

struct CompatibilityHasher(Sha256);

impl Hasher for CompatibilityHasher {
    fn finish(&self) -> u64 {
        0
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

struct ComponentLimits {
    inner: StoreLimits,
    memory_bytes: usize,
    table_elements: usize,
    pending_memory: usize,
    pending_table: usize,
}

impl ComponentLimits {
    fn new() -> Self {
        Self {
            inner: StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY_BYTES)
                .table_elements(MAX_TABLE_ELEMENTS)
                // A component may contain adapter core instances; `ComponentRuntime` itself
                // still instantiates exactly one top-level component.
                .instances(32)
                .memories(4)
                .tables(4)
                .trap_on_grow_failure(true)
                .build(),
            memory_bytes: 0,
            table_elements: 0,
            pending_memory: 0,
            pending_table: 0,
        }
    }
}

impl ResourceLimiter for ComponentLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let delta = desired.saturating_sub(current);
        if self.memory_bytes.saturating_add(delta) > MAX_MEMORY_BYTES {
            return Err(wasmtime::Error::msg(
                "component aggregate linear memory exceeds 64 MiB",
            ));
        }
        let allowed = self.inner.memory_growing(current, desired, maximum)?;
        if allowed {
            self.memory_bytes = self.memory_bytes.saturating_add(delta);
            self.pending_memory = delta;
        }
        Ok(allowed)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.memory_bytes = self.memory_bytes.saturating_sub(self.pending_memory);
        self.pending_memory = 0;
        Err(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let delta = desired.saturating_sub(current);
        if self.table_elements.saturating_add(delta) > MAX_TABLE_ELEMENTS {
            return Err(wasmtime::Error::msg(
                "component aggregate table size exceeds 100000 elements",
            ));
        }
        let allowed = self.inner.table_growing(current, desired, maximum)?;
        if allowed {
            self.table_elements = self.table_elements.saturating_add(delta);
            self.pending_table = delta;
        }
        Ok(allowed)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.table_elements = self.table_elements.saturating_sub(self.pending_table);
        self.pending_table = 0;
        Err(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

fn read_cache(path: &Path, key: &str) -> io::Result<Vec<u8>> {
    let bytes = read_limited(
        path,
        MAX_CACHE_BYTES,
        "runtime_unavailable: component cache exceeds 256 MiB",
    )?;
    let header = CACHE_MAGIC.len() + 64 + 64;
    if bytes.len() < header
        || &bytes[..CACHE_MAGIC.len()] != CACHE_MAGIC
        || &bytes[CACHE_MAGIC.len()..CACHE_MAGIC.len() + 64] != key.as_bytes()
    {
        return Err(invalid("runtime_unavailable: component cache key mismatch"));
    }
    let expected = &bytes[CACHE_MAGIC.len() + 64..header];
    let payload = &bytes[header..];
    let actual = hex(&Sha256::digest(payload));
    if expected != actual.as_bytes() {
        return Err(invalid(
            "runtime_unavailable: component cache digest mismatch",
        ));
    }
    Ok(payload.to_vec())
}

fn write_cache(path: &Path, key: &str, serialized: &[u8]) -> io::Result<()> {
    if serialized.len() as u64 > MAX_CACHE_BYTES {
        return Err(invalid(
            "runtime_unavailable: compiled component exceeds 256 MiB",
        ));
    }
    let mut body = Vec::with_capacity(CACHE_MAGIC.len() + 128 + serialized.len());
    body.extend_from_slice(CACHE_MAGIC);
    body.extend_from_slice(key.as_bytes());
    body.extend_from_slice(hex(&Sha256::digest(serialized)).as_bytes());
    body.extend_from_slice(serialized);
    atomic_write(path, &body)
}

struct ComponentPaths {
    config: PathBuf,
    data: PathBuf,
    storage: PathBuf,
    cache: PathBuf,
}

impl ComponentPaths {
    fn new(plugin_id: &str) -> io::Result<Self> {
        let root = crate::config::config_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no user config directory"))?
            .join("plugins");
        let name = format!("p-{}", &hex(&Sha256::digest(plugin_id.as_bytes()))[..32]);
        Ok(Self {
            config: root.join("config").join(&name),
            data: root.join("data").join(&name),
            storage: root.join("data").join(&name).join("storage"),
            cache: root.join("cache").join("components"),
        })
    }

    fn ensure(&self) -> io::Result<()> {
        for path in [&self.config, &self.data, &self.storage, &self.cache] {
            fs::create_dir_all(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }
}

fn storage_path(root: &Path, key: &str) -> io::Result<PathBuf> {
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(invalid(
            "schema_invalid: storage key must be 1..128 printable characters",
        ));
    }
    Ok(root.join(format!("s-{}", hex(&Sha256::digest(key.as_bytes())))))
}

fn read_limited(path: &Path, limit: u64, too_large: &str) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > limit {
        return Err(invalid(too_large));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid(too_large));
    }
    Ok(bytes)
}

fn enforce_storage_limit(root: &Path, replacing: &Path, new_size: u64) -> io::Result<()> {
    let mut total = new_size;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.path() != replacing && entry.file_type()?.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
        if total > MAX_STORAGE_BYTES {
            return Err(invalid("busy: component storage exceeds 16 MiB"));
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(1);
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        crate::runtime::atomic_replace(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn plugin_error(code: &str, message: &str) -> bindings::vivido::vvmux_plugin::host::PluginError {
    bindings::vivido::vvmux_plugin::host::PluginError {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn io_plugin_error(error: &io::Error) -> bindings::vivido::vvmux_plugin::host::PluginError {
    let message = error.to_string();
    let code = message
        .split_once(':')
        .map_or("runtime_unavailable", |(code, _)| code);
    plugin_error(code, &message)
}

fn guest_error(error: bindings::vivido::vvmux_plugin::host::PluginError) -> io::Error {
    invalid(format!("{}: {}", error.code, error.message))
}

fn component_error(error: impl std::fmt::Display) -> io::Error {
    invalid(format!("runtime_unavailable: component: {error}"))
}

fn component_trap(error: impl std::fmt::Display) -> io::Error {
    invalid(format!("runtime_crashed: component trapped: {error}"))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(cancel: Arc<AtomicBool>, deadline: Instant) -> ComponentState {
        ComponentState {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            limits: ComponentLimits::new(),
            broker: None,
            storage: tempfile::tempdir().unwrap().keep(),
            deadline,
            cancel,
            invocation_logs: String::new(),
            logs_truncated: false,
        }
    }

    #[test]
    fn cache_rejects_a_key_or_payload_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("component.cwasm");
        write_cache(&path, &"a".repeat(64), b"serialized").unwrap();
        assert_eq!(read_cache(&path, &"a".repeat(64)).unwrap(), b"serialized");
        assert!(read_cache(&path, &"b".repeat(64)).is_err());
        let mut corrupt = fs::read(&path).unwrap();
        *corrupt.last_mut().unwrap() ^= 1;
        fs::write(&path, corrupt).unwrap();
        assert!(read_cache(&path, &"a".repeat(64)).is_err());
    }

    #[test]
    fn compiled_cache_is_keyed_by_engine_and_component() {
        let directory = tempfile::tempdir().unwrap();
        let engine = component_engine().unwrap();
        let first = wat::parse_str("(component)").unwrap();
        let second = wat::parse_str("(component (type (func)))").unwrap();
        assert_ne!(cache_key(engine, &first), cache_key(engine, &second));
        load_component(engine, directory.path(), &first).unwrap();
        let path = directory
            .path()
            .join(format!("{}.cwasm", cache_key(engine, &first)));
        assert!(path.is_file());
        load_component(engine, directory.path(), &first).unwrap();
    }

    #[test]
    fn epoch_interrupts_cancelled_component_code() {
        let engine = component_engine().unwrap();
        let bytes = wat::parse_str(
            r#"(component
                (core module $m
                    (func (export "run") (loop $forever (br $forever))))
                (core instance $i (instantiate $m))
                (func $run (canon lift (core func $i "run")))
                (export "run" (func $run)))"#,
        )
        .unwrap();
        let component = Component::from_binary(engine, &bytes).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut store = Store::new(engine, test_state(cancel, deadline));
        store.limiter(|state| &mut state.limits);
        store.set_fuel(u64::MAX).unwrap();
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|context| {
            if context.data().cancel.load(Ordering::Acquire) {
                Err(wasmtime::Error::msg("component invocation interrupted"))
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });
        let instance = Linker::new(engine)
            .instantiate(&mut store, &component)
            .unwrap();
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .unwrap();
        let started = Instant::now();
        assert!(run.call(&mut store, ()).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn fuel_interrupts_unbounded_component_code() {
        let engine = component_engine().unwrap();
        let bytes = wat::parse_str(
            r#"(component
                (core module $m
                    (func (export "run") (loop $forever (br $forever))))
                (core instance $i (instantiate $m))
                (func $run (canon lift (core func $i "run")))
                (export "run" (func $run)))"#,
        )
        .unwrap();
        let component = Component::from_binary(engine, &bytes).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut store = Store::new(
            engine,
            test_state(Arc::new(AtomicBool::new(false)), deadline),
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(1_000).unwrap();
        store.set_epoch_deadline(1_000);
        let instance = Linker::new(engine)
            .instantiate(&mut store, &component)
            .unwrap();
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .unwrap();
        assert!(run.call(&mut store, ()).is_err());
    }

    #[test]
    fn store_rejects_excessive_memory_and_tables() {
        let engine = component_engine().unwrap();
        for source in [
            "(component (core module $m (memory 1025)) (core instance (instantiate $m)))",
            "(component (core module $m (table 100001 funcref)) (core instance (instantiate $m)))",
        ] {
            let component =
                Component::from_binary(engine, &wat::parse_str(source).unwrap()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut store = Store::new(
                engine,
                test_state(Arc::new(AtomicBool::new(false)), deadline),
            );
            store.limiter(|state| &mut state.limits);
            store.set_fuel(FUEL_PER_CALL).unwrap();
            store.set_epoch_deadline(100);
            assert!(
                Linker::new(engine)
                    .instantiate(&mut store, &component)
                    .is_err()
            );
        }
    }

    #[test]
    fn wasi_context_inherits_no_host_environment() {
        let engine = component_engine().unwrap();
        let bytes = wat::parse_str(
            r#"(component
                (import "wasi:cli/environment@0.2.0" (instance $environment
                    (export "get-environment"
                        (func (result (list (tuple string string)))))))
                (alias export $environment "get-environment" (func $get-environment))
                (core module $libc
                    (memory (export "memory") 1)
                    (global $next (mut i32) (i32.const 1024))
                    (func (export "realloc")
                        (param i32 i32 i32 i32) (result i32)
                        (local $result i32)
                        global.get $next
                        local.tee $result
                        local.get 3
                        i32.add
                        global.set $next
                        local.get $result))
                (core instance $libc-instance (instantiate $libc))
                (alias core export $libc-instance "memory" (core memory $memory))
                (alias core export $libc-instance "realloc" (core func $realloc))
                (canon lower (func $get-environment)
                    (memory $memory) (realloc $realloc) (core func $get-environment-core))
                (core module $adapter
                    (import "" "get-environment" (func $get-environment (param i32)))
                    (func (export "get-environment") (result i32)
                        i32.const 512
                        call $get-environment
                        i32.const 512))
                (core instance $adapter-instance (instantiate $adapter
                    (with "" (instance
                        (export "get-environment" (func $get-environment-core))))))
                (func $get-environment-export
                    (result (list (tuple string string)))
                    (canon lift (core func $adapter-instance "get-environment")
                        (memory $memory) (realloc $realloc)))
                (export "get-environment" (func $get-environment-export)))"#,
        )
        .unwrap();
        let component = Component::from_binary(engine, &bytes).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut store = Store::new(
            engine,
            test_state(Arc::new(AtomicBool::new(false)), deadline),
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(FUEL_PER_CALL).unwrap();
        store.set_epoch_deadline(100);
        let mut linker = Linker::new(engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).unwrap();
        let instance = linker.instantiate(&mut store, &component).unwrap();
        let environment = instance
            .get_typed_func::<(), (Vec<(String, String)>,)>(&mut store, "get-environment")
            .unwrap()
            .call(&mut store, ())
            .unwrap();
        assert!(environment.0.is_empty());
    }

    #[test]
    fn storage_keys_are_scoped_and_bounded() {
        let root = Path::new("/private/plugin");
        let one = storage_path(root, "../secret").unwrap();
        let two = storage_path(root, "other").unwrap();
        assert!(one.starts_with(root));
        assert!(two.starts_with(root));
        assert_ne!(one, two);
        assert!(storage_path(root, "").is_err());
        assert!(storage_path(root, &"x".repeat(129)).is_err());
    }
}
