//! Session-scoped plugin scheduling and runtime ownership.
//!
//! The session actor submits bounded work here and never performs manifest loading, schema
//! validation, process startup, or protocol I/O itself. Each plugin gets one deterministic worker
//! and runtime cache; different plugins can progress independently.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use serde_json::Value;

use crate::ipc::AutomationError;
use crate::session::{ActorEvent, AutomationReplyTarget};

const COMMAND_QUEUE: usize = 32;
const PLUGIN_QUEUE: usize = 4;
const MAX_SESSION_JOBS: usize = 16;
const MAX_PLUGIN_JOBS: usize = 4;

#[derive(Clone)]
pub(crate) struct PluginSupervisor {
    sender: mpsc::SyncSender<Message>,
    next_job_id: Arc<AtomicU64>,
}

enum Completion {
    Automation(AutomationReplyTarget),
    Notice(String),
}

struct Job {
    id: u64,
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
    Complete { job: Job, result: io::Result<Value> },
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
        let (sender, receiver) = mpsc::sync_channel(COMMAND_QUEUE);
        let manager_sender = sender.clone();
        thread::Builder::new()
            .name(format!("vvmux-plugin-supervisor-{session_name}"))
            .spawn(move || {
                run_manager(
                    receiver,
                    manager_sender,
                    actor,
                    session_name,
                    session_instance,
                );
            })?;
        Ok(Self {
            sender,
            next_job_id: Arc::new(AtomicU64::new(1)),
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

    fn submit(
        &self,
        reference: String,
        input: Value,
        completion: Completion,
    ) -> Result<(), AutomationError> {
        let plugin_id = reference
            .split_once('/')
            .map(|(plugin, _)| plugin)
            .filter(|plugin| !plugin.is_empty())
            .ok_or_else(|| {
                AutomationError::new("action_not_found", "plugin reference must be ID/ACTION")
            })?
            .to_owned();
        let job = Job {
            id: self.next_job_id.fetch_add(1, Ordering::Relaxed),
            plugin_id,
            reference,
            input,
            cancel: Arc::new(AtomicBool::new(false)),
            completion,
        };
        self.sender.try_send(Message::Invoke(job)).map_err(|error| {
            let message = match error {
                mpsc::TrySendError::Full(_) => "plugin supervisor queue is full",
                mpsc::TrySendError::Disconnected(_) => "plugin supervisor is unavailable",
            };
            AutomationError::new("busy", message)
        })
    }

    pub(crate) fn cancel_client(&self, client_id: u64) {
        let _ = self.sender.try_send(Message::CancelClient(client_id));
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.sender.try_send(Message::Shutdown);
    }
}

fn run_manager(
    receiver: mpsc::Receiver<Message>,
    sender: mpsc::SyncSender<Message>,
    actor: mpsc::SyncSender<ActorEvent>,
    session_name: String,
    session_instance: String,
) {
    let mut workers = HashMap::<String, mpsc::SyncSender<WorkerMessage>>::new();
    let mut active = HashMap::<u64, ActiveJob>::new();
    let mut accounting = JobAccounting::default();
    while let Ok(message) = receiver.recv() {
        match message {
            Message::Invoke(job) => {
                if !accounting.admit(&job.plugin_id) {
                    deliver(
                        &actor,
                        job.completion,
                        Err(io::Error::other("busy: plugin job limit reached")),
                    );
                    continue;
                }
                let worker = if let Some(worker) = workers.get(&job.plugin_id) {
                    worker.clone()
                } else {
                    match spawn_worker(
                        &job.plugin_id,
                        &session_name,
                        &session_instance,
                        sender.clone(),
                    ) {
                        Ok(worker) => {
                            workers.insert(job.plugin_id.clone(), worker.clone());
                            worker
                        }
                        Err(error) => {
                            accounting.complete(&job.plugin_id);
                            deliver(&actor, job.completion, Err(error));
                            continue;
                        }
                    }
                };
                let client_id = match &job.completion {
                    Completion::Automation(reply) => Some(reply.client_id()),
                    Completion::Notice(_) => None,
                };
                let active_job = ActiveJob {
                    plugin_id: job.plugin_id.clone(),
                    client_id,
                    cancel: job.cancel.clone(),
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
                    deliver(
                        &actor,
                        job.completion,
                        Err(io::Error::other(
                            "runtime_unavailable: plugin worker stopped",
                        )),
                    );
                    continue;
                }
                active.insert(job_id, active_job);
            }
            Message::Complete { job, result } => {
                if let Some(active_job) = active.remove(&job.id) {
                    accounting.complete(&active_job.plugin_id);
                }
                deliver(&actor, job.completion, result);
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
    };
    let _ = actor.send(event);
}

fn random_identity() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
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
    fn declared_limits_match_the_public_contract() {
        assert_eq!(MAX_SESSION_JOBS, 16);
        assert_eq!(MAX_PLUGIN_JOBS, 4);
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
}
