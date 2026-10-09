//! Bounded hosted session operations. Local session APIs remain unchanged.
use super::tunnel::ClientControl;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, mpsc};

/// How long a completed metadata request's result is kept for retrieval.
const RETENTION: Duration = Duration::from_secs(600);
/// Completed metadata results kept at once.
const MAX_RESULTS: usize = 256;
struct Operation {
    account: String,
    name: String,
    created: Instant,
    result: Option<Value>,
}
#[derive(Default)]
struct Results {
    entries: HashMap<(String, String), Operation>,
}
enum Begin {
    Cached(Value),
    Run,
}
impl Results {
    fn begin(
        &mut self,
        owner: &str,
        id: &str,
        account: &str,
        action: &str,
        name: Option<&str>,
    ) -> Result<Begin, &'static str> {
        self.entries
            .retain(|_, v| v.created.elapsed() < RETENTION || v.result.is_none());
        let key = (owner.to_owned(), id.to_owned());
        if let Some(value) = self.entries.get(&key) {
            if value.account != account
                || (action != "result" && (action != "create" || name != Some(value.name.as_str())))
            {
                return Err("operation_conflict");
            }
            return Ok(Begin::Cached(
                value
                    .result
                    .clone()
                    .unwrap_or_else(|| json!({"error":"pending"})),
            ));
        }
        if action == "result" {
            return Err("unknown_result");
        }
        if action != "create" {
            return Ok(Begin::Run);
        }
        if self.entries.len() >= MAX_RESULTS {
            return Err("operation_capacity");
        }
        self.entries.insert(
            key,
            Operation {
                account: account.to_owned(),
                name: name.unwrap_or_default().to_owned(),
                created: Instant::now(),
                result: None,
            },
        );
        Ok(Begin::Run)
    }
}
pub(super) struct MetadataService {
    results: Mutex<Results>,
    slots: Arc<Semaphore>,
}
impl MetadataService {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            results: Mutex::new(Results::default()),
            slots: Arc::new(Semaphore::new(4)),
        })
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "the metadata request fields arrive individually from the decoded control frame"
    )]
    pub(super) fn dispatch(
        self: &Arc<Self>,
        request_id: String,
        operation_id: String,
        owner: String,
        account: String,
        action: String,
        name: Option<String>,
        config_path: Option<std::path::PathBuf>,
        reports: mpsc::Sender<ClientControl>,
    ) {
        let reply = |result| {
            let _ = reports.try_send(ClientControl::SessionResult {
                request_id: request_id.clone(),
                result,
            });
        };
        if !valid_id(&request_id)
            || !valid_id(&operation_id)
            || owner.is_empty()
            || owner.len() > 257
            || account.is_empty()
            || account.len() > 385
            || !matches!(action.as_str(), "list" | "create" | "result")
            || name
                .as_ref()
                .is_some_and(|v| v.len() > 128 || crate::runtime::validate_session_name(v).is_err())
            || (action == "create" && name.is_none())
        {
            reply(json!({"error":"invalid_request"}));
            return;
        }
        let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() else {
            reply(json!({"error":"capacity"}));
            return;
        };
        let begin = self
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .begin(&owner, &operation_id, &account, &action, name.as_deref());
        match begin {
            Ok(Begin::Cached(result)) => {
                reply(result);
                return;
            }
            Err(error) => {
                reply(json!({"error":error}));
                return;
            }
            Ok(Begin::Run) => {}
        }
        let service = Arc::clone(self);
        // The worker is deliberately owned beyond the requesting tunnel: dropping a response
        // cannot roll back a dispatched create, and reconnect can query its retained result.
        tokio::spawn(async move {
            let create = action == "create";
            let result=tokio::task::spawn_blocking(move || {
                if create {
                    let name=name.expect("validated creation name");
                    match crate::client::create_detached(&name,config_path.as_deref(),None) {
                        Ok(())=>json!({"name":name,"created":true}),
                        Err(e)=>json!({"error":if e.kind()==std::io::ErrorKind::AlreadyExists {"session_exists"} else {"session_unavailable"}}),
                    }
                } else {
                    match super::list_live_sessions() {
                        Ok(sessions) if sessions.len()<=128=>json!({"sessions":sessions}),
                        _=>json!({"error":"session_directory_unavailable"}),
                    }
                }
            }).await.unwrap_or_else(|_|json!({"error":"operation_uncertain"}));
            let result = if serde_json::to_vec(&result).is_ok_and(|b| b.len() <= 60 * 1024) {
                result
            } else {
                json!({"error":"result_too_large"})
            };
            if create
                && let Some(entry) = service
                    .results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entries
                    .get_mut(&(owner, operation_id))
            {
                entry.result = Some(result.clone());
                entry.created = Instant::now();
            }
            let _ = reports.try_send(ClientControl::SessionResult { request_id, result });
            drop(permit);
        });
    }
}
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uncertain_creation_is_retained_and_owner_isolated() {
        let mut results = Results::default();
        assert!(matches!(
            results.begin(
                "tenant-a/user-a",
                "same-id",
                "account-a",
                "create",
                Some("session")
            ),
            Ok(Begin::Run)
        ));
        assert!(matches!(
            results.begin(
                "tenant-a/user-a",
                "same-id",
                "account-a",
                "create",
                Some("session")
            ),
            Ok(Begin::Cached(_))
        ));
        assert!(
            results
                .begin(
                    "tenant-a/user-a",
                    "same-id",
                    "account-a",
                    "create",
                    Some("different")
                )
                .is_err()
        );
        assert!(
            results
                .begin("tenant-a/user-a", "same-id", "account-b", "result", None)
                .is_err()
        );
        assert!(matches!(
            results.begin(
                "tenant-b/user-b",
                "same-id",
                "account-b",
                "create",
                Some("session")
            ),
            Ok(Begin::Run)
        ));
        assert!(
            results
                .begin("tenant-a/user-a", "unknown", "account-a", "result", None)
                .is_err()
        );
        assert_eq!(results.entries.len(), 2);
    }
    #[test]
    fn pending_mutations_cannot_be_evicted_to_admit_more_work() {
        let mut results = Results::default();
        for i in 0..MAX_RESULTS {
            results
                .begin("owner", &i.to_string(), "account", "create", Some("name"))
                .unwrap();
        }
        assert!(
            results
                .begin("owner", "overflow", "account", "create", Some("name"))
                .is_err()
        );
        results
            .begin("owner", "list", "account", "list", None)
            .unwrap();
    }
}
