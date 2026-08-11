//! Session-scoped plugin scheduling and runtime ownership.
//!
//! The session actor submits bounded work here and never performs manifest loading, schema
//! validation, process startup, or protocol I/O itself. Each plugin gets one deterministic worker
//! and runtime cache; different plugins can progress independently.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use vvmux_plugin_api::{Activation, Event, EventHook, HostCall, Permission};

use crate::ipc::{AutomationError, PluginEventEnvelope};
use crate::session::{ActorEvent, AutomationReplyTarget};

const COMMAND_QUEUE: usize = 32;
const PLUGIN_QUEUE: usize = 128;
const MAX_SESSION_JOBS: usize = 16;
const MAX_PLUGIN_JOBS: usize = 4;
const MAX_RETAINED_JOBS: usize = 200;
const MAX_RETAINED_LOG_BYTES: usize = 256 * 1024;
const MAX_PENDING_EVENT_WORKFLOWS_PER_PLUGIN: usize = 128;

#[derive(Clone)]
pub(crate) struct PluginSupervisor {
    sender: mpsc::SyncSender<Message>,
    next_job_id: Arc<AtomicU64>,
    reload_requested: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
    event_gap: Arc<Mutex<Option<(u64, u64)>>>,
    session_name: String,
    session_instance: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeScope {
    pub(crate) session_instance: String,
    pub(crate) plugin_id: String,
    pub(crate) plugin_instance: String,
    pub(crate) permissions: Vec<Permission>,
}

#[derive(Clone)]
pub(crate) struct HostBroker {
    actor: mpsc::SyncSender<ActorEvent>,
    manager: mpsc::SyncSender<Message>,
    session_instance: String,
    tokens: Arc<Mutex<HashMap<String, BrokerIdentity>>>,
}

pub(crate) struct BrokerLease {
    token: String,
    scope: RuntimeScope,
    cause: Arc<Mutex<Option<PluginCause>>>,
    broker: HostBroker,
}

#[derive(Debug, Clone)]
struct BrokerIdentity {
    scope: RuntimeScope,
    cause: Arc<Mutex<Option<PluginCause>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct PluginCause {
    pub(crate) source: String,
    pub(crate) correlation_id: String,
    pub(crate) causation_id: String,
    pub(crate) causation_depth: u8,
    pub(crate) pane_id: Option<u64>,
    pub(crate) tab_id: Option<u64>,
}

pub(crate) struct CauseReset(Arc<Mutex<Option<PluginCause>>>);

impl HostBroker {
    pub(crate) fn issue(
        &self,
        plugin_id: &str,
        plugin_instance: &str,
        permissions: &[Permission],
    ) -> io::Result<BrokerLease> {
        let token = random_token()?;
        let scope = RuntimeScope {
            session_instance: self.session_instance.clone(),
            plugin_id: plugin_id.to_owned(),
            plugin_instance: plugin_instance.to_owned(),
            permissions: permissions.to_vec(),
        };
        let cause = Arc::new(Mutex::new(None));
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                token.clone(),
                BrokerIdentity {
                    scope: scope.clone(),
                    cause: cause.clone(),
                },
            );
        Ok(BrokerLease {
            token,
            scope,
            cause,
            broker: self.clone(),
        })
    }

    fn call(
        &self,
        token: &str,
        scope: &RuntimeScope,
        call: HostCall,
        deadline: Instant,
    ) -> io::Result<Value> {
        let cause = self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(token)
            .filter(|registered| registered.scope == *scope)
            .map(|registered| {
                registered
                    .cause
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            });
        let Some(cause) = cause else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "scope_denied: stale plugin broker token",
            ));
        };
        if call.method == "plugin.invoke" {
            if !scope.permissions.contains(&Permission::PluginInvoke) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "capability_denied: plugin lacks `plugin.invoke`",
                ));
            }
            let params = call
                .params
                .as_object()
                .ok_or_else(|| io::Error::other("schema_invalid: params must be an object"))?;
            if params
                .keys()
                .any(|key| key != "reference" && key != "input")
            {
                return Err(io::Error::other(
                    "schema_invalid: plugin.invoke has unknown parameters",
                ));
            }
            let reference = params
                .get("reference")
                .and_then(Value::as_str)
                .filter(|reference| !reference.is_empty() && reference.len() <= 256)
                .ok_or_else(|| {
                    io::Error::other("schema_invalid: plugin.invoke requires reference")
                })?
                .to_owned();
            let input = params
                .get("input")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            if serde_json::to_vec(&input).map_or(true, |body| body.len() > 1024 * 1024) {
                return Err(io::Error::other(
                    "schema_invalid: plugin.invoke input exceeds 1 MiB",
                ));
            }
            let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
            self.manager
                .try_send(Message::DependencyInvoke {
                    caller: scope.clone(),
                    cause,
                    reference,
                    input,
                    deadline,
                    reply: reply_sender,
                })
                .map_err(|error| match error {
                    mpsc::TrySendError::Full(_) => {
                        io::Error::other("busy: plugin supervisor queue is full")
                    }
                    mpsc::TrySendError::Disconnected(_) => {
                        io::Error::other("runtime_unavailable: plugin supervisor stopped")
                    }
                })?;
            return match reply_receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    Err(io::Error::other("timeout: dependency invocation expired"))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
                    "runtime_unavailable: dependency invocation reply dropped",
                )),
            };
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        self.actor
            .try_send(ActorEvent::PluginHostCall {
                scope: scope.clone(),
                cause,
                call,
                reply: sender,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    io::Error::other("busy: session actor queue is full")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::other("runtime_unavailable: session actor stopped")
                }
            })?;
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{}: {}", error.code, error.message),
            )),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timeout: host call expired",
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
                "runtime_unavailable: host call reply dropped",
            )),
        }
    }
}

impl BrokerLease {
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn call(&self, call: HostCall, deadline: Instant) -> io::Result<Value> {
        self.broker.call(&self.token, &self.scope, call, deadline)
    }

    pub(crate) fn enter_event(&self, context: &vvmux_plugin_api::InvocationContext) -> CauseReset {
        *self
            .cause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(PluginCause {
            source: format!(
                "plugin:{}:{}",
                self.scope.plugin_id, self.scope.plugin_instance
            ),
            correlation_id: context.correlation_id.clone(),
            causation_id: context.causation_id.clone(),
            causation_depth: context.causation_depth,
            pane_id: context.pane_id,
            tab_id: context.tab_id,
        });
        CauseReset(self.cause.clone())
    }
}

impl Drop for CauseReset {
    fn drop(&mut self) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

impl Drop for BrokerLease {
    fn drop(&mut self) {
        self.broker
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.token);
    }
}

enum Completion {
    Automation(AutomationReplyTarget),
    Notice(String),
    Broker(mpsc::SyncSender<io::Result<Value>>),
    Detached,
}

struct Job {
    id: u64,
    public_id: String,
    created_ms: u128,
    plugin_id: String,
    reference: String,
    input: Value,
    context: Option<vvmux_plugin_api::InvocationContext>,
    cancel: Arc<AtomicBool>,
    completion: Completion,
    event_workflow: Option<EventWorkflowDelivery>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EventWorkflowDelivery {
    workflow_id: String,
    sequence: u64,
    gap: Option<(u64, u64)>,
}

#[derive(Clone)]
struct PendingEventWorkflow {
    plugin_id: String,
    workflow_id: String,
    sequence: u64,
    payload: Value,
    context: vvmux_plugin_api::InvocationContext,
    gap: Option<(u64, u64)>,
}

struct WorkflowStepJob {
    run_id: u64,
    step_id: String,
    reference: String,
    input: Value,
    context: Option<vvmux_plugin_api::InvocationContext>,
    cancel: Arc<AtomicBool>,
    started_ms: u128,
    plugin_id: String,
    plugin_version: String,
    plugin_digest: String,
}

struct RunningWorkflowStep {
    plugin_id: String,
}

struct WorkflowRun {
    job: Job,
    workflow: crate::plugin::RuntimeWorkflow,
    trigger: Value,
    outputs: BTreeMap<String, Value>,
    running: BTreeMap<String, RunningWorkflowStep>,
    trace: Vec<Value>,
    aggregate_bytes: usize,
    deadline: Instant,
    failure: Option<io::Error>,
}

struct ActiveJob {
    plugin_id: String,
    client_id: Option<u64>,
    cancel: Arc<AtomicBool>,
    public_id: String,
}

struct WorkerHandle {
    sender: mpsc::SyncSender<WorkerMessage>,
    shutdown: Arc<AtomicBool>,
    plugin: crate::plugin::RuntimePlugin,
    stopping: bool,
}

#[derive(Clone, Serialize)]
struct RegistryReloadReport {
    generation: u64,
    applied: Vec<String>,
    deferred: Vec<String>,
    failed: BTreeMap<String, String>,
}

enum ReloadCompletion {
    Automation(AutomationReplyTarget),
    Notice,
}

struct ReloadWaiter {
    pending: BTreeSet<String>,
    report: RegistryReloadReport,
    completion: ReloadCompletion,
}

struct ManagerInputs {
    receiver: mpsc::Receiver<Message>,
    sender: mpsc::SyncSender<Message>,
    actor: mpsc::SyncSender<ActorEvent>,
    session_name: String,
    session_instance: String,
    broker: HostBroker,
    reload_requested: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
}

struct AppliedRegistry {
    generation: u64,
    plugins: BTreeMap<String, crate::plugin::RuntimePlugin>,
    catalog: BTreeMap<String, Vec<Value>>,
    failures: BTreeMap<String, String>,
}

#[derive(Clone, Serialize)]
struct RetainedJob {
    job_id: String,
    plugin_id: String,
    action: String,
    status: String,
    created_ms: u128,
    started_ms: u128,
    finished_ms: Option<u128>,
    result: Option<Value>,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    trace: Option<Value>,
}

#[derive(Default)]
struct JobStore {
    records: HashMap<String, RetainedJob>,
    completed: VecDeque<String>,
}

impl JobStore {
    fn start(&mut self, job: &Job) {
        if !matches!(job.completion, Completion::Detached) {
            return;
        }
        self.records.insert(
            job.public_id.clone(),
            RetainedJob {
                job_id: job.public_id.clone(),
                plugin_id: job.plugin_id.clone(),
                action: job.reference.clone(),
                status: "running".into(),
                created_ms: job.created_ms,
                started_ms: now_ms(),
                finished_ms: None,
                result: None,
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                trace: None,
            },
        );
    }

    fn finish(&mut self, job: &Job, result: &io::Result<Value>) {
        if !matches!(job.completion, Completion::Detached) {
            return;
        }
        if !self.records.contains_key(&job.public_id) {
            self.start(job);
        }
        let record = self
            .records
            .get_mut(&job.public_id)
            .expect("detached jobs are inserted before completion");
        record.finished_ms = Some(now_ms());
        match result {
            Ok(value) => {
                record.status = "succeeded".into();
                let rendered = serde_json::to_string(value)
                    .unwrap_or_else(|error| format!("result serialization failed: {error}"));
                let (stdout, truncated) = truncate_log(rendered, MAX_RETAINED_LOG_BYTES);
                record.stdout = stdout;
                record.stdout_truncated = truncated;
                if !truncated {
                    record.result = Some(value.clone());
                }
            }
            Err(error) => {
                let message = error.to_string();
                record.status = if message.starts_with("cancelled") {
                    "cancelled"
                } else if message.starts_with("timeout") {
                    "timed_out"
                } else {
                    "failed"
                }
                .into();
                let (stderr, truncated) = truncate_log(message, MAX_RETAINED_LOG_BYTES);
                record.stderr = stderr;
                record.stderr_truncated = truncated;
            }
        }
        self.completed.push_back(job.public_id.clone());
        while self.completed.len() > MAX_RETAINED_JOBS {
            if let Some(oldest) = self.completed.pop_front() {
                self.records.remove(&oldest);
            }
        }
    }

    fn finish_with_logs(
        &mut self,
        job: &Job,
        result: &io::Result<Value>,
        logs: crate::plugin::RuntimeLogs,
    ) {
        self.finish(job, result);
        if !matches!(job.completion, Completion::Detached) || logs.stderr.is_empty() {
            return;
        }
        let record = self
            .records
            .get_mut(&job.public_id)
            .expect("detached jobs retain a record after completion");
        let mut combined = logs.stderr;
        combined.push_str(&record.stderr);
        let (stderr, truncated) = truncate_log(combined, MAX_RETAINED_LOG_BYTES);
        record.stderr = stderr;
        record.stderr_truncated |= logs.stderr_truncated || truncated;
    }

    fn status(&self, job_id: &str) -> Result<Value, AutomationError> {
        let record = self.records.get(job_id).ok_or_else(job_not_found)?;
        serde_json::to_value(record).map_err(|error| {
            AutomationError::new(
                "runtime_unavailable",
                format!("serialize job status: {error}"),
            )
        })
    }

    fn logs(&self, job_id: &str) -> Result<Value, AutomationError> {
        let record = self.records.get(job_id).ok_or_else(job_not_found)?;
        Ok(serde_json::json!({
            "job_id": record.job_id,
            "status": record.status,
            "stdout": record.stdout,
            "stderr": record.stderr,
            "stdout_truncated": record.stdout_truncated,
            "stderr_truncated": record.stderr_truncated,
        }))
    }

    fn set_trace(&mut self, job_id: &str, trace: Value) {
        if let Some(record) = self.records.get_mut(job_id) {
            record.trace = Some(trace);
        }
    }
}

#[derive(Default)]
struct JobAccounting {
    total: usize,
    per_plugin: HashMap<String, usize>,
}

impl JobAccounting {
    fn can_admit(&self, plugin_id: &str) -> bool {
        let plugin_jobs = self.per_plugin.get(plugin_id).copied().unwrap_or(0);
        self.total < MAX_SESSION_JOBS && plugin_jobs < MAX_PLUGIN_JOBS
    }

    fn admit(&mut self, plugin_id: &str) -> bool {
        if !self.can_admit(plugin_id) {
            return false;
        }
        self.total += 1;
        *self.per_plugin.entry(plugin_id.to_owned()).or_default() += 1;
        true
    }

    fn complete(&mut self, plugin_id: &str) {
        self.total = self.total.saturating_sub(1);
        if let Some(count) = self.per_plugin.get_mut(plugin_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_plugin.remove(plugin_id);
            }
        }
    }
}

enum Message {
    Invoke(Job),
    DependencyInvoke {
        caller: RuntimeScope,
        cause: Option<PluginCause>,
        reference: String,
        input: Value,
        deadline: Instant,
        reply: mpsc::SyncSender<io::Result<Value>>,
    },
    Complete {
        job: Job,
        result: io::Result<Value>,
        logs: crate::plugin::RuntimeLogs,
    },
    WorkflowStepComplete {
        step: WorkflowStepJob,
        result: io::Result<Value>,
        logs: crate::plugin::RuntimeLogs,
    },
    WorkflowDeadline(u64),
    WorkerReady(String),
    JobStatus {
        job_id: String,
        reply: AutomationReplyTarget,
    },
    JobCancel {
        job_id: String,
        reply: AutomationReplyTarget,
    },
    JobLogs {
        job_id: String,
        reply: AutomationReplyTarget,
    },
    OpenPane {
        reference: String,
        reply: AutomationReplyTarget,
    },
    Capabilities {
        reply: AutomationReplyTarget,
    },
    Reload {
        completion: ReloadCompletion,
    },
    ReloadLoaded {
        result: io::Result<crate::plugin::RegistryCandidate>,
        completions: Vec<ReloadCompletion>,
    },
    WorkerStopped {
        plugin_id: String,
        digest: String,
    },
    PublishEvent(PluginEventEnvelope),
    RuntimeCrashed {
        plugin_id: String,
        context: Option<vvmux_plugin_api::InvocationContext>,
    },
    CancelClient(u64),
    Shutdown,
}

enum WorkerMessage {
    Invoke(Job),
    WorkflowStep(WorkflowStepJob),
    Activate,
    Event { hook: EventHook, event: Event },
    Shutdown,
}

impl PluginSupervisor {
    pub(crate) fn start(
        session_name: String,
        session_instance: String,
        actor: mpsc::SyncSender<ActorEvent>,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_QUEUE);
        let broker = HostBroker {
            actor: actor.clone(),
            manager: sender.clone(),
            session_instance: session_instance.clone(),
            tokens: Arc::new(Mutex::new(HashMap::new())),
        };
        let manager_sender = sender.clone();
        let reload_requested = Arc::new(AtomicBool::new(false));
        let manager_reload_requested = reload_requested.clone();
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let manager_shutdown_requested = shutdown_requested.clone();
        let manager_session_name = session_name.clone();
        let manager_session_instance = session_instance.clone();
        let event_gap = Arc::new(Mutex::new(None));
        thread::Builder::new()
            .name(format!("vvmux-plugin-supervisor-{session_name}"))
            .spawn(move || {
                run_manager(ManagerInputs {
                    receiver,
                    sender: manager_sender,
                    actor,
                    session_name: manager_session_name,
                    session_instance: manager_session_instance,
                    broker,
                    reload_requested: manager_reload_requested,
                    shutdown_requested: manager_shutdown_requested,
                });
            })?;
        Ok(Self {
            sender,
            next_job_id: Arc::new(AtomicU64::new(1)),
            reload_requested,
            shutdown_requested,
            session_name,
            session_instance,
            event_gap,
        })
    }

    pub(crate) fn invoke_automation(
        &self,
        reference: String,
        input: Value,
        reply: AutomationReplyTarget,
    ) -> Result<(), AutomationError> {
        self.submit(reference, input, Completion::Automation(reply))
    }

    pub(crate) fn invoke_notice(
        &self,
        reference: String,
        input: Value,
        display_reference: String,
    ) -> Result<(), AutomationError> {
        self.submit(reference, input, Completion::Notice(display_reference))
    }

    pub(crate) fn invoke_detached(
        &self,
        reference: String,
        input: Value,
    ) -> Result<String, AutomationError> {
        self.submit_job(reference, input, Completion::Detached)
    }

    pub(crate) fn job_status(
        &self,
        job_id: String,
        reply: AutomationReplyTarget,
    ) -> Result<(), AutomationError> {
        self.send_control(Message::JobStatus { job_id, reply })
    }

    pub(crate) fn job_cancel(
        &self,
        job_id: String,
        reply: AutomationReplyTarget,
    ) -> Result<(), AutomationError> {
        self.send_control(Message::JobCancel { job_id, reply })
    }

    pub(crate) fn job_logs(
        &self,
        job_id: String,
        reply: AutomationReplyTarget,
    ) -> Result<(), AutomationError> {
        self.send_control(Message::JobLogs { job_id, reply })
    }

    pub(crate) fn open_pane(
        &self,
        reference: String,
        reply: AutomationReplyTarget,
    ) -> Result<(), AutomationError> {
        self.send_control(Message::OpenPane { reference, reply })
    }

    pub(crate) fn capabilities(&self, reply: AutomationReplyTarget) -> Result<(), AutomationError> {
        self.send_control(Message::Capabilities { reply })
    }

    pub(crate) fn reload_automation(
        &self,
        reply: AutomationReplyTarget,
    ) -> Result<(), AutomationError> {
        self.send_control(Message::Reload {
            completion: ReloadCompletion::Automation(reply),
        })
    }

    pub(crate) fn reload_notice(&self) -> Result<(), AutomationError> {
        match self.sender.try_send(Message::Reload {
            completion: ReloadCompletion::Notice,
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                self.reload_requested.store(true, Ordering::Release);
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => Err(AutomationError::new(
                "runtime_unavailable",
                "plugin supervisor is unavailable",
            )),
        }
    }

    fn submit(
        &self,
        reference: String,
        input: Value,
        completion: Completion,
    ) -> Result<(), AutomationError> {
        self.submit_job(reference, input, completion).map(|_| ())
    }

    fn submit_job(
        &self,
        reference: String,
        input: Value,
        completion: Completion,
    ) -> Result<String, AutomationError> {
        let plugin_id = reference
            .split_once('/')
            .map(|(plugin, _)| plugin)
            .filter(|plugin| !plugin.is_empty())
            .ok_or_else(|| {
                AutomationError::new("action_not_found", "plugin reference must be ID/ACTION")
            })?
            .to_owned();
        let id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        let public_id = format!("{}/{}-{id:016x}", self.session_name, self.session_instance);
        let job = Job {
            id,
            public_id: public_id.clone(),
            created_ms: now_ms(),
            plugin_id,
            reference,
            input,
            context: None,
            cancel: Arc::new(AtomicBool::new(false)),
            completion,
            event_workflow: None,
        };
        self.sender
            .try_send(Message::Invoke(job))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    AutomationError::new("busy", "plugin supervisor queue is full")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    AutomationError::new("runtime_unavailable", "plugin supervisor is unavailable")
                }
            })?;
        Ok(public_id)
    }

    fn send_control(&self, message: Message) -> Result<(), AutomationError> {
        self.sender.try_send(message).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => {
                AutomationError::new("busy", "plugin supervisor queue is full")
            }
            mpsc::TrySendError::Disconnected(_) => {
                AutomationError::new("runtime_unavailable", "plugin supervisor is unavailable")
            }
        })
    }

    pub(crate) fn cancel_client(&self, client_id: u64) {
        let _ = self.sender.try_send(Message::CancelClient(client_id));
    }

    pub(crate) fn publish_event(&self, event: PluginEventEnvelope) {
        let sequence = match &event {
            PluginEventEnvelope::Event { sequence, .. } => *sequence,
            PluginEventEnvelope::Gap { to_sequence, .. } => *to_sequence,
        };
        let pending_gap = self
            .event_gap
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some((from_sequence, to_sequence)) = pending_gap
            && self
                .sender
                .try_send(Message::PublishEvent(PluginEventEnvelope::Gap {
                    from_sequence,
                    to_sequence,
                }))
                .is_err()
        {
            *self
                .event_gap
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((from_sequence, sequence));
            return;
        }
        if self.sender.try_send(Message::PublishEvent(event)).is_err() {
            let mut gap = self
                .event_gap
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(gap) = gap.as_mut() {
                gap.1 = sequence;
            } else {
                *gap = Some((sequence, sequence));
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
        let _ = self.sender.try_send(Message::Shutdown);
    }
}

fn run_manager(inputs: ManagerInputs) {
    let ManagerInputs {
        receiver,
        sender,
        actor,
        session_name,
        session_instance,
        broker,
        reload_requested,
        shutdown_requested,
    } = inputs;
    let initial = crate::plugin::load_registry_candidate();
    let mut registry = match initial {
        Ok(candidate) => AppliedRegistry {
            generation: candidate.generation,
            plugins: candidate.plugins,
            catalog: candidate.catalog,
            failures: candidate.failed,
        },
        Err(error) => {
            let mut failures = BTreeMap::new();
            failures.insert("registry".into(), error.to_string());
            AppliedRegistry {
                generation: 0,
                plugins: BTreeMap::new(),
                catalog: BTreeMap::new(),
                failures,
            }
        }
    };
    let mut workers = HashMap::<String, WorkerHandle>::new();
    let mut active = HashMap::<u64, ActiveJob>::new();
    let mut accounting = JobAccounting::default();
    let mut retained = JobStore::default();
    let mut transitions = BTreeSet::<String>::new();
    let mut reload_waiters = Vec::<ReloadWaiter>::new();
    let mut reload_loading = false;
    let mut queued_reloads = Vec::<ReloadCompletion>::new();
    let mut event_gaps = HashMap::<String, (u64, u64)>::new();
    let mut upstream_workflow_gap = None;
    let mut workflows = HashMap::<u64, WorkflowRun>::new();
    let mut queued_event_workflows = BTreeSet::<(String, String)>::new();
    let mut pending_event_workflows = BTreeMap::<(String, String), PendingEventWorkflow>::new();
    let mut next_dependency_job_id = 1_u64 << 62;
    let mut next_event_workflow_id = 1_u64 << 63;
    activate_session_plugins(
        &registry,
        &mut workers,
        &session_name,
        &session_instance,
        &broker,
        &sender,
    );
    loop {
        drain_pending_event_workflows(
            &mut pending_event_workflows,
            &mut queued_event_workflows,
            &workflows,
            &accounting,
            &registry,
            &transitions,
            &sender,
            &session_name,
            &session_instance,
            &mut next_event_workflow_id,
        );
        let Ok(message) = receiver.recv() else {
            break;
        };
        match message {
            Message::DependencyInvoke {
                caller,
                cause,
                reference,
                input,
                deadline,
                reply,
            } => {
                let result = resolve_dependency_invocation(
                    &registry,
                    &transitions,
                    &session_instance,
                    &caller,
                    cause,
                    &reference,
                    input,
                    deadline,
                    next_dependency_job_id,
                    &session_name,
                    reply,
                );
                next_dependency_job_id =
                    next_dependency_job_id.checked_add(1).unwrap_or(1_u64 << 62);
                match result {
                    Ok(job) => {
                        if let Err(error) = sender.try_send(Message::Invoke(job)) {
                            let job = match error {
                                mpsc::TrySendError::Full(Message::Invoke(job))
                                | mpsc::TrySendError::Disconnected(Message::Invoke(job)) => job,
                                _ => unreachable!(
                                    "dependency submission queues only invocation jobs"
                                ),
                            };
                            deliver(
                                &actor,
                                job.completion,
                                Err(io::Error::other("busy: plugin supervisor queue is full")),
                            );
                        }
                    }
                    Err((reply, error)) => {
                        let _ = reply.try_send(Err(error));
                    }
                }
            }
            Message::Invoke(job) => {
                if let Some(delivery) = &job.event_workflow {
                    queued_event_workflows
                        .remove(&(job.plugin_id.clone(), delivery.workflow_id.clone()));
                }
                retained.start(&job);
                let Some(plugin) = registry.plugins.get(&job.plugin_id).cloned() else {
                    let result = if let Some(error) = registry.failures.get(&job.plugin_id) {
                        Err(io::Error::other(format!(
                            "runtime_unavailable: registry entry is invalid: {error}"
                        )))
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("plugin_not_found: `{}` is not installed", job.plugin_id),
                        ))
                    };
                    retained.finish(&job, &result);
                    deliver(&actor, job.completion, result);
                    continue;
                };
                if !plugin.enabled {
                    let result = Err(io::Error::other("plugin_disabled: plugin is disabled"));
                    retained.finish(&job, &result);
                    deliver(&actor, job.completion, result);
                    continue;
                }
                if transitions.contains(&job.plugin_id) {
                    let result = Err(io::Error::other(
                        "busy: plugin runtime is draining for a registry update",
                    ));
                    retained.finish(&job, &result);
                    deliver(&actor, job.completion, result);
                    continue;
                }
                let workflow = job
                    .reference
                    .split_once('/')
                    .and_then(|(_, workflow_id)| {
                        plugin
                            .workflows
                            .iter()
                            .find(|workflow| workflow.id == workflow_id)
                    })
                    .cloned();
                if let Some(workflow) = workflow {
                    let validation = vvmux_plugin_api::validate_schema_instance(
                        &workflow.input_schema,
                        &job.input,
                    );
                    if let Err(errors) = validation {
                        let result = Err(io::Error::other(format!(
                            "schema_invalid: {}",
                            errors.join("; ")
                        )));
                        retained.finish(&job, &result);
                        deliver(&actor, job.completion, result);
                        continue;
                    }
                    if !accounting.admit(&job.plugin_id) {
                        restore_pending_event_workflow(&job, &mut pending_event_workflows);
                        let result = Err(io::Error::other("busy: plugin job limit reached"));
                        retained.finish(&job, &result);
                        deliver(&actor, job.completion, result);
                        continue;
                    }
                    let client_id = match &job.completion {
                        Completion::Automation(reply) => Some(reply.client_id()),
                        Completion::Notice(_) | Completion::Broker(_) | Completion::Detached => {
                            None
                        }
                    };
                    let run_id = job.id;
                    let public_id = job.public_id.clone();
                    let cancel = job.cancel.clone();
                    let mut timeout = std::time::Duration::from_millis(workflow.timeout_ms);
                    if let Some(context_deadline) = job
                        .context
                        .as_ref()
                        .map(|context| context.deadline_unix_ms)
                        .filter(|deadline| *deadline != 0)
                    {
                        let now = now_ms().min(u128::from(u64::MAX)) as u64;
                        timeout = timeout.min(std::time::Duration::from_millis(
                            context_deadline.saturating_sub(now).max(1),
                        ));
                    }
                    let deadline = Instant::now()
                        .checked_add(timeout)
                        .unwrap_or_else(Instant::now);
                    active.insert(
                        run_id,
                        ActiveJob {
                            plugin_id: job.plugin_id.clone(),
                            client_id,
                            cancel,
                            public_id,
                        },
                    );
                    let trace = job
                        .event_workflow
                        .as_ref()
                        .and_then(|delivery| delivery.gap)
                        .map(|(from_sequence, to_sequence)| {
                            vec![serde_json::json!({
                                "kind": "event_gap",
                                "from_sequence": from_sequence,
                                "to_sequence": to_sequence,
                            })]
                        })
                        .unwrap_or_default();
                    workflows.insert(
                        run_id,
                        WorkflowRun {
                            trigger: job.input.clone(),
                            job,
                            workflow,
                            outputs: BTreeMap::new(),
                            running: BTreeMap::new(),
                            trace,
                            aggregate_bytes: 0,
                            deadline,
                            failure: None,
                        },
                    );
                    let deadline_sender = sender.clone();
                    thread::spawn(move || {
                        thread::sleep(deadline.saturating_duration_since(Instant::now()));
                        let _ = deadline_sender.send(Message::WorkflowDeadline(run_id));
                    });
                    advance_workflow(
                        run_id,
                        &mut workflows,
                        &registry,
                        &transitions,
                        &mut workers,
                        &session_name,
                        &session_instance,
                        &broker,
                        &sender,
                        &mut active,
                        &mut accounting,
                        &mut retained,
                        &actor,
                    );
                    continue;
                }
                if !accounting.admit(&job.plugin_id) {
                    restore_pending_event_workflow(&job, &mut pending_event_workflows);
                    let result = Err(io::Error::other("busy: plugin job limit reached"));
                    retained.finish(&job, &result);
                    deliver(&actor, job.completion, result);
                    continue;
                }
                let worker = if let Some(worker) = workers.get(&job.plugin_id) {
                    worker.sender.clone()
                } else {
                    match spawn_worker(
                        plugin.clone(),
                        &session_name,
                        &session_instance,
                        broker.clone(),
                        sender.clone(),
                    ) {
                        Ok((worker, shutdown)) => {
                            workers.insert(
                                job.plugin_id.clone(),
                                WorkerHandle {
                                    sender: worker.clone(),
                                    shutdown,
                                    plugin,
                                    stopping: false,
                                },
                            );
                            worker
                        }
                        Err(error) => {
                            accounting.complete(&job.plugin_id);
                            let result = Err(error);
                            retained.finish(&job, &result);
                            deliver(&actor, job.completion, result);
                            continue;
                        }
                    }
                };
                let client_id = match &job.completion {
                    Completion::Automation(reply) => Some(reply.client_id()),
                    Completion::Notice(_) | Completion::Broker(_) | Completion::Detached => None,
                };
                let active_job = ActiveJob {
                    plugin_id: job.plugin_id.clone(),
                    client_id,
                    cancel: job.cancel.clone(),
                    public_id: job.public_id.clone(),
                };
                let job_id = job.id;
                if let Err(error) = worker.try_send(WorkerMessage::Invoke(job)) {
                    let (job, disconnected) = match error {
                        mpsc::TrySendError::Full(WorkerMessage::Invoke(job)) => (job, false),
                        mpsc::TrySendError::Disconnected(WorkerMessage::Invoke(job)) => (job, true),
                        mpsc::TrySendError::Full(
                            WorkerMessage::Shutdown | WorkerMessage::Activate,
                        )
                        | mpsc::TrySendError::Disconnected(
                            WorkerMessage::Shutdown | WorkerMessage::Activate,
                        )
                        | mpsc::TrySendError::Full(WorkerMessage::Event { .. })
                        | mpsc::TrySendError::Disconnected(WorkerMessage::Event { .. })
                        | mpsc::TrySendError::Full(WorkerMessage::WorkflowStep(_))
                        | mpsc::TrySendError::Disconnected(WorkerMessage::WorkflowStep(_)) => {
                            unreachable!("the supervisor only submits invocation messages here")
                        }
                    };
                    if disconnected {
                        workers.remove(&active_job.plugin_id);
                    }
                    accounting.complete(&active_job.plugin_id);
                    let result = if disconnected {
                        Err(io::Error::other(
                            "runtime_unavailable: plugin worker stopped",
                        ))
                    } else {
                        Err(io::Error::other("busy: plugin event queue is full"))
                    };
                    retained.finish(&job, &result);
                    deliver(&actor, job.completion, result);
                    continue;
                }
                active.insert(job_id, active_job);
            }
            Message::Complete { job, result, logs } => {
                if let Some(active_job) = active.remove(&job.id) {
                    accounting.complete(&active_job.plugin_id);
                }
                retained.finish_with_logs(&job, &result, logs);
                let status = match &result {
                    Ok(_) => "succeeded",
                    Err(error) if error.to_string().starts_with("cancelled") => "cancelled",
                    Err(error) if error.to_string().starts_with("timeout") => "timed_out",
                    Err(_) => "failed",
                };
                let _ = actor.try_send(ActorEvent::PluginLifecycle {
                    name: "plugin.job_completed".into(),
                    payload: serde_json::json!({
                        "job_id": &job.public_id,
                        "plugin_id": &job.plugin_id,
                        "action": &job.reference,
                        "status": status,
                    }),
                    context: job_lifecycle_context(&job),
                });
                if result
                    .as_ref()
                    .is_err_and(|error| error.to_string().starts_with("runtime_crashed"))
                {
                    let _ = actor.try_send(ActorEvent::PluginLifecycle {
                        name: "plugin.runtime_crashed".into(),
                        payload: serde_json::json!({"plugin_id": &job.plugin_id}),
                        context: None,
                    });
                }
                deliver(&actor, job.completion, result);
                request_worker_stop_if_drained(
                    &job.plugin_id,
                    &active,
                    workflow_uses_plugin(&workflows, &job.plugin_id),
                    &transitions,
                    &mut workers,
                );
                advance_all_workflows(
                    &mut workflows,
                    &registry,
                    &transitions,
                    &mut workers,
                    &session_name,
                    &session_instance,
                    &broker,
                    &sender,
                    &mut active,
                    &mut accounting,
                    &mut retained,
                    &actor,
                );
            }
            Message::WorkflowStepComplete { step, result, logs } => {
                accounting.complete(&step.plugin_id);
                if let Some(run) = workflows.get_mut(&step.run_id) {
                    run.running.remove(&step.step_id);
                    let finished_ms = now_ms();
                    let status = match &result {
                        Ok(_) => "succeeded",
                        Err(error) if error.to_string().starts_with("cancelled") => "cancelled",
                        Err(error) if error.to_string().starts_with("timeout") => "timed_out",
                        Err(_) => "failed",
                    };
                    let error_code = result.as_ref().err().map(|error| {
                        crate::session::plugin_automation_error(io::Error::new(
                            error.kind(),
                            error.to_string(),
                        ))
                        .code
                    });
                    run.trace.push(serde_json::json!({
                        "step_id": &step.step_id,
                        "action": &step.reference,
                        "plugin_id": &step.plugin_id,
                        "plugin_version": &step.plugin_version,
                        "plugin_digest": &step.plugin_digest,
                        "started_ms": step.started_ms,
                        "finished_ms": finished_ms,
                        "status": status,
                        "error_code": error_code,
                        "stderr_truncated": logs.stderr_truncated,
                    }));
                    match result {
                        Ok(output) => {
                            let size =
                                serde_json::to_vec(&output).map_or(usize::MAX, |body| body.len());
                            run.aggregate_bytes = run.aggregate_bytes.saturating_add(size);
                            if run.aggregate_bytes > 1024 * 1024 {
                                run.failure = Some(io::Error::other(
                                    "output_invalid: workflow intermediates exceed 1 MiB",
                                ));
                                run.job.cancel.store(true, Ordering::Release);
                            } else {
                                run.outputs.insert(step.step_id, output);
                            }
                        }
                        Err(error) => {
                            if run.failure.is_none() {
                                run.failure = Some(io::Error::other(format!(
                                    "dependency_failed: workflow step failed: {error}"
                                )));
                            }
                            run.job.cancel.store(true, Ordering::Release);
                        }
                    }
                }
                request_worker_stop_if_drained(
                    &step.plugin_id,
                    &active,
                    workflow_uses_plugin(&workflows, &step.plugin_id),
                    &transitions,
                    &mut workers,
                );
                advance_workflow(
                    step.run_id,
                    &mut workflows,
                    &registry,
                    &transitions,
                    &mut workers,
                    &session_name,
                    &session_instance,
                    &broker,
                    &sender,
                    &mut active,
                    &mut accounting,
                    &mut retained,
                    &actor,
                );
            }
            Message::WorkflowDeadline(run_id) => {
                if let Some(run) = workflows.get_mut(&run_id)
                    && Instant::now() >= run.deadline
                    && run.failure.is_none()
                {
                    run.failure = Some(io::Error::other("timeout: workflow deadline expired"));
                    run.job.cancel.store(true, Ordering::Release);
                }
                advance_workflow(
                    run_id,
                    &mut workflows,
                    &registry,
                    &transitions,
                    &mut workers,
                    &session_name,
                    &session_instance,
                    &broker,
                    &sender,
                    &mut active,
                    &mut accounting,
                    &mut retained,
                    &actor,
                );
            }
            Message::WorkerReady(plugin_id) => {
                flush_plugin_event_gap(
                    &plugin_id,
                    &registry,
                    &mut workers,
                    &mut event_gaps,
                    &session_instance,
                );
                advance_all_workflows(
                    &mut workflows,
                    &registry,
                    &transitions,
                    &mut workers,
                    &session_name,
                    &session_instance,
                    &broker,
                    &sender,
                    &mut active,
                    &mut accounting,
                    &mut retained,
                    &actor,
                );
            }
            Message::JobStatus { job_id, reply } => {
                deliver_query(&actor, reply, retained.status(&job_id));
            }
            Message::JobCancel { job_id, reply } => {
                let workflow = active
                    .values()
                    .find(|active_job| active_job.public_id == job_id)
                    .map(|job| job.public_id.clone());
                let result = if let Some(job) = active
                    .values()
                    .find(|active_job| active_job.public_id == job_id)
                {
                    job.cancel.store(true, Ordering::Release);
                    Ok(serde_json::json!({"job_id": job_id, "status": "cancelling"}))
                } else {
                    retained.status(&job_id)
                };
                if workflow.is_some() {
                    advance_all_workflows(
                        &mut workflows,
                        &registry,
                        &transitions,
                        &mut workers,
                        &session_name,
                        &session_instance,
                        &broker,
                        &sender,
                        &mut active,
                        &mut accounting,
                        &mut retained,
                        &actor,
                    );
                }
                deliver_query(&actor, reply, result);
            }
            Message::JobLogs { job_id, reply } => {
                deliver_query(&actor, reply, retained.logs(&job_id));
            }
            Message::OpenPane { reference, reply } => {
                let result =
                    resolve_pane_launch(&registry, &transitions, &session_instance, &reference);
                match result {
                    Ok(launch) => {
                        let _ = actor.send(ActorEvent::PluginPaneOpen { launch, reply });
                    }
                    Err(error) => deliver_query(&actor, reply, Err(error)),
                }
            }
            Message::Capabilities { reply } => {
                let actions = registry
                    .catalog
                    .iter()
                    .filter(|(plugin_id, _)| {
                        !transitions.contains(*plugin_id)
                            && registry
                                .plugins
                                .get(*plugin_id)
                                .is_some_and(|plugin| plugin.enabled)
                    })
                    .flat_map(|(_, actions)| actions.iter().cloned())
                    .collect::<Vec<_>>();
                let plugin = serde_json::json!({
                    "enabled": true,
                    "protocol_version": vvmux_plugin_api::PROTOCOL_VERSION,
                    "session_instance": session_instance,
                    "applied_generation": registry.generation,
                    "methods": ["catalog", "invoke", "job_status", "job_cancel", "job_logs", "pane_open", "event_subscribe", "event_unsubscribe", "reload"],
                    "native_trust": "full_user_authority",
                    "component_sandbox": true,
                    "enforceable_capabilities": crate::session::plugin_enforceable_capabilities(),
                    "actions": actions,
                    "failed": registry.failures,
                });
                deliver_query(
                    &actor,
                    reply,
                    Ok(crate::session::automation_capabilities(plugin)),
                );
            }
            Message::Reload { completion } => {
                if reload_loading {
                    queued_reloads.push(completion);
                } else {
                    reload_loading = true;
                    if let Err((error, completions)) =
                        spawn_registry_load(sender.clone(), vec![completion])
                    {
                        reload_loading = false;
                        for completion in completions {
                            finish_reload(&actor, completion, Err(error.clone()));
                        }
                    }
                }
            }
            Message::ReloadLoaded {
                result,
                completions,
            } => {
                reload_loading = false;
                match result {
                    Ok(candidate) if candidate.generation < registry.generation => {
                        let error = AutomationError::new(
                            "dependency_failed",
                            format!(
                                "plugin registry generation {} is older than applied generation {}",
                                candidate.generation, registry.generation
                            ),
                        );
                        for completion in completions {
                            finish_reload(&actor, completion, Err(error.clone()));
                        }
                    }
                    Ok(candidate) => {
                        let report = apply_registry_candidate(
                            candidate,
                            &mut registry,
                            &active,
                            &workflows,
                            &mut workers,
                            &mut transitions,
                            &actor,
                        );
                        activate_session_plugins(
                            &registry,
                            &mut workers,
                            &session_name,
                            &session_instance,
                            &broker,
                            &sender,
                        );
                        let pending = transitions.clone();
                        for completion in completions {
                            if pending.is_empty() {
                                finish_reload(
                                    &actor,
                                    completion,
                                    serde_json::to_value(&report).map_err(|error| {
                                        AutomationError::new(
                                            "runtime_unavailable",
                                            format!("serialize plugin reload report: {error}"),
                                        )
                                    }),
                                );
                            } else {
                                reload_waiters.push(ReloadWaiter {
                                    pending: pending.clone(),
                                    report: report.clone(),
                                    completion,
                                });
                            }
                        }
                    }
                    Err(error) => {
                        let error = AutomationError::new(
                            "dependency_failed",
                            format!("plugin registry reload failed: {error}"),
                        );
                        for completion in completions {
                            finish_reload(&actor, completion, Err(error.clone()));
                        }
                    }
                }
                if !queued_reloads.is_empty() {
                    reload_loading = true;
                    if let Err((error, completions)) =
                        spawn_registry_load(sender.clone(), std::mem::take(&mut queued_reloads))
                    {
                        reload_loading = false;
                        for completion in completions {
                            finish_reload(&actor, completion, Err(error.clone()));
                        }
                    }
                }
            }
            Message::WorkerStopped { plugin_id, digest } => {
                if workers
                    .get(&plugin_id)
                    .is_some_and(|worker| worker.plugin.digest == digest)
                {
                    workers.remove(&plugin_id);
                    transitions.remove(&plugin_id);
                    for waiter in &mut reload_waiters {
                        waiter.pending.remove(&plugin_id);
                    }
                    finish_ready_reload_waiters(&actor, &mut reload_waiters);
                    activate_session_plugins(
                        &registry,
                        &mut workers,
                        &session_name,
                        &session_instance,
                        &broker,
                        &sender,
                    );
                }
            }
            Message::PublishEvent(envelope) => {
                if let PluginEventEnvelope::Gap {
                    from_sequence,
                    to_sequence,
                } = &envelope
                {
                    merge_gap(&mut upstream_workflow_gap, (*from_sequence, *to_sequence));
                    for plugin in registry.plugins.values().filter(|plugin| {
                        plugin.enabled
                            && !plugin.events.is_empty()
                            && plugin.permissions.contains(&Permission::EventsSubscribe)
                    }) {
                        event_gaps
                            .entry(plugin.id.clone())
                            .and_modify(|gap| gap.1 = *to_sequence)
                            .or_insert((*from_sequence, *to_sequence));
                    }
                    continue;
                }
                let PluginEventEnvelope::Event {
                    sequence,
                    name,
                    payload,
                    context,
                } = envelope
                else {
                    continue;
                };
                if context.causation_depth >= 8 {
                    continue;
                }
                let plugin_ids = registry.plugins.keys().cloned().collect::<Vec<_>>();
                for plugin_id in plugin_ids {
                    let Some(plugin) = registry.plugins.get(&plugin_id).cloned() else {
                        continue;
                    };
                    if !plugin.enabled || transitions.contains(&plugin_id) {
                        continue;
                    }
                    if !plugin.permissions.contains(&Permission::EventsSubscribe) {
                        continue;
                    }
                    let hooks = plugin
                        .events
                        .iter()
                        .filter(|hook| hook.on == name)
                        .filter(|hook| hook_accepts_event(&plugin_id, hook, &context))
                        .cloned()
                        .collect::<Vec<_>>();
                    if hooks.is_empty() {
                        continue;
                    }
                    let worker = if let Some(worker) = workers.get(&plugin_id) {
                        worker.sender.clone()
                    } else {
                        match spawn_worker(
                            plugin.clone(),
                            &session_name,
                            &session_instance,
                            broker.clone(),
                            sender.clone(),
                        ) {
                            Ok((worker, shutdown)) => {
                                workers.insert(
                                    plugin_id.clone(),
                                    WorkerHandle {
                                        sender: worker.clone(),
                                        shutdown,
                                        plugin,
                                        stopping: false,
                                    },
                                );
                                worker
                            }
                            Err(_) => continue,
                        }
                    };
                    for hook in hooks {
                        let mut event_context = context.clone();
                        event_context.causation_depth =
                            event_context.causation_depth.saturating_add(1);
                        event_context.deadline_unix_ms = now_ms()
                            .saturating_add(u128::from(hook.timeout_ms))
                            .min(u128::from(u64::MAX))
                            as u64;
                        if let Some((from_sequence, to_sequence)) = event_gaps.remove(&plugin_id) {
                            let gap = Event {
                                request_id: 0,
                                sequence: to_sequence,
                                name: "vvmux.event_gap".into(),
                                payload: serde_json::json!({
                                    "from_sequence": from_sequence,
                                    "to_sequence": to_sequence,
                                }),
                                context: event_context.clone(),
                            };
                            if worker
                                .try_send(WorkerMessage::Event {
                                    hook: hook.clone(),
                                    event: gap,
                                })
                                .is_err()
                            {
                                event_gaps.insert(plugin_id.clone(), (from_sequence, sequence));
                                break;
                            }
                        }
                        let event = Event {
                            request_id: 0,
                            sequence,
                            name: name.clone(),
                            payload: payload.clone(),
                            context: event_context,
                        };
                        if worker
                            .try_send(WorkerMessage::Event { hook, event })
                            .is_err()
                        {
                            event_gaps
                                .entry(plugin_id.clone())
                                .and_modify(|gap| gap.1 = sequence)
                                .or_insert((sequence, sequence));
                            break;
                        }
                    }
                }
                let triggered = registry
                    .plugins
                    .values()
                    .filter(|plugin| plugin.enabled && !transitions.contains(&plugin.id))
                    .flat_map(|plugin| {
                        plugin
                            .workflows
                            .iter()
                            .filter(|workflow| workflow.trigger == name)
                            .filter(|_| {
                                !context
                                    .source
                                    .starts_with(&format!("plugin:{}:", plugin.id))
                            })
                            .map(|workflow| (plugin.id.clone(), workflow.clone()))
                    })
                    .collect::<Vec<_>>();
                let has_triggered_workflows = !triggered.is_empty();
                for (plugin_id, workflow) in triggered {
                    let mut workflow_context = context.clone();
                    workflow_context.causation_depth =
                        workflow_context.causation_depth.saturating_add(1);
                    workflow_context.deadline_unix_ms = now_ms()
                        .saturating_add(u128::from(workflow.timeout_ms))
                        .min(u128::from(u64::MAX))
                        as u64;
                    retain_event_workflow_trigger(
                        &mut pending_event_workflows,
                        PendingEventWorkflow {
                            plugin_id,
                            workflow_id: workflow.id,
                            sequence,
                            payload: payload.clone(),
                            context: workflow_context,
                            gap: upstream_workflow_gap,
                        },
                    );
                }
                if has_triggered_workflows {
                    upstream_workflow_gap = None;
                }
            }
            Message::RuntimeCrashed {
                plugin_id,
                mut context,
            } => {
                if context.is_none() {
                    let correlation_id =
                        random_identity().unwrap_or_else(|_| format!("runtime-crash-{}", now_ms()));
                    context = Some(vvmux_plugin_api::InvocationContext {
                        correlation_id: correlation_id.clone(),
                        causation_id: correlation_id,
                        causation_depth: 0,
                        source: format!("plugin:{plugin_id}:runtime"),
                        session_instance: session_instance.clone(),
                        pane_id: None,
                        tab_id: None,
                        deadline_unix_ms: 0,
                    });
                }
                if let Some(context) = &mut context {
                    context.source = format!("plugin:{plugin_id}:runtime");
                }
                let _ = actor.try_send(ActorEvent::PluginLifecycle {
                    name: "plugin.runtime_crashed".into(),
                    payload: serde_json::json!({"plugin_id": plugin_id}),
                    context,
                });
            }
            Message::CancelClient(client_id) => {
                for job in active
                    .values()
                    .filter(|job| job.client_id == Some(client_id))
                {
                    job.cancel.store(true, Ordering::Release);
                }
                advance_all_workflows(
                    &mut workflows,
                    &registry,
                    &transitions,
                    &mut workers,
                    &session_name,
                    &session_instance,
                    &broker,
                    &sender,
                    &mut active,
                    &mut accounting,
                    &mut retained,
                    &actor,
                );
            }
            Message::Shutdown => {
                for job in active.values() {
                    job.cancel.store(true, Ordering::Release);
                }
                for worker in workers.values() {
                    worker.shutdown.store(true, Ordering::Release);
                    let _ = worker.sender.try_send(WorkerMessage::Shutdown);
                }
                break;
            }
        }
        if reload_requested.swap(false, Ordering::AcqRel) {
            if reload_loading {
                queued_reloads.push(ReloadCompletion::Notice);
            } else {
                reload_loading = true;
                if let Err((error, completions)) =
                    spawn_registry_load(sender.clone(), vec![ReloadCompletion::Notice])
                {
                    reload_loading = false;
                    for completion in completions {
                        finish_reload(&actor, completion, Err(error.clone()));
                    }
                }
            }
        }
        if shutdown_requested.load(Ordering::Acquire) {
            for job in active.values() {
                job.cancel.store(true, Ordering::Release);
            }
            for worker in workers.values() {
                worker.shutdown.store(true, Ordering::Release);
                let _ = worker.sender.try_send(WorkerMessage::Shutdown);
            }
            break;
        }
    }
}

fn hook_accepts_event(
    plugin_id: &str,
    hook: &EventHook,
    context: &vvmux_plugin_api::InvocationContext,
) -> bool {
    context.causation_depth < 8
        && (hook.include_self || !context.source.starts_with(&format!("plugin:{plugin_id}:")))
}

fn merge_gap(target: &mut Option<(u64, u64)>, incoming: (u64, u64)) {
    match target {
        Some((from_sequence, to_sequence)) => {
            *from_sequence = (*from_sequence).min(incoming.0);
            *to_sequence = (*to_sequence).max(incoming.1);
        }
        None => *target = Some(incoming),
    }
}

fn retain_event_workflow_trigger(
    pending: &mut BTreeMap<(String, String), PendingEventWorkflow>,
    mut incoming: PendingEventWorkflow,
) {
    let key = (incoming.plugin_id.clone(), incoming.workflow_id.clone());
    if let Some(previous) = pending.remove(&key) {
        merge_gap(
            &mut incoming.gap,
            previous
                .gap
                .unwrap_or((previous.sequence, previous.sequence)),
        );
        if previous.sequence < incoming.sequence {
            merge_gap(
                &mut incoming.gap,
                (previous.sequence, incoming.sequence.saturating_sub(1)),
            );
        }
    } else if pending
        .keys()
        .filter(|(plugin_id, _)| plugin_id == &incoming.plugin_id)
        .count()
        >= MAX_PENDING_EVENT_WORKFLOWS_PER_PLUGIN
    {
        return;
    }
    pending.insert(key, incoming);
}

fn restore_pending_event_workflow(
    job: &Job,
    pending: &mut BTreeMap<(String, String), PendingEventWorkflow>,
) {
    let Some(delivery) = &job.event_workflow else {
        return;
    };
    let Some(context) = job.context.clone() else {
        return;
    };
    let key = (job.plugin_id.clone(), delivery.workflow_id.clone());
    if let Some(existing) = pending.get_mut(&key) {
        merge_gap(
            &mut existing.gap,
            delivery
                .gap
                .unwrap_or((delivery.sequence, delivery.sequence)),
        );
        if delivery.sequence < existing.sequence {
            merge_gap(
                &mut existing.gap,
                (delivery.sequence, existing.sequence.saturating_sub(1)),
            );
        }
        return;
    }
    pending.insert(
        key,
        PendingEventWorkflow {
            plugin_id: job.plugin_id.clone(),
            workflow_id: delivery.workflow_id.clone(),
            sequence: delivery.sequence,
            payload: job.input.clone(),
            context,
            gap: delivery.gap,
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn drain_pending_event_workflows(
    pending: &mut BTreeMap<(String, String), PendingEventWorkflow>,
    queued: &mut BTreeSet<(String, String)>,
    workflows: &HashMap<u64, WorkflowRun>,
    accounting: &JobAccounting,
    registry: &AppliedRegistry,
    transitions: &BTreeSet<String>,
    manager: &mpsc::SyncSender<Message>,
    session_name: &str,
    session_instance: &str,
    next_id: &mut u64,
) {
    let active = workflows
        .values()
        .filter_map(|run| {
            run.job
                .event_workflow
                .as_ref()
                .map(|delivery| (run.job.plugin_id.clone(), delivery.workflow_id.clone()))
        })
        .collect::<BTreeSet<_>>();
    let candidates = pending
        .keys()
        .filter(|key| !queued.contains(*key) && !active.contains(*key))
        .take(8)
        .cloned()
        .collect::<Vec<_>>();
    for key in candidates {
        let Some(trigger) = pending.get(&key).cloned() else {
            continue;
        };
        let Some(plugin) = registry.plugins.get(&trigger.plugin_id) else {
            pending.remove(&key);
            continue;
        };
        if !plugin.enabled || transitions.contains(&trigger.plugin_id) {
            pending.remove(&key);
            continue;
        }
        if !plugin
            .workflows
            .iter()
            .any(|workflow| workflow.id == trigger.workflow_id)
        {
            pending.remove(&key);
            continue;
        }
        if !accounting.can_admit(&trigger.plugin_id) {
            continue;
        }
        let id = *next_id;
        let job = Job {
            id,
            public_id: format!("{session_name}/{session_instance}-{id:016x}"),
            created_ms: now_ms(),
            plugin_id: trigger.plugin_id.clone(),
            reference: format!("{}/{}", trigger.plugin_id, trigger.workflow_id),
            input: trigger.payload,
            context: Some(trigger.context),
            cancel: Arc::new(AtomicBool::new(false)),
            completion: Completion::Detached,
            event_workflow: Some(EventWorkflowDelivery {
                workflow_id: trigger.workflow_id,
                sequence: trigger.sequence,
                gap: trigger.gap,
            }),
        };
        match manager.try_send(Message::Invoke(job)) {
            Ok(()) => {
                pending.remove(&key);
                queued.insert(key);
                *next_id = next_id.checked_add(1).unwrap_or(1_u64 << 63);
            }
            Err(mpsc::TrySendError::Full(Message::Invoke(_))) => break,
            Err(mpsc::TrySendError::Disconnected(Message::Invoke(_))) => break,
            Err(_) => unreachable!("event workflows queue only invocation jobs"),
        }
    }
}

fn flush_plugin_event_gap(
    plugin_id: &str,
    registry: &AppliedRegistry,
    workers: &mut HashMap<String, WorkerHandle>,
    event_gaps: &mut HashMap<String, (u64, u64)>,
    session_instance: &str,
) {
    let Some(&(from_sequence, to_sequence)) = event_gaps.get(plugin_id) else {
        return;
    };
    let Some(plugin) = registry.plugins.get(plugin_id) else {
        event_gaps.remove(plugin_id);
        return;
    };
    let Some(hook) = plugin.events.first().cloned() else {
        return;
    };
    let Some(worker) = workers.get(plugin_id) else {
        return;
    };
    let correlation_id = format!("{session_instance}-event-gap-{to_sequence:016x}");
    let event = Event {
        request_id: 0,
        sequence: to_sequence,
        name: "vvmux.event_gap".into(),
        payload: serde_json::json!({
            "from_sequence": from_sequence,
            "to_sequence": to_sequence,
        }),
        context: vvmux_plugin_api::InvocationContext {
            correlation_id: correlation_id.clone(),
            causation_id: correlation_id,
            causation_depth: 0,
            source: "session".into(),
            session_instance: session_instance.to_owned(),
            pane_id: None,
            tab_id: None,
            deadline_unix_ms: now_ms()
                .saturating_add(u128::from(hook.timeout_ms))
                .min(u128::from(u64::MAX)) as u64,
        },
    };
    if worker
        .sender
        .try_send(WorkerMessage::Event { hook, event })
        .is_ok()
    {
        event_gaps.remove(plugin_id);
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_dependency_invocation(
    registry: &AppliedRegistry,
    transitions: &BTreeSet<String>,
    session_instance: &str,
    caller: &RuntimeScope,
    cause: Option<PluginCause>,
    reference: &str,
    input: Value,
    deadline: Instant,
    id: u64,
    session_name: &str,
    reply: mpsc::SyncSender<io::Result<Value>>,
) -> Result<Job, (mpsc::SyncSender<io::Result<Value>>, io::Error)> {
    let result = (|| -> io::Result<Job> {
        if caller.session_instance != session_instance {
            return Err(io::Error::other(
                "scope_denied: plugin belongs to another session instance",
            ));
        }
        if !caller.permissions.contains(&Permission::PluginInvoke) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "capability_denied: plugin lacks `plugin.invoke`",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("timeout: dependency invocation expired"));
        }
        let (alias, action_id) = reference.split_once('/').ok_or_else(|| {
            io::Error::other("action_not_found: dependency reference must be ALIAS/ACTION")
        })?;
        if alias.is_empty() || action_id.is_empty() || action_id.contains('/') {
            return Err(io::Error::other(
                "action_not_found: dependency reference must be ALIAS/ACTION",
            ));
        }
        let owner = registry.plugins.get(&caller.plugin_id).ok_or_else(|| {
            io::Error::other("plugin_not_found: invoking plugin is not installed")
        })?;
        if !owner.enabled || transitions.contains(&caller.plugin_id) {
            return Err(io::Error::other(
                "runtime_unavailable: invoking plugin is draining",
            ));
        }
        let loaded = crate::plugin::load_package(&owner.root)?;
        let dependency_id = loaded
            .manifest
            .dependencies
            .iter()
            .find(|dependency| dependency.alias == alias)
            .map(|dependency| dependency.id.as_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "scope_denied: plugin invocation requires a declared dependency alias",
                )
            })?;
        let dependency = registry.plugins.get(dependency_id).ok_or_else(|| {
            io::Error::other(format!(
                "dependency_failed: plugin `{dependency_id}` is unavailable"
            ))
        })?;
        if !dependency.enabled || transitions.contains(dependency_id) {
            return Err(io::Error::other(format!(
                "dependency_failed: plugin `{dependency_id}` is unavailable"
            )));
        }
        let dependency_package = crate::plugin::load_package(&dependency.root)?;
        if dependency_package.action(action_id).is_none()
            && !dependency
                .workflows
                .iter()
                .any(|workflow| workflow.id == action_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("action_not_found: `{dependency_id}/{action_id}` does not exist"),
            ));
        }
        let causation_id = random_identity()?;
        let (correlation_id, causation_depth, pane_id, tab_id) = cause.map_or_else(
            || (causation_id.clone(), 1, None, None),
            |cause| {
                (
                    cause.correlation_id,
                    cause.causation_depth.saturating_add(1),
                    cause.pane_id,
                    cause.tab_id,
                )
            },
        );
        if causation_depth >= 8 {
            return Err(io::Error::other(
                "scope_denied: plugin causation depth reached 8",
            ));
        }
        let deadline_unix_ms = now_ms()
            .saturating_add(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .min(u128::from(u64::MAX)) as u64;
        let public_id = format!("{session_name}/{session_instance}-{id:016x}");
        Ok(Job {
            id,
            public_id,
            created_ms: now_ms(),
            plugin_id: dependency_id.to_owned(),
            reference: format!("{dependency_id}/{action_id}"),
            input,
            context: Some(vvmux_plugin_api::InvocationContext {
                correlation_id,
                causation_id,
                causation_depth,
                source: format!("plugin:{}:{}", caller.plugin_id, caller.plugin_instance),
                session_instance: session_instance.to_owned(),
                pane_id,
                tab_id,
                deadline_unix_ms,
            }),
            cancel: Arc::new(AtomicBool::new(false)),
            completion: Completion::Broker(reply.clone()),
            event_workflow: None,
        })
    })();
    result.map_err(|error| (reply, error))
}

fn spawn_worker(
    plugin: crate::plugin::RuntimePlugin,
    session_name: &str,
    session_instance: &str,
    broker: HostBroker,
    manager: mpsc::SyncSender<Message>,
) -> io::Result<(mpsc::SyncSender<WorkerMessage>, Arc<AtomicBool>)> {
    let (sender, receiver) = mpsc::sync_channel(PLUGIN_QUEUE);
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = shutdown.clone();
    let plugin_id = plugin.id.clone();
    let digest = plugin.digest.clone();
    let session_name = session_name.to_owned();
    let session_instance = session_instance.to_owned();
    let mut runtime =
        crate::plugin::SessionPluginRuntime::new(session_name, session_instance, plugin, broker)?;
    thread::Builder::new()
        .name(format!("vvmux-plugin-{plugin_id}"))
        .spawn(move || {
            while let Ok(message) = receiver.recv() {
                if worker_shutdown.load(Ordering::Acquire) {
                    break;
                }
                match message {
                    WorkerMessage::Invoke(job) => {
                        let result = runtime.invoke(
                            &job.reference,
                            job.input.clone(),
                            job.cancel.clone(),
                            job.context.clone(),
                        );
                        let logs = runtime.take_logs();
                        if manager
                            .send(Message::Complete { job, result, logs })
                            .is_err()
                        {
                            break;
                        }
                    }
                    WorkerMessage::WorkflowStep(step) => {
                        let result = runtime.invoke(
                            &step.reference,
                            step.input.clone(),
                            step.cancel.clone(),
                            step.context.clone(),
                        );
                        let logs = runtime.take_logs();
                        if manager
                            .send(Message::WorkflowStepComplete { step, result, logs })
                            .is_err()
                        {
                            break;
                        }
                    }
                    WorkerMessage::Activate => {
                        if runtime.activate().is_err() {
                            let _ = manager.send(Message::RuntimeCrashed {
                                plugin_id: plugin_id.clone(),
                                context: None,
                            });
                        }
                    }
                    WorkerMessage::Event { hook, event } => {
                        let context = event.context.clone();
                        if runtime
                            .on_event(&hook, event, worker_shutdown.clone())
                            .is_err_and(|error| error.to_string().starts_with("runtime_crashed"))
                        {
                            let _ = manager.send(Message::RuntimeCrashed {
                                plugin_id: plugin_id.clone(),
                                context: Some(context),
                            });
                        }
                        let _ = manager.try_send(Message::WorkerReady(plugin_id.clone()));
                    }
                    WorkerMessage::Shutdown => break,
                }
            }
            drop(runtime);
            let _ = manager.send(Message::WorkerStopped { plugin_id, digest });
        })?;
    Ok((sender, shutdown))
}

#[allow(clippy::too_many_arguments)]
fn advance_all_workflows(
    workflows: &mut HashMap<u64, WorkflowRun>,
    registry: &AppliedRegistry,
    transitions: &BTreeSet<String>,
    workers: &mut HashMap<String, WorkerHandle>,
    session_name: &str,
    session_instance: &str,
    broker: &HostBroker,
    manager: &mpsc::SyncSender<Message>,
    active: &mut HashMap<u64, ActiveJob>,
    accounting: &mut JobAccounting,
    retained: &mut JobStore,
    actor: &mpsc::SyncSender<ActorEvent>,
) {
    let run_ids = workflows.keys().copied().collect::<Vec<_>>();
    for run_id in run_ids {
        advance_workflow(
            run_id,
            workflows,
            registry,
            transitions,
            workers,
            session_name,
            session_instance,
            broker,
            manager,
            active,
            accounting,
            retained,
            actor,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn advance_workflow(
    run_id: u64,
    workflows: &mut HashMap<u64, WorkflowRun>,
    registry: &AppliedRegistry,
    transitions: &BTreeSet<String>,
    workers: &mut HashMap<String, WorkerHandle>,
    session_name: &str,
    session_instance: &str,
    broker: &HostBroker,
    manager: &mpsc::SyncSender<Message>,
    active: &mut HashMap<u64, ActiveJob>,
    accounting: &mut JobAccounting,
    retained: &mut JobStore,
    actor: &mpsc::SyncSender<ActorEvent>,
) {
    let Some(mut run) = workflows.remove(&run_id) else {
        return;
    };
    if run.failure.is_none() && run.job.cancel.load(Ordering::Acquire) {
        run.failure = Some(io::Error::other("cancelled: workflow was cancelled"));
    }
    if run.failure.is_none() && Instant::now() >= run.deadline {
        run.failure = Some(io::Error::other("timeout: workflow deadline expired"));
        run.job.cancel.store(true, Ordering::Release);
    }
    if run.failure.is_some() {
        run.job.cancel.store(true, Ordering::Release);
        if run.running.is_empty() {
            finish_workflow(run, active, accounting, retained, actor);
        } else {
            workflows.insert(run_id, run);
        }
        return;
    }
    if run.outputs.len() == run.workflow.steps.len() {
        let result = crate::plugin::resolve_workflow_template(
            &run.workflow.output,
            &run.trigger,
            &run.outputs,
        )
        .and_then(|output| {
            if serde_json::to_vec(&output).map_or(true, |body| body.len() > 1024 * 1024) {
                return Err(io::Error::other(
                    "output_invalid: workflow result exceeds 1 MiB",
                ));
            }
            vvmux_plugin_api::validate_schema_instance(&run.workflow.output_schema, &output)
                .map_err(|errors| {
                    io::Error::other(format!("output_invalid: {}", errors.join("; ")))
                })?;
            Ok(output)
        });
        if let Err(error) = &result {
            run.failure = Some(io::Error::new(error.kind(), error.to_string()));
        }
        finish_workflow_with_result(run, result, active, accounting, retained, actor);
        return;
    }

    let mut admitted_any = false;
    let ready = run
        .workflow
        .steps
        .iter()
        .filter(|step| !run.outputs.contains_key(&step.id) && !run.running.contains_key(&step.id))
        .filter(|step| step.needs.iter().all(|need| run.outputs.contains_key(need)))
        .take(8_usize.saturating_sub(run.running.len()))
        .cloned()
        .collect::<Vec<_>>();
    for step in ready {
        let input =
            match crate::plugin::resolve_workflow_template(&step.input, &run.trigger, &run.outputs)
            {
                Ok(input) => input,
                Err(error) => {
                    run.failure = Some(error);
                    break;
                }
            };
        let Some((plugin_id, _)) = step.reference.split_once('/') else {
            run.failure = Some(io::Error::other(
                "dependency_failed: invalid step reference",
            ));
            break;
        };
        let Some(plugin) = registry.plugins.get(plugin_id).cloned() else {
            run.failure = Some(io::Error::other(format!(
                "dependency_failed: plugin `{plugin_id}` is unavailable"
            )));
            break;
        };
        if !plugin.enabled || transitions.contains(plugin_id) {
            run.failure = Some(io::Error::other(format!(
                "dependency_failed: plugin `{plugin_id}` is unavailable"
            )));
            break;
        }
        if !accounting.admit(plugin_id) {
            continue;
        }
        let worker = if let Some(worker) = workers.get(plugin_id) {
            worker.sender.clone()
        } else {
            match spawn_worker(
                plugin.clone(),
                session_name,
                session_instance,
                broker.clone(),
                manager.clone(),
            ) {
                Ok((sender, shutdown)) => {
                    workers.insert(
                        plugin_id.to_owned(),
                        WorkerHandle {
                            sender: sender.clone(),
                            shutdown,
                            plugin: plugin.clone(),
                            stopping: false,
                        },
                    );
                    sender
                }
                Err(error) => {
                    accounting.complete(plugin_id);
                    run.failure = Some(error);
                    break;
                }
            }
        };
        let started_ms = now_ms();
        let workflow_step = WorkflowStepJob {
            run_id,
            step_id: step.id.clone(),
            reference: step.reference.clone(),
            input,
            context: run.job.context.clone(),
            cancel: run.job.cancel.clone(),
            started_ms,
            plugin_id: plugin_id.to_owned(),
            plugin_version: plugin.version.clone(),
            plugin_digest: plugin.digest.clone(),
        };
        match worker.try_send(WorkerMessage::WorkflowStep(workflow_step)) {
            Ok(()) => {
                admitted_any = true;
                run.running.insert(
                    step.id,
                    RunningWorkflowStep {
                        plugin_id: plugin_id.to_owned(),
                    },
                );
            }
            Err(mpsc::TrySendError::Full(WorkerMessage::WorkflowStep(_))) => {
                accounting.complete(plugin_id);
            }
            Err(mpsc::TrySendError::Disconnected(WorkerMessage::WorkflowStep(_))) => {
                accounting.complete(plugin_id);
                workers.remove(plugin_id);
                run.failure = Some(io::Error::other(format!(
                    "runtime_unavailable: plugin `{plugin_id}` worker stopped"
                )));
                break;
            }
            Err(_) => unreachable!("only workflow steps are submitted here"),
        }
    }
    if run.failure.is_some() {
        run.job.cancel.store(true, Ordering::Release);
    }
    if run.failure.is_some() && run.running.is_empty() {
        finish_workflow(run, active, accounting, retained, actor);
    } else {
        let _ = admitted_any;
        workflows.insert(run_id, run);
    }
}

fn finish_workflow(
    mut run: WorkflowRun,
    active: &mut HashMap<u64, ActiveJob>,
    accounting: &mut JobAccounting,
    retained: &mut JobStore,
    actor: &mpsc::SyncSender<ActorEvent>,
) {
    let error = run
        .failure
        .take()
        .unwrap_or_else(|| io::Error::other("dependency_failed: workflow failed"));
    finish_workflow_with_result(run, Err(error), active, accounting, retained, actor);
}

fn finish_workflow_with_result(
    run: WorkflowRun,
    result: io::Result<Value>,
    active: &mut HashMap<u64, ActiveJob>,
    accounting: &mut JobAccounting,
    retained: &mut JobStore,
    actor: &mpsc::SyncSender<ActorEvent>,
) {
    let status = match &result {
        Ok(_) => "succeeded",
        Err(error) if error.to_string().starts_with("cancelled") => "cancelled",
        Err(error) if error.to_string().starts_with("timeout") => "timed_out",
        Err(_) => "failed",
    };
    active.remove(&run.job.id);
    accounting.complete(&run.job.plugin_id);
    retained.finish(&run.job, &result);
    retained.set_trace(
        &run.job.public_id,
        serde_json::json!({
            "workflow": run.workflow.id,
            "status": status,
            "steps": &run.trace,
        }),
    );
    let _ = actor.try_send(ActorEvent::PluginLifecycle {
        name: "plugin.job_completed".into(),
        payload: serde_json::json!({
            "job_id": &run.job.public_id,
            "plugin_id": &run.job.plugin_id,
            "action": &run.job.reference,
            "status": status,
        }),
        context: job_lifecycle_context(&run.job),
    });
    deliver(actor, run.job.completion, result);
}

fn activate_session_plugins(
    registry: &AppliedRegistry,
    workers: &mut HashMap<String, WorkerHandle>,
    session_name: &str,
    session_instance: &str,
    broker: &HostBroker,
    manager: &mpsc::SyncSender<Message>,
) {
    let plugins = registry
        .plugins
        .values()
        .filter(|plugin| {
            plugin.enabled
                && plugin.activation == Activation::Session
                && !workers.contains_key(&plugin.id)
        })
        .cloned()
        .collect::<Vec<_>>();
    for plugin in plugins {
        let Ok((sender, shutdown)) = spawn_worker(
            plugin.clone(),
            session_name,
            session_instance,
            broker.clone(),
            manager.clone(),
        ) else {
            continue;
        };
        let _ = sender.try_send(WorkerMessage::Activate);
        workers.insert(
            plugin.id.clone(),
            WorkerHandle {
                sender,
                shutdown,
                plugin,
                stopping: false,
            },
        );
    }
}

fn spawn_registry_load(
    manager: mpsc::SyncSender<Message>,
    completions: Vec<ReloadCompletion>,
) -> Result<(), (AutomationError, Vec<ReloadCompletion>)> {
    let shared_completions = Arc::new(Mutex::new(Some(completions)));
    let thread_completions = shared_completions.clone();
    thread::Builder::new()
        .name("vvmux-plugin-registry-load".into())
        .spawn(move || {
            let result = crate::plugin::load_registry_candidate();
            let completions = thread_completions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
                .unwrap_or_default();
            let _ = manager.send(Message::ReloadLoaded {
                result,
                completions,
            });
        })
        .map(|_| ())
        .map_err(|error| {
            let completions = shared_completions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
                .unwrap_or_default();
            (
                AutomationError::new(
                    "runtime_unavailable",
                    format!("could not start plugin registry loader: {error}"),
                ),
                completions,
            )
        })
}

fn apply_registry_candidate(
    candidate: crate::plugin::RegistryCandidate,
    registry: &mut AppliedRegistry,
    active: &HashMap<u64, ActiveJob>,
    workflows: &HashMap<u64, WorkflowRun>,
    workers: &mut HashMap<String, WorkerHandle>,
    transitions: &mut BTreeSet<String>,
    actor: &mpsc::SyncSender<ActorEvent>,
) -> RegistryReloadReport {
    let mut ids = BTreeSet::new();
    ids.extend(registry.plugins.keys().cloned());
    ids.extend(candidate.plugins.keys().cloned());
    ids.extend(candidate.failed.keys().cloned());
    let mut report = RegistryReloadReport {
        generation: candidate.generation,
        applied: Vec::new(),
        deferred: Vec::new(),
        failed: candidate.failed.clone(),
    };
    for id in ids {
        if candidate.failed.contains_key(&id) {
            continue;
        }
        let previous = registry.plugins.get(&id);
        let next = candidate.plugins.get(&id);
        let changed = previous != next || registry.catalog.get(&id) != candidate.catalog.get(&id);
        if changed
            && let Some(previous) = previous
            && previous.enabled
        {
            let _ = actor.send(ActorEvent::PluginPanesClose {
                plugin_id: id.clone(),
                package_digest: previous.digest.clone(),
            });
        }
        if changed {
            match next {
                Some(plugin) => {
                    registry.plugins.insert(id.clone(), plugin.clone());
                    registry.catalog.insert(
                        id.clone(),
                        candidate.catalog.get(&id).cloned().unwrap_or_default(),
                    );
                }
                None => {
                    registry.plugins.remove(&id);
                    registry.catalog.remove(&id);
                }
            }
        }
        let worker_needs_stop = workers.get(&id).is_some_and(|worker| {
            transitions.contains(&id)
                || next.is_none_or(|plugin| !plugin.enabled || worker.plugin != *plugin)
        });
        if worker_needs_stop {
            transitions.insert(id.clone());
            if next.is_none_or(|plugin| !plugin.enabled) {
                for job in active.values().filter(|job| job.plugin_id == id) {
                    job.cancel.store(true, Ordering::Release);
                }
                for run in workflows
                    .values()
                    .filter(|run| run.job.plugin_id == id || workflow_run_uses_plugin(run, &id))
                {
                    run.job.cancel.store(true, Ordering::Release);
                }
            }
            request_worker_stop_if_drained(
                &id,
                active,
                workflow_uses_plugin(workflows, &id),
                transitions,
                workers,
            );
            report.deferred.push(id);
        } else if changed {
            report.applied.push(id);
        }
    }
    registry.generation = candidate.generation.max(registry.generation);
    registry.failures = candidate.failed;
    report
}

fn resolve_pane_launch(
    registry: &AppliedRegistry,
    transitions: &BTreeSet<String>,
    session_instance: &str,
    reference: &str,
) -> Result<crate::session::PluginPaneLaunch, AutomationError> {
    let (plugin_id, pane_id) = reference.split_once('/').ok_or_else(|| {
        AutomationError::new("action_not_found", "plugin pane reference must be ID/PANE")
    })?;
    if plugin_id.is_empty() || pane_id.is_empty() || reference.len() > 256 {
        return Err(AutomationError::new(
            "action_not_found",
            "plugin pane reference must be ID/PANE",
        ));
    }
    let plugin = registry.plugins.get(plugin_id).ok_or_else(|| {
        AutomationError::new(
            "plugin_not_found",
            format!("plugin `{plugin_id}` is not installed"),
        )
    })?;
    if !plugin.enabled {
        return Err(AutomationError::new(
            "plugin_disabled",
            format!("plugin `{plugin_id}` is disabled"),
        ));
    }
    if transitions.contains(plugin_id) {
        return Err(AutomationError::new(
            "runtime_unavailable",
            format!("plugin `{plugin_id}` is changing generations"),
        ));
    }
    if !plugin.permissions.contains(&Permission::PaneCreate) {
        return Err(AutomationError::new(
            "capability_denied",
            format!("plugin `{plugin_id}` lacks `pane.create` capability"),
        ));
    }
    let pane = plugin
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .ok_or_else(|| {
            AutomationError::new(
                "action_not_found",
                format!("plugin `{plugin_id}` has no pane `{pane_id}`"),
            )
        })?;
    let plugin_instance = random_identity().map_err(|error| {
        AutomationError::new(
            "runtime_unavailable",
            format!("could not allocate plugin pane identity: {error}"),
        )
    })?;
    Ok(crate::session::PluginPaneLaunch {
        scope: RuntimeScope {
            session_instance: session_instance.to_owned(),
            plugin_id: plugin_id.to_owned(),
            plugin_instance,
            permissions: plugin.permissions.clone(),
        },
        package_digest: plugin.digest.clone(),
        package_root: plugin.root.clone(),
        pane,
    })
}

fn request_worker_stop_if_drained(
    plugin_id: &str,
    active: &HashMap<u64, ActiveJob>,
    workflow_in_use: bool,
    transitions: &BTreeSet<String>,
    workers: &mut HashMap<String, WorkerHandle>,
) {
    if !transitions.contains(plugin_id)
        || workflow_in_use
        || active.values().any(|job| job.plugin_id == plugin_id)
    {
        return;
    }
    if let Some(worker) = workers.get_mut(plugin_id)
        && !worker.stopping
    {
        worker.stopping = true;
        worker.shutdown.store(true, Ordering::Release);
        if worker.sender.try_send(WorkerMessage::Shutdown).is_err() {
            // A disconnected worker will have already queued WorkerStopped. A full queue cannot
            // occur after every accounted invocation completed, but leaving the transition set
            // makes a later completion/reload retry safely instead of accepting work too early.
            worker.stopping = false;
        }
    }
}

fn workflow_uses_plugin(workflows: &HashMap<u64, WorkflowRun>, plugin_id: &str) -> bool {
    workflows
        .values()
        .any(|run| workflow_run_uses_plugin(run, plugin_id))
}

fn workflow_run_uses_plugin(run: &WorkflowRun, plugin_id: &str) -> bool {
    run.running.values().any(|step| step.plugin_id == plugin_id)
}

fn finish_ready_reload_waiters(
    actor: &mpsc::SyncSender<ActorEvent>,
    waiters: &mut Vec<ReloadWaiter>,
) {
    let mut index = 0;
    while index < waiters.len() {
        if waiters[index].pending.is_empty() {
            let waiter = waiters.swap_remove(index);
            finish_reload(
                actor,
                waiter.completion,
                serde_json::to_value(waiter.report).map_err(|error| {
                    AutomationError::new(
                        "runtime_unavailable",
                        format!("serialize plugin reload report: {error}"),
                    )
                }),
            );
        } else {
            index += 1;
        }
    }
}

fn finish_reload(
    actor: &mpsc::SyncSender<ActorEvent>,
    completion: ReloadCompletion,
    result: Result<Value, AutomationError>,
) {
    match completion {
        ReloadCompletion::Automation(reply) => deliver_query(actor, reply, result),
        ReloadCompletion::Notice => {
            let _ = actor.send(ActorEvent::PluginReloaded { result });
        }
    }
}

fn deliver(
    actor: &mpsc::SyncSender<ActorEvent>,
    completion: Completion,
    result: io::Result<Value>,
) {
    let event = match completion {
        Completion::Automation(reply) => ActorEvent::PluginComplete {
            reply,
            result: result.map_err(crate::session::plugin_automation_error),
        },
        Completion::Notice(reference) => ActorEvent::PluginNotice {
            reference,
            result: result.map(|_| ()).map_err(|error| error.to_string()),
        },
        Completion::Broker(reply) => {
            let _ = reply.try_send(result);
            return;
        }
        Completion::Detached => return,
    };
    let _ = actor.send(event);
}

fn deliver_query(
    actor: &mpsc::SyncSender<ActorEvent>,
    reply: AutomationReplyTarget,
    result: Result<Value, AutomationError>,
) {
    let _ = actor.send(ActorEvent::PluginComplete { reply, result });
}

fn job_lifecycle_context(job: &Job) -> Option<vvmux_plugin_api::InvocationContext> {
    job.context.clone().map(|mut context| {
        context.source = format!("plugin:{}:job-{:016x}", job.plugin_id, job.id);
        context
    })
}

fn job_not_found() -> AutomationError {
    AutomationError::new("job_not_found", "plugin job was not found in this session")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn truncate_log(mut value: String, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}

fn random_identity() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(hex::encode(bytes))
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_identity_is_collision_resistant_and_windows_safe() {
        let first = random_identity().unwrap();
        let second = random_identity().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn broker_tokens_are_scoped_and_revoked_with_the_runtime() {
        let (actor, _events) = mpsc::sync_channel(1);
        let tokens = Arc::new(Mutex::new(HashMap::new()));
        let broker = HostBroker {
            actor,
            manager: {
                let (sender, _receiver) = mpsc::sync_channel(1);
                sender
            },
            session_instance: "session-a".into(),
            tokens: tokens.clone(),
        };
        let lease = broker
            .issue("dev.example", "instance-a", &[Permission::PaneRead])
            .unwrap();
        assert_eq!(lease.token().len(), 64);
        let registered = tokens.lock().unwrap().get(lease.token()).cloned().unwrap();
        assert_eq!(registered.scope.session_instance, "session-a");
        assert_eq!(registered.scope.plugin_id, "dev.example");
        assert_eq!(registered.scope.plugin_instance, "instance-a");
        assert_eq!(registered.scope.permissions, vec![Permission::PaneRead]);
        let token = lease.token().to_owned();
        drop(lease);
        assert!(!tokens.lock().unwrap().contains_key(&token));
    }

    #[test]
    fn declared_limits_match_the_public_contract() {
        assert_eq!(MAX_SESSION_JOBS, 16);
        assert_eq!(MAX_PLUGIN_JOBS, 4);
        assert_eq!(MAX_RETAINED_JOBS, 200);
        assert_eq!(MAX_RETAINED_LOG_BYTES, 256 * 1024);
    }

    #[test]
    fn event_hooks_suppress_self_and_stop_at_depth_eight() {
        let hook = EventHook {
            on: "pane.screen_changed".into(),
            handler: Some("screen".into()),
            command: None,
            include_self: false,
            timeout_ms: 10_000,
        };
        let mut context = vvmux_plugin_api::InvocationContext {
            correlation_id: "correlation".into(),
            causation_id: "cause".into(),
            causation_depth: 1,
            source: "plugin:dev.example:instance-a".into(),
            session_instance: "session-a".into(),
            pane_id: Some(1),
            tab_id: Some(1),
            deadline_unix_ms: 0,
        };
        assert!(!hook_accepts_event("dev.example", &hook, &context));
        assert!(hook_accepts_event("dev.peer", &hook, &context));
        let include_self = EventHook {
            include_self: true,
            ..hook
        };
        assert!(hook_accepts_event("dev.example", &include_self, &context));
        context.causation_depth = 8;
        assert!(!hook_accepts_event("dev.peer", &include_self, &context));
    }

    #[test]
    fn pane_launch_resolution_is_capability_scoped_and_allocates_exact_identity() {
        let plugin = crate::plugin::RuntimePlugin {
            id: "dev.example".into(),
            version: "1.0.0".into(),
            source: "test".into(),
            root: "/package".into(),
            digest: "digest-a".into(),
            manifest_digest: "manifest-a".into(),
            enabled: true,
            permissions: vec![Permission::PaneCreate, Permission::MediaProduce],
            panes: vec![vvmux_plugin_api::Pane {
                id: "dashboard".into(),
                title: "Dashboard".into(),
                placement: vvmux_plugin_api::Placement::Float,
                command: vec!["python".into(), "dashboard.py".into()],
                hold_on_exit: true,
                accept_sync_input: false,
            }],
            activation: Activation::OnDemand,
            events: Vec::new(),
            workflows: Vec::new(),
        };
        let registry = AppliedRegistry {
            generation: 1,
            plugins: [(plugin.id.clone(), plugin)].into_iter().collect(),
            catalog: BTreeMap::new(),
            failures: BTreeMap::new(),
        };
        let launch = resolve_pane_launch(
            &registry,
            &BTreeSet::new(),
            "session-a",
            "dev.example/dashboard",
        )
        .unwrap();
        assert_eq!(launch.scope.session_instance, "session-a");
        assert_eq!(launch.scope.plugin_id, "dev.example");
        assert_eq!(launch.scope.plugin_instance.len(), 32);
        assert_eq!(launch.package_digest, "digest-a");
        assert_eq!(launch.pane.id, "dashboard");

        let mut denied = registry;
        denied
            .plugins
            .get_mut("dev.example")
            .unwrap()
            .permissions
            .clear();
        assert_eq!(
            resolve_pane_launch(
                &denied,
                &BTreeSet::new(),
                "session-a",
                "dev.example/dashboard",
            )
            .unwrap_err()
            .code,
            "capability_denied"
        );
    }

    #[test]
    fn accounting_enforces_plugin_and_session_limits_and_releases_slots() {
        let mut accounting = JobAccounting::default();
        for _ in 0..MAX_PLUGIN_JOBS {
            assert!(accounting.admit("dev.one"));
        }
        assert!(!accounting.admit("dev.one"));
        for index in 0..(MAX_SESSION_JOBS - MAX_PLUGIN_JOBS) {
            assert!(accounting.admit(&format!("dev.plugin{index}")));
        }
        assert!(!accounting.admit("dev.overflow"));
        accounting.complete("dev.one");
        assert!(accounting.admit("dev.one"));
    }

    #[test]
    fn event_workflow_firehose_retains_one_latest_trigger_with_a_gap() {
        let mut pending = BTreeMap::new();
        for sequence in [10, 11, 14] {
            retain_event_workflow_trigger(
                &mut pending,
                pending_event_workflow("dev.bundle", "refresh", sequence),
            );
        }

        assert_eq!(pending.len(), 1);
        let retained = &pending[&("dev.bundle".into(), "refresh".into())];
        assert_eq!(retained.sequence, 14);
        assert_eq!(retained.payload, serde_json::json!({"sequence": 14}));
        assert_eq!(retained.gap, Some((10, 13)));
    }

    #[test]
    fn pending_event_workflow_memory_is_bounded_per_plugin() {
        let mut pending = BTreeMap::new();
        for index in 0..(MAX_PENDING_EVENT_WORKFLOWS_PER_PLUGIN + 32) {
            retain_event_workflow_trigger(
                &mut pending,
                pending_event_workflow("dev.bundle", &format!("workflow-{index}"), index as u64),
            );
        }

        assert_eq!(pending.len(), MAX_PENDING_EVENT_WORKFLOWS_PER_PLUGIN);
    }

    fn pending_event_workflow(
        plugin_id: &str,
        workflow_id: &str,
        sequence: u64,
    ) -> PendingEventWorkflow {
        PendingEventWorkflow {
            plugin_id: plugin_id.into(),
            workflow_id: workflow_id.into(),
            sequence,
            payload: serde_json::json!({"sequence": sequence}),
            context: vvmux_plugin_api::InvocationContext {
                correlation_id: format!("correlation-{sequence}"),
                causation_id: "cause".into(),
                causation_depth: 1,
                source: "session".into(),
                session_instance: "session-a".into(),
                pane_id: Some(1),
                tab_id: Some(1),
                deadline_unix_ms: u64::MAX,
            },
            gap: None,
        }
    }

    #[test]
    fn retained_logs_are_utf8_safe_and_bounded() {
        let value = "é".repeat(MAX_RETAINED_LOG_BYTES);
        let (truncated, was_truncated) = truncate_log(value, MAX_RETAINED_LOG_BYTES);
        assert!(was_truncated);
        assert!(truncated.len() <= MAX_RETAINED_LOG_BYTES);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn detached_completion_history_evicts_the_oldest_record() {
        let mut store = JobStore::default();
        for id in 0..=MAX_RETAINED_JOBS {
            let job = Job {
                id: id as u64,
                public_id: format!("session/instance-{id}"),
                created_ms: id as u128,
                plugin_id: "dev.example".into(),
                reference: "dev.example/action".into(),
                input: serde_json::json!({}),
                context: None,
                cancel: Arc::new(AtomicBool::new(false)),
                completion: Completion::Detached,
                event_workflow: None,
            };
            store.start(&job);
            store.finish(&job, &Ok(serde_json::json!({"id": id})));
        }
        assert_eq!(store.records.len(), MAX_RETAINED_JOBS);
        assert!(!store.records.contains_key("session/instance-0"));
        assert!(
            store
                .records
                .contains_key(&format!("session/instance-{MAX_RETAINED_JOBS}"))
        );
    }
}
