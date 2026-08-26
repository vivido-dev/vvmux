//! Advisory, expiring leases that let several agents share one session safely.
//!
//! vvmux is the product where this matters. A Vivido window has one user; a vvmux session
//! routinely has several agents working in different panes, and nothing stopped two of them from
//! typing into the same one. A lease is how one says "this pane is mine for the next minute".
//!
//! Deliberately advisory in one direction only: a caller that holds no lease is never blocked, so
//! nothing that worked before starts failing, and an interactive user is never locked out of their
//! own terminal. What a lease does is exclude *other automation* — once one caller holds `input` on
//! a pane, another caller's input request is refused rather than interleaved.
//!
//! Every lease expires. A crashed agent must not hold a pane forever, and a TTL is the only
//! release that does not depend on the holder still being alive to perform it.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::ipc::{AutomationError, MethodClass};
use crate::layout::PaneId;

/// Leases one session may hold at once.
///
/// Bounded because callers create them: a caller that acquired without ever releasing must not be
/// able to grow this without limit. Expired entries are swept on every access, so the ceiling is
/// only ever reached by live holders.
const MAX_LEASES: usize = 64;
pub const MAX_LEASE_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_HOLDER_BYTES: usize = 64;

/// What a lease excludes other callers from doing.
///
/// Coarse on purpose, and matching [`MethodClass`] so a request's class decides which lease covers
/// it without a second table to keep in step.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum LeaseScope {
    /// Shared. Several agents may watch one pane; watching changes nothing.
    Observe,
    /// Exclusive. Writing to a pane's PTY, including keys, paste, and mouse.
    Input,
    /// Exclusive. Moving, resizing, renaming, zooming, or closing.
    Layout,
    /// Exclusive. Signals and anything else acting on the pane's processes.
    Process,
}

impl LeaseScope {
    /// Whether holding this scope excludes another holder of it.
    pub fn is_exclusive(self) -> bool {
        self != Self::Observe
    }

    /// The scope a method of this class falls under, if any.
    ///
    /// `Agent`, `Plugin`, and `Config` are unleased: an agent method already addresses one agent,
    /// a plugin call is mediated by the plugin host's own permissions, and configuration is
    /// session-wide rather than per pane, so a pane lease would be the wrong shape for it.
    pub fn for_class(class: MethodClass) -> Option<Self> {
        match class {
            MethodClass::Input => Some(Self::Input),
            MethodClass::Pane | MethodClass::Layout => Some(Self::Layout),
            MethodClass::Process => Some(Self::Process),
            MethodClass::Observe
            | MethodClass::Config
            | MethodClass::Agent
            | MethodClass::Lifecycle
            | MethodClass::Plugin => None,
        }
    }
}

#[derive(Debug)]
struct Lease {
    id: String,
    pane_id: PaneId,
    scope: LeaseScope,
    holder: Option<String>,
    expires: Instant,
}

#[derive(Default)]
pub struct Leases {
    leases: Vec<Lease>,
    next_id: u64,
}

impl Leases {
    /// Take a lease, or say who already holds it.
    pub fn acquire(
        &mut self,
        pane_id: PaneId,
        scope: LeaseScope,
        ttl: Duration,
        holder: Option<String>,
        session_instance: &str,
    ) -> Result<serde_json::Value, AutomationError> {
        if let Some(holder) = &holder
            && (holder.is_empty() || holder.len() > MAX_HOLDER_BYTES)
        {
            return Err(AutomationError::new(
                "invalid_params",
                format!("a lease holder name holds 1..={MAX_HOLDER_BYTES} bytes"),
            ));
        }
        if ttl.is_zero() || ttl > MAX_LEASE_TTL {
            return Err(AutomationError::new(
                "invalid_params",
                format!(
                    "a lease lives 1ms..={}s; it must expire so a crashed holder cannot keep a \
                     pane forever",
                    MAX_LEASE_TTL.as_secs()
                ),
            ));
        }
        self.sweep();
        if scope.is_exclusive()
            && let Some(existing) = self
                .leases
                .iter()
                .find(|lease| lease.pane_id == pane_id && lease.scope == scope)
        {
            return Err(AutomationError::new(
                "lease_denied",
                format!(
                    "pane {pane_id} is already leased for {} by {}",
                    serde_json::to_string(&scope)
                        .unwrap_or_default()
                        .trim_matches('"'),
                    existing.holder.as_deref().unwrap_or("another caller"),
                ),
            ));
        }
        if self.leases.len() >= MAX_LEASES {
            return Err(AutomationError::new(
                "limit_exceeded",
                format!("a session holds at most {MAX_LEASES} leases"),
            ));
        }
        self.next_id = self.next_id.saturating_add(1);
        let id = format!("{session_instance}-lease-{:08x}", self.next_id);
        let expires = Instant::now() + ttl;
        self.leases.push(Lease {
            id: id.clone(),
            pane_id,
            scope,
            holder,
            expires,
        });
        Ok(serde_json::json!({
            "lease_id": id,
            "pane_id": pane_id,
            "scope": scope,
            "expires_in_ms": ttl.as_millis() as u64,
        }))
    }

    pub fn renew(&mut self, id: &str, ttl: Duration) -> Result<serde_json::Value, AutomationError> {
        if ttl.is_zero() || ttl > MAX_LEASE_TTL {
            return Err(AutomationError::new(
                "invalid_params",
                format!("a lease lives 1ms..={}s", MAX_LEASE_TTL.as_secs()),
            ));
        }
        self.sweep();
        let lease = self
            .leases
            .iter_mut()
            .find(|lease| lease.id == id)
            // An expired lease is gone rather than renewable: the pane may already have been taken
            // by somebody else, and silently reviving it would hand out a second exclusive hold.
            .ok_or_else(|| {
                AutomationError::new("lease_not_found", format!("no live lease `{id}`"))
            })?;
        lease.expires = Instant::now() + ttl;
        Ok(serde_json::json!({
            "lease_id": lease.id,
            "pane_id": lease.pane_id,
            "scope": lease.scope,
            "expires_in_ms": ttl.as_millis() as u64,
        }))
    }

    pub fn release(&mut self, id: &str) -> Result<serde_json::Value, AutomationError> {
        self.sweep();
        let index = self
            .leases
            .iter()
            .position(|lease| lease.id == id)
            .ok_or_else(|| {
                AutomationError::new("lease_not_found", format!("no live lease `{id}`"))
            })?;
        let lease = self.leases.remove(index);
        Ok(serde_json::json!({
            "lease_id": lease.id,
            "pane_id": lease.pane_id,
            "scope": lease.scope,
            "released": true,
        }))
    }

    pub fn list(&mut self) -> serde_json::Value {
        self.sweep();
        let now = Instant::now();
        serde_json::json!({
            "leases": self
                .leases
                .iter()
                .map(|lease| serde_json::json!({
                    "lease_id": lease.id,
                    "pane_id": lease.pane_id,
                    "scope": lease.scope,
                    "holder": lease.holder,
                    "expires_in_ms": lease.expires.saturating_duration_since(now).as_millis() as u64,
                }))
                .collect::<Vec<_>>(),
        })
    }

    /// Whether a request may act, given the lease it presented.
    ///
    /// A caller holding the right lease proceeds. A caller holding none proceeds too, unless
    /// somebody else holds an exclusive lease on that pane and scope — which is the whole point:
    /// leases exclude other automation without making themselves mandatory.
    pub fn check(
        &mut self,
        pane_id: Option<PaneId>,
        class: MethodClass,
        presented: Option<&str>,
    ) -> Result<(), AutomationError> {
        self.sweep();
        if let Some(id) = presented
            && !self.leases.iter().any(|lease| lease.id == id)
        {
            return Err(AutomationError::new(
                "lease_not_found",
                format!("lease `{id}` has expired or was released"),
            ));
        }
        let (Some(pane_id), Some(scope)) = (pane_id, LeaseScope::for_class(class)) else {
            return Ok(());
        };
        let Some(holder) = self.leases.iter().find(|lease| {
            lease.pane_id == pane_id && lease.scope == scope && lease.scope.is_exclusive()
        }) else {
            return Ok(());
        };
        if presented == Some(holder.id.as_str()) {
            return Ok(());
        }
        Err(AutomationError::new(
            "lease_denied",
            format!(
                "pane {pane_id} is leased for {} by {}; pass --lease to act under it",
                serde_json::to_string(&scope)
                    .unwrap_or_default()
                    .trim_matches('"'),
                holder.holder.as_deref().unwrap_or("another caller"),
            ),
        ))
    }

    /// Drop leases on a pane that no longer exists.
    pub fn forget_pane(&mut self, pane_id: PaneId) {
        self.leases.retain(|lease| lease.pane_id != pane_id);
    }

    fn sweep(&mut self) {
        let now = Instant::now();
        self.leases.retain(|lease| lease.expires > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(60);

    fn leases() -> Leases {
        Leases::default()
    }

    #[test]
    fn an_exclusive_scope_admits_one_holder_and_observe_admits_many() {
        let mut leases = leases();
        let first = leases
            .acquire(1, LeaseScope::Input, DEFAULT_LEASE_TTL, None, "test")
            .unwrap();
        assert!(
            leases
                .acquire(1, LeaseScope::Input, DEFAULT_LEASE_TTL, None, "test")
                .is_err()
        );
        // A different pane and a different scope are both free.
        assert!(
            leases
                .acquire(2, LeaseScope::Input, DEFAULT_LEASE_TTL, None, "test")
                .is_ok()
        );
        assert!(
            leases
                .acquire(1, LeaseScope::Layout, DEFAULT_LEASE_TTL, None, "test")
                .is_ok()
        );
        // Observation is shared, because watching a pane changes nothing about it.
        for _ in 0..3 {
            assert!(
                leases
                    .acquire(1, LeaseScope::Observe, DEFAULT_LEASE_TTL, None, "test")
                    .is_ok()
            );
        }
        let id = first["lease_id"].as_str().unwrap().to_owned();
        assert!(leases.release(&id).is_ok());
        assert!(
            leases
                .acquire(1, LeaseScope::Input, DEFAULT_LEASE_TTL, None, "test")
                .is_ok(),
            "releasing did not free the pane"
        );
    }

    #[test]
    fn a_lease_excludes_other_automation_but_never_an_unleased_pane() {
        let mut leases = leases();
        // Nothing held: every class is allowed, which is what keeps leases advisory.
        for class in [
            MethodClass::Input,
            MethodClass::Layout,
            MethodClass::Process,
        ] {
            assert!(leases.check(Some(1), class, None).is_ok());
        }
        let held = leases
            .acquire(1, LeaseScope::Input, DEFAULT_LEASE_TTL, None, "test")
            .unwrap();
        let id = held["lease_id"].as_str().unwrap();

        assert!(leases.check(Some(1), MethodClass::Input, Some(id)).is_ok());
        assert!(leases.check(Some(1), MethodClass::Input, None).is_err());
        // Only the leased scope is excluded; an observation and an unrelated pane still pass.
        assert!(leases.check(Some(1), MethodClass::Observe, None).is_ok());
        assert!(leases.check(Some(2), MethodClass::Input, None).is_ok());
        // A scope with no lease on this pane is unaffected.
        assert!(leases.check(Some(1), MethodClass::Layout, None).is_ok());
    }

    #[test]
    fn an_expired_lease_frees_its_pane_and_cannot_be_renewed() {
        let mut leases = leases();
        let held = leases
            .acquire(1, LeaseScope::Input, Duration::from_millis(1), None, "test")
            .unwrap();
        let id = held["lease_id"].as_str().unwrap().to_owned();
        std::thread::sleep(Duration::from_millis(5));
        // The holder is gone, so the pane is free again. This is the only release that does not
        // need the holder to still be alive.
        assert!(leases.check(Some(1), MethodClass::Input, None).is_ok());
        assert!(leases.renew(&id, DEFAULT_LEASE_TTL).is_err());
        assert!(leases.list()["leases"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_ttl_is_required_and_bounded() {
        let mut leases = leases();
        assert!(
            leases
                .acquire(1, LeaseScope::Input, Duration::ZERO, None, "test")
                .is_err()
        );
        assert!(
            leases
                .acquire(1, LeaseScope::Input, MAX_LEASE_TTL * 2, None, "test")
                .is_err()
        );
    }
}
