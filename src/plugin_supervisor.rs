//! Session-scoped plugin scheduling and runtime ownership.
//!
//! The session actor submits bounded work here and never performs manifest loading, schema
//! validation, process startup, or protocol I/O itself. Each plugin gets one deterministic worker
//! and runtime cache; different plugins can progress independently.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use vvmux_plugin_api::{HostCall, Permission};

use crate::ipc::AutomationError;
use crate::session::{ActorEvent, AutomationReplyTarget};

const COMMAND_QUEUE: usize = 32;
const PLUGIN_QUEUE: usize = 4;
const MAX_SESSION_JOBS: usize = 16;
const MAX_PLUGIN_JOBS: usize = 4;
const MAX_RETAINED_JOBS: usize = 200;
const MAX_RETAINED_LOG_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct PluginSupervisor {
    sender: mpsc::SyncSender<Message>,
    next_job_id: Arc<AtomicU64>,
    session_name: String,
    session_instance: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RuntimeScope {
    pub(crate) session_instance: String,
    pub(crate) plugin_id: String,
    pub(crate) plugin_instance: String,
    pub(crate) permissions: Vec<Permission>,
}

#[derive(Clone)]
pub(crate) struct HostBroker {
    actor: mpsc::SyncSender<ActorEvent>,
    session_instance: String,
    tokens: Arc<Mutex<HashMap<String, RuntimeScope>>>,
}

pub(crate) struct BrokerLease {
    token: String,
    scope: RuntimeScope,
    broker: HostBroker,
}

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
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(token.clone(), scope.clone());
        Ok(BrokerLease {
            token,
            scope,
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
        let valid = self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(token)
            .is_some_and(|registered| registered == scope);
        if !valid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "scope_denied: stale plugin broker token",
            ));
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        self.actor
            .try_send(ActorEvent::PluginHostCall {
                scope: scope.clone(),
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
    Detached,
}

struct Job {
    id: u64,
    public_id: String,
    created_ms: u128,
    plugin_id: String,
    reference: String,
    input: Value,
    cancel: Arc<AtomicBool>,
    completion: Completion,
}

struct ActiveJob {
    plugin_id: String,
    client_id: Option<u64>,
    cancel: Arc<AtomicBool>,
    public_id: String,
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
}

#[derive(Default)]
struct JobAccounting {
    total: usize,
    per_plugin: HashMap<String, usize>,
}

impl JobAccounting {
    fn admit(&mut self, plugin_id: &str) -> bool {
        let plugin_jobs = self.per_plugin.get(plugin_id).copied().unwrap_or(0);
        if self.total >= MAX_SESSION_JOBS || plugin_jobs >= MAX_PLUGIN_JOBS {
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
    Complete {
        job: Job,
        result: io::Result<Value>,
    },
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
    CancelClient(u64),
    Shutdown,
}

enum WorkerMessage {
    Invoke(Job),
    Shutdown,
}

impl PluginSupervisor {
    pub(crate) fn start(
        session_name: String,
        actor: mpsc::SyncSender<ActorEvent>,
    ) -> io::Result<Self> {
        let session_instance = random_identity()?;
        let broker = HostBroker {
            actor: actor.clone(),
            session_instance: session_instance.clone(),
            tokens: Arc::new(Mutex::new(HashMap::new())),
        };
        let (sender, receiver) = mpsc::sync_channel(COMMAND_QUEUE);
        let manager_sender = sender.clone();
        let manager_session_name = session_name.clone();
        let manager_session_instance = session_instance.clone();
        thread::Builder::new()
            .name(format!("vvmux-plugin-supervisor-{session_name}"))
            .spawn(move || {
                run_manager(
                    receiver,
                    manager_sender,
                    actor,
                    manager_session_name,
                    manager_session_instance,
                    broker,
                );
            })?;
        Ok(Self {
            sender,
            next_job_id: Arc::new(AtomicU64::new(1)),
            session_name,
            session_instance,
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
            cancel: Arc::new(AtomicBool::new(false)),
            completion,
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

    pub(crate) fn shutdown(&self) {
        let _ = self.sender.try_send(Message::Shutdown);
    }

    pub(crate) fn session_instance(&self) -> &str {
        &self.session_instance
    }
}

fn run_manager(
    receiver: mpsc::Receiver<Message>,
    sender: mpsc::SyncSender<Message>,
    actor: mpsc::SyncSender<ActorEvent>,
    session_name: String,
    session_instance: String,
    broker: HostBroker,
) {
    let mut workers = HashMap::<String, mpsc::SyncSender<WorkerMessage>>::new();
    let mut active = HashMap::<u64, ActiveJob>::new();
    let mut accounting = JobAccounting::default();
    let mut retained = JobStore::default();
    while let Ok(message) = receiver.recv() {
        match message {
            Message::Invoke(job) => {
                retained.start(&job);
                if !accounting.admit(&job.plugin_id) {
                    let result = Err(io::Error::other("busy: plugin job limit reached"));
                    retained.finish(&job, &result);
                    deliver(&actor, job.completion, result);
                    continue;
                }
                let worker = if let Some(worker) = workers.get(&job.plugin_id) {
                    worker.clone()
                } else {
                    match spawn_worker(
                        &job.plugin_id,
                        &session_name,
                        &session_instance,
                        broker.clone(),
                        sender.clone(),
                    ) {
                        Ok(worker) => {
                            workers.insert(job.plugin_id.clone(), worker.clone());
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
                    Completion::Notice(_) | Completion::Detached => None,
                };
                let active_job = ActiveJob {
                    plugin_id: job.plugin_id.clone(),
                    client_id,
                    cancel: job.cancel.clone(),
                    public_id: job.public_id.clone(),
                };
                let job_id = job.id;
                if let Err(error) = worker.try_send(WorkerMessage::Invoke(job)) {
                    let job = match error {
                        mpsc::TrySendError::Full(WorkerMessage::Invoke(job))
                        | mpsc::TrySendError::Disconnected(WorkerMessage::Invoke(job)) => job,
                        mpsc::TrySendError::Full(WorkerMessage::Shutdown)
                        | mpsc::TrySendError::Disconnected(WorkerMessage::Shutdown) => {
                            unreachable!("the supervisor only submits invocation messages here")
                        }
                    };
                    workers.remove(&active_job.plugin_id);
                    accounting.complete(&active_job.plugin_id);
                    let result = Err(io::Error::other(
                        "runtime_unavailable: plugin worker stopped",
                    ));
                    retained.finish(&job, &result);
                    deliver(&actor, job.completion, result);
                    continue;
                }
                active.insert(job_id, active_job);
            }
            Message::Complete { job, result } => {
                if let Some(active_job) = active.remove(&job.id) {
                    accounting.complete(&active_job.plugin_id);
                }
                retained.finish(&job, &result);
                deliver(&actor, job.completion, result);
            }
            Message::JobStatus { job_id, reply } => {
                deliver_query(&actor, reply, retained.status(&job_id));
            }
            Message::JobCancel { job_id, reply } => {
                let result = if let Some(job) = active
                    .values()
                    .find(|active_job| active_job.public_id == job_id)
                {
                    job.cancel.store(true, Ordering::Release);
                    Ok(serde_json::json!({"job_id": job_id, "status": "cancelling"}))
                } else {
                    retained.status(&job_id)
                };
                deliver_query(&actor, reply, result);
            }
            Message::JobLogs { job_id, reply } => {
                deliver_query(&actor, reply, retained.logs(&job_id));
            }
            Message::CancelClient(client_id) => {
                for job in active
                    .values()
                    .filter(|job| job.client_id == Some(client_id))
                {
                    job.cancel.store(true, Ordering::Release);
                }
            }
            Message::Shutdown => {
                for job in active.values() {
                    job.cancel.store(true, Ordering::Release);
                }
                for worker in workers.values() {
                    let _ = worker.try_send(WorkerMessage::Shutdown);
                }
                break;
            }
        }
    }
}

fn spawn_worker(
    plugin_id: &str,
    session_name: &str,
    session_instance: &str,
    broker: HostBroker,
    manager: mpsc::SyncSender<Message>,
) -> io::Result<mpsc::SyncSender<WorkerMessage>> {
    let (sender, receiver) = mpsc::sync_channel(PLUGIN_QUEUE);
    let plugin_id = plugin_id.to_owned();
    let session_name = session_name.to_owned();
    let session_instance = session_instance.to_owned();
    let mut runtime = crate::plugin::SessionPluginRuntime::new(
        session_name,
        session_instance,
        plugin_id.clone(),
        broker,
    )?;
    thread::Builder::new()
        .name(format!("vvmux-plugin-{plugin_id}"))
        .spawn(move || {
            while let Ok(message) = receiver.recv() {
                match message {
                    WorkerMessage::Invoke(job) => {
                        let result = runtime.invoke(&job.reference, job.input.clone(), &job.cancel);
                        if manager.send(Message::Complete { job, result }).is_err() {
                            break;
                        }
                    }
                    WorkerMessage::Shutdown => break,
                }
            }
        })?;
    Ok(sender)
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
            session_instance: "session-a".into(),
            tokens: tokens.clone(),
        };
        let lease = broker
            .issue("dev.example", "instance-a", &[Permission::PaneRead])
            .unwrap();
        assert_eq!(lease.token().len(), 64);
        let registered = tokens.lock().unwrap().get(lease.token()).cloned().unwrap();
        assert_eq!(registered.session_instance, "session-a");
        assert_eq!(registered.plugin_id, "dev.example");
        assert_eq!(registered.plugin_instance, "instance-a");
        assert_eq!(registered.permissions, vec![Permission::PaneRead]);
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
                cancel: Arc::new(AtomicBool::new(false)),
                completion: Completion::Detached,
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
