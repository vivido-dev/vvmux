//! Passive and explicitly reported AI-agent state for terminal panes.
//!
//! Process discovery and terminal rules are adapted from HerdR commit
//! 6c6ddcd49384d6ea9f0ee2e63bf7b2643dfd5bcf (Apache-2.0). See
//! `agent/PROVENANCE.md` for the source inventory and adaptation notes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use vvmux_plugin_api::{Agent as AgentDefinition, AgentGate, AgentRuleState};
use vvmux_terminal::Terminal;
use vvmux_terminal::pty::PtyControl;

use crate::layout::PaneId;

pub const MAX_REPORT_SOURCE_BYTES: usize = 128;
/// Distinct reporting sources one pane retains sequence state for.
///
/// The sequence map is keyed by a caller-chosen string, so without a ceiling a chatty or hostile
/// reporter could grow it without bound.
pub const MAX_REPORT_SOURCES: usize = 32;
/// Block reason carried by a report and shown beside a blocked agent.
pub const MAX_REPORT_MESSAGE_BYTES: usize = 256;
/// Native agent session identifier or path retained for a later resume.
pub const MAX_AGENT_SESSION_BYTES: usize = 256;
/// Display-only tokens one pane retains.
pub const MAX_METADATA_TOKENS: usize = 16;
pub const MAX_METADATA_KEY_BYTES: usize = 32;
pub const MAX_METADATA_VALUE_BYTES: usize = 128;
/// Custom status names, one per [`AgentStatus`].
pub const MAX_METADATA_STATE_LABELS: usize = 4;
pub const MAX_DISPLAY_AGENT_BYTES: usize = 64;
pub const MAX_AGENT_TITLE_BYTES: usize = 128;
/// Matches the automation timeout ceiling, so a TTL cannot outlive what a caller can wait for.
pub const MAX_METADATA_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Bottom-buffer text agent classification runs against.
const MAX_DETECTION_BYTES: usize = 64 * 1024;
/// Region excerpt returned per rule by `agent-explain`.
const MAX_REGION_PREVIEW_BYTES: usize = 256;
const IDLE_CONFIRMATIONS: u8 = 3;
const IDLE_CONFIRMATION_LIMIT: Duration = Duration::from_millis(700);
const IDLE_CONFIRMATION_RECHECK: Duration = Duration::from_millis(100);
const STARTUP_GRACE: Duration = Duration::from_secs(3);
const FULL_PROCESS_RECHECK: Duration = Duration::from_secs(5);
#[cfg(unix)]
const MAX_PROCESS_ARGV_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        valid
            .then_some(Self(value))
            .ok_or("agent ID must be a local identifier of 1..=64 bytes")
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for AgentId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for AgentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

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
    Hash,
)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
}

impl FromStr for AgentStatus {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "idle" => Ok(Self::Idle),
            "working" => Ok(Self::Working),
            "blocked" => Ok(Self::Blocked),
            "done" => Ok(Self::Done),
            _ => Err("agent status must be idle, working, blocked, or done"),
        }
    }
}

impl AgentStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }

    pub fn urgency(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Done => 1,
            Self::Working => 2,
            Self::Idle => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    Screen,
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pub id: AgentId,
    pub name: String,
    pub provider: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub kind: AgentId,
    pub label: String,
    pub provider: String,
    pub state: AgentState,
    pub status: AgentStatus,
    pub source: AgentSource,
    /// Why the agent is blocked, as reported by its lifecycle integration.
    pub message: Option<String>,
    /// Whether a native session reference has been reported for this agent.
    ///
    /// Only presence travels in a snapshot. The reference itself is withheld; see
    /// [`AgentSessionRef`].
    pub session_present: bool,
}

/// A native agent session reference reported by a lifecycle integration.
///
/// Capability-adjacent: it names a resumable conversation belonging to the user's agent account,
/// so it is disclosed only through an explicit single-pane inspect. Every other surface —
/// `list-panes`, plugin `session.inspect`, `diagnose`, and the debug bundle built from it —
/// reports presence alone.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentSessionRef {
    id: Option<String>,
    path: Option<String>,
}

impl AgentSessionRef {
    /// Build a reference, or `None` when neither half was reported.
    pub fn new(id: Option<String>, path: Option<String>) -> Option<Self> {
        (id.is_some() || path.is_some()).then_some(Self { id, path })
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    fn validate(&self) -> Result<(), &'static str> {
        let within_bound = |value: &Option<String>| {
            value
                .as_ref()
                .is_none_or(|value| !value.is_empty() && value.len() <= MAX_AGENT_SESSION_BYTES)
        };
        (within_bound(&self.id) && within_bound(&self.path))
            .then_some(())
            .ok_or("agent session identity must contain 1..=256 bytes")
    }
}

/// Redacted on purpose. This type exists to be withheld, and a derived `Debug` would leak it
/// through any diagnostic that formats a pane, a runtime, or an actor event.
impl fmt::Debug for AgentSessionRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentSessionRef")
            .field("id", &self.id.is_some())
            .field("path", &self.path.is_some())
            .finish()
    }
}

/// Display-only annotations an integration attaches to a pane.
///
/// Adapted from HerdR's `src/metadata_tokens.rs` (Apache-2.0); see `agent/PROVENANCE.md`.
///
/// Deliberately outside [`AgentSnapshot`]: these are presentation details that can change on every
/// tool call ("indexing 42 files"), while a snapshot is the lifecycle fact that waiters and
/// notifications react to. Folding one into the other would turn a progress counter into a stream
/// of status transitions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMetadata {
    tokens: BTreeMap<String, MetadataToken>,
    display_agent: Option<String>,
    state_labels: BTreeMap<AgentStatus, String>,
    title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataToken {
    value: String,
    expires_at: Option<Instant>,
}

/// One display-only metadata update. A `None` value clears that field; a field absent from the
/// patch is left alone, so an integration can update one token without restating the rest.
#[derive(Debug, Clone, Default)]
pub struct AgentMetadataPatch {
    pub tokens: Vec<(String, Option<String>)>,
    pub ttl: Option<Duration>,
    pub display_agent: Option<Option<String>>,
    pub state_labels: Vec<(AgentStatus, Option<String>)>,
    pub title: Option<Option<String>>,
}

impl AgentMetadata {
    pub fn tokens(&self) -> impl Iterator<Item = (&str, &str)> {
        self.tokens
            .iter()
            .map(|(key, token)| (key.as_str(), token.value.as_str()))
    }

    pub fn display_agent(&self) -> Option<&str> {
        self.display_agent.as_deref()
    }

    pub fn state_label(&self, status: AgentStatus) -> Option<&str> {
        self.state_labels.get(&status).map(String::as_str)
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
            && self.display_agent.is_none()
            && self.state_labels.is_empty()
            && self.title.is_none()
    }

    /// The earliest token expiry, so the actor can schedule a wake instead of polling.
    pub fn next_expiry(&self) -> Option<Instant> {
        self.tokens
            .values()
            .filter_map(|token| token.expires_at)
            .min()
    }

    /// Drop every token whose TTL has elapsed. Returns whether anything was removed.
    pub fn expire_at(&mut self, now: Instant) -> bool {
        let before = self.tokens.len();
        self.tokens
            .retain(|_, token| token.expires_at.is_none_or(|deadline| deadline > now));
        self.tokens.len() != before
    }

    /// Validate a patch against every bound before any of it is applied, so a rejected update
    /// leaves the previous display intact rather than half-replacing it.
    fn validate(&self, patch: &AgentMetadataPatch) -> Result<(), &'static str> {
        let mut keys = self.tokens.keys().cloned().collect::<BTreeSet<_>>();
        for (key, value) in &patch.tokens {
            if key.is_empty()
                || key.len() > MAX_METADATA_KEY_BYTES
                || key.chars().any(char::is_control)
            {
                return Err("metadata token name must contain 1..=32 printable bytes");
            }
            match value {
                Some(value) => {
                    if value.len() > MAX_METADATA_VALUE_BYTES || value.chars().any(char::is_control)
                    {
                        return Err(
                            "metadata token value must contain at most 128 printable bytes",
                        );
                    }
                    keys.insert(key.clone());
                }
                None => {
                    keys.remove(key);
                }
            }
        }
        if keys.len() > MAX_METADATA_TOKENS {
            return Err("metadata token limit reached");
        }
        let bounded = |value: &Option<Option<String>>, limit: usize| {
            value
                .as_ref()
                .and_then(Option::as_ref)
                .is_none_or(|value| value.len() <= limit && !value.chars().any(char::is_control))
        };
        if !bounded(&patch.display_agent, MAX_DISPLAY_AGENT_BYTES) {
            return Err("metadata display agent must contain at most 64 printable bytes");
        }
        if !bounded(&patch.title, MAX_AGENT_TITLE_BYTES) {
            return Err("metadata title must contain at most 128 printable bytes");
        }
        if patch.state_labels.iter().any(|(_, label)| {
            label.as_ref().is_some_and(|label| {
                label.is_empty()
                    || label.len() > MAX_METADATA_VALUE_BYTES
                    || label.chars().any(char::is_control)
            })
        }) {
            return Err("metadata state label must contain 1..=128 printable bytes");
        }
        if patch.state_labels.len() > MAX_METADATA_STATE_LABELS {
            return Err("metadata state label limit reached");
        }
        Ok(())
    }

    /// Apply a validated patch. Returns whether the rendered display actually changed.
    fn apply(&mut self, patch: AgentMetadataPatch, now: Instant) -> bool {
        let expires_at = patch.ttl.and_then(|ttl| now.checked_add(ttl));
        let mut changed = false;
        for (key, value) in patch.tokens {
            match value {
                Some(value) => {
                    let token = MetadataToken { value, expires_at };
                    changed |= self.tokens.insert(key, token.clone()).as_ref() != Some(&token);
                }
                None => changed |= self.tokens.remove(&key).is_some(),
            }
        }
        if let Some(display_agent) = patch.display_agent {
            changed |= self.display_agent != display_agent;
            self.display_agent = display_agent;
        }
        if let Some(title) = patch.title {
            changed |= self.title != title;
            self.title = title;
        }
        for (status, label) in patch.state_labels {
            match label {
                Some(label) => {
                    changed |= self.state_labels.insert(status, label.clone()) != Some(label)
                }
                None => changed |= self.state_labels.remove(&status).is_some(),
            }
        }
        changed
    }
}

/// One authoritative lifecycle report from a named source.
#[derive(Debug, Clone)]
pub struct AgentReport {
    pub identity: AgentIdentity,
    pub state: AgentState,
    pub source: String,
    pub sequence: u64,
    pub message: Option<String>,
    /// Reported once by integrations that know it; a later state-only report from the same source
    /// leaves the stored reference in place rather than erasing it.
    pub session: Option<AgentSessionRef>,
}

#[derive(Debug, Clone)]
struct ReportedState {
    source: String,
    state: AgentState,
    sequence: u64,
    message: Option<String>,
}

#[derive(Debug)]
pub struct AgentRuntime {
    identity: Option<AgentIdentity>,
    process_group: Option<u32>,
    state: AgentState,
    source: AgentSource,
    done: bool,
    identified_at: Instant,
    pending_idle: Option<(Instant, u8)>,
    report: Option<ReportedState>,
    report_sequences: HashMap<String, u64>,
    session: Option<AgentSessionRef>,
    metadata: AgentMetadata,
}

impl AgentRuntime {
    pub fn new() -> Self {
        Self {
            identity: None,
            process_group: None,
            state: AgentState::Idle,
            source: AgentSource::Screen,
            done: false,
            identified_at: Instant::now(),
            pending_idle: None,
            report: None,
            report_sequences: HashMap::new(),
            session: None,
            metadata: AgentMetadata::default(),
        }
    }

    pub fn snapshot(&self) -> Option<AgentSnapshot> {
        let identity = self.identity.as_ref()?;
        let status = match self.state {
            AgentState::Idle if self.done => AgentStatus::Done,
            AgentState::Idle => AgentStatus::Idle,
            AgentState::Working => AgentStatus::Working,
            AgentState::Blocked => AgentStatus::Blocked,
        };
        Some(AgentSnapshot {
            kind: identity.id.clone(),
            label: identity.name.clone(),
            provider: identity.provider.clone(),
            state: self.state,
            status,
            source: self.source,
            message: self
                .report
                .as_ref()
                .and_then(|report| report.message.clone()),
            session_present: self.session.is_some(),
        })
    }

    /// The native session reference reported for this agent, if any.
    ///
    /// Callers must treat the result as capability-adjacent: see [`AgentSessionRef`].
    pub fn session(&self) -> Option<&AgentSessionRef> {
        self.session.as_ref()
    }

    pub fn metadata(&self) -> &AgentMetadata {
        &self.metadata
    }

    /// Attach display-only metadata from a named source.
    ///
    /// Shares the report sequence table, so metadata and state from one integration cannot be
    /// applied out of order relative to each other. Accepted even before the agent is detected:
    /// an integration that starts reporting during startup grace keeps its annotations, which
    /// surface once the process is identified.
    pub fn report_metadata(
        &mut self,
        source: &str,
        sequence: u64,
        patch: AgentMetadataPatch,
        now: Instant,
    ) -> Result<bool, &'static str> {
        self.check_sequence(source, sequence)?;
        if let Some(ttl) = patch.ttl
            && (ttl.is_zero() || ttl > Duration::from_millis(MAX_METADATA_TTL_MS))
        {
            return Err("metadata TTL must be from 1ms through 24h");
        }
        self.metadata.validate(&patch)?;
        self.report_sequences.insert(source.to_owned(), sequence);
        Ok(self.metadata.apply(patch, now))
    }

    /// Drop expired tokens. Returns whether the rendered display changed.
    pub fn expire_metadata(&mut self, now: Instant) -> bool {
        self.metadata.expire_at(now)
    }

    pub fn next_evaluation_delay(&self, now: Instant) -> Option<Duration> {
        self.identity.as_ref()?;
        if self.report.is_some() {
            return None;
        }
        let grace_end = self.identified_at + STARTUP_GRACE;
        if now < grace_end {
            return Some(grace_end.saturating_duration_since(now));
        }
        self.pending_idle
            .is_some()
            .then_some(IDLE_CONFIRMATION_RECHECK)
    }

    /// Returns true when stale OSC evidence must be discarded.
    pub fn observe_process(&mut self, group: Option<u32>, identity: Option<AgentIdentity>) -> bool {
        let changed_group = self.process_group != group;
        if changed_group {
            let startup_report_matches = self.process_group.is_none()
                && self.report.is_some()
                && identity.is_some()
                && identity.as_ref().map(|value| &value.id)
                    == self.identity.as_ref().map(|value| &value.id)
                && self.identified_at.elapsed() < STARTUP_GRACE;
            if startup_report_matches {
                self.process_group = group;
                return false;
            }
            self.process_group = group;
            self.report = None;
            self.report_sequences.clear();
            self.session = None;
            self.metadata = AgentMetadata::default();
            self.pending_idle = None;
            self.done = false;
            self.identity = identity;
            self.state = AgentState::Idle;
            self.source = AgentSource::Screen;
            self.identified_at = Instant::now();
            return true;
        }
        if let Some(identity) = identity {
            if self.identity.as_ref() != Some(&identity) {
                self.report = None;
                self.session = None;
                self.metadata = AgentMetadata::default();
                self.pending_idle = None;
                self.done = false;
                self.state = AgentState::Idle;
                self.source = AgentSource::Screen;
                self.identified_at = Instant::now();
            }
            self.identity = Some(identity);
        } else if self.identity.is_some() {
            self.report = None;
            self.report_sequences.clear();
            self.session = None;
            self.metadata = AgentMetadata::default();
            self.pending_idle = None;
            self.identity = None;
            self.done = false;
            self.state = AgentState::Idle;
            self.source = AgentSource::Screen;
        }
        false
    }

    pub fn report(&mut self, report: AgentReport, visible: bool) -> Result<(), &'static str> {
        let AgentReport {
            identity,
            state,
            source,
            sequence,
            message,
            session,
        } = report;
        self.check_sequence(&source, sequence)?;
        if message
            .as_ref()
            .is_some_and(|message| message.is_empty() || message.len() > MAX_REPORT_MESSAGE_BYTES)
        {
            return Err("agent report message must contain 1..=256 bytes");
        }
        if let Some(session) = &session {
            session.validate()?;
        }
        if self
            .identity
            .as_ref()
            .is_some_and(|detected| detected.id != identity.id)
        {
            return Err("reported agent does not match the pane foreground process");
        }
        self.identity = Some(identity);
        self.report_sequences.insert(source.clone(), sequence);
        // A state-only report from an integration that already sent its session identity must not
        // erase it: identity is reported once, state repeatedly.
        if session.is_some() {
            self.session = session;
        }
        self.report = Some(ReportedState {
            source,
            state,
            sequence,
            message,
        });
        self.pending_idle = None;
        self.commit(state, AgentSource::Report, visible);
        Ok(())
    }

    pub fn clear_report(&mut self, source: &str, sequence: u64) -> Result<(), &'static str> {
        self.check_sequence(source, sequence)?;
        self.report_sequences.insert(source.to_owned(), sequence);
        if self
            .report
            .as_ref()
            .is_some_and(|report| report.source == source && sequence > report.sequence)
        {
            self.report = None;
            self.source = AgentSource::Screen;
        }
        Ok(())
    }

    /// Validate a source name and its sequence without recording either.
    ///
    /// The count ceiling is checked here rather than at insertion so a report that is going to be
    /// rejected for another reason cannot claim one of the bounded source slots.
    fn check_sequence(&self, source: &str, sequence: u64) -> Result<(), &'static str> {
        if source.is_empty() || source.len() > MAX_REPORT_SOURCE_BYTES {
            return Err("agent report source must contain 1..=128 bytes");
        }
        let known = self.report_sequences.get(source);
        if known.is_some_and(|previous| sequence <= *previous) {
            return Err("agent report sequence is stale");
        }
        if known.is_none() && self.report_sequences.len() >= MAX_REPORT_SOURCES {
            return Err("agent report source limit reached");
        }
        Ok(())
    }

    pub fn evaluate_terminal(
        &mut self,
        catalog: &AgentCatalog,
        terminal: &Terminal,
        visible: bool,
        now: Instant,
    ) {
        let Some(identity) = self.identity.as_ref() else {
            return;
        };
        if let Some(report) = &self.report {
            let state = report.state;
            self.commit(state, AgentSource::Report, visible);
            return;
        }
        let screen = detection_snapshot(terminal);
        let Some(candidate) = detect_candidate(
            catalog,
            &identity.id,
            &screen,
            terminal.agent_osc_title(),
            terminal.agent_osc_progress(),
        ) else {
            self.pending_idle = None;
            return;
        };
        if now.saturating_duration_since(self.identified_at) < STARTUP_GRACE {
            self.commit(AgentState::Idle, AgentSource::Screen, visible);
            return;
        }
        if candidate.state == AgentState::Idle
            && !candidate.visible_idle
            && self.state == AgentState::Working
        {
            let (started, confirmations) = self.pending_idle.get_or_insert((now, 0));
            *confirmations = confirmations.saturating_add(1);
            if *confirmations < IDLE_CONFIRMATIONS
                && now.saturating_duration_since(*started) < IDLE_CONFIRMATION_LIMIT
            {
                return;
            }
        } else {
            self.pending_idle = None;
        }
        self.pending_idle = None;
        self.commit(candidate.state, AgentSource::Screen, visible);
    }

    /// Replay classification for this pane and report what decided its state.
    ///
    /// Rules are evaluated and returned even while a report holds authority: "the hook says idle
    /// but the screen says blocked" is the disagreement most worth seeing, and `decision` says
    /// which one won.
    pub fn explain(
        &self,
        catalog: &AgentCatalog,
        terminal: &Terminal,
        now: Instant,
    ) -> Option<AgentExplanation> {
        let identity = self.identity.as_ref()?;
        let snapshot = self.snapshot()?;
        let screen = detection_snapshot(terminal);
        let definition = catalog.definitions.get(&identity.id);
        let rules = definition.map_or_else(Vec::new, |definition| {
            definition.manifest.explain(
                &screen,
                terminal.agent_osc_title(),
                terminal.agent_osc_progress(),
            )
        });
        let startup_grace_active =
            now.saturating_duration_since(self.identified_at) < STARTUP_GRACE;
        let decided = rules.iter().find(|rule| rule.decided);
        let decision = if self.report.is_some() {
            "reported"
        } else if startup_grace_active {
            "startup_grace"
        } else {
            match decided {
                Some(rule) if rule.skip_state_update => "state_preserved",
                Some(_) => "rule_matched",
                None => "no_rule_matched",
            }
        };
        Some(AgentExplanation {
            agent: identity.id.clone(),
            label: identity.name.clone(),
            provider: identity.provider.clone(),
            fingerprint: identity.fingerprint.clone(),
            effective_state: snapshot.state,
            effective_status: snapshot.status,
            source: snapshot.source,
            report: self.report.as_ref().map(|report| ReportExplanation {
                source: report.source.clone(),
                sequence: report.sequence,
                state: report.state,
                message: report.message.clone(),
            }),
            decision,
            matched_rule: decided.map(|rule| rule.id.clone()),
            startup_grace_active,
            pending_idle_confirmations: self.pending_idle.map_or(0, |(_, count)| count),
            osc_title: terminal.agent_osc_title().to_owned(),
            osc_progress: terminal.agent_osc_progress().to_owned(),
            detection_bytes: screen.len(),
            detection_rows: terminal.rows(),
            rules,
        })
    }

    pub fn mark_seen(&mut self) {
        if self.state == AgentState::Idle {
            self.done = false;
        }
    }

    pub fn reconcile_catalog(&mut self, catalog: &AgentCatalog) -> bool {
        let stale = self.identity.as_ref().is_some_and(|current| {
            catalog
                .identity(&current.id)
                .is_none_or(|next| next != *current)
        });
        if stale {
            self.report = None;
            self.report_sequences.clear();
            self.session = None;
            self.metadata = AgentMetadata::default();
            self.pending_idle = None;
            self.identity = None;
            self.done = false;
            self.state = AgentState::Idle;
            self.source = AgentSource::Screen;
        }
        stale
    }

    fn commit(&mut self, state: AgentState, source: AgentSource, visible: bool) {
        if state != self.state {
            self.done = state == AgentState::Idle
                && matches!(self.state, AgentState::Working | AgentState::Blocked)
                && !visible;
            self.state = state;
        }
        self.source = source;
        if visible && state == AgentState::Idle {
            self.done = false;
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentCatalogSource {
    pub provider: String,
    pub fingerprint: String,
    pub definition: AgentDefinition,
}

#[derive(Debug, Clone, Default)]
pub struct AgentCatalog {
    definitions: BTreeMap<AgentId, CompiledAgent>,
}

#[derive(Debug, Clone)]
struct CompiledAgent {
    identity: AgentIdentity,
    executables: BTreeSet<String>,
    argv_contains: Vec<String>,
    manifest: CompiledManifest,
}

impl AgentCatalog {
    pub const MAX_ENABLED_AGENTS: usize = 64;

    pub fn compile(sources: Vec<AgentCatalogSource>) -> Result<Self, String> {
        if sources.len() > Self::MAX_ENABLED_AGENTS {
            return Err("enabled plugin catalog exceeds 64 agent definitions".into());
        }
        let mut definitions = BTreeMap::new();
        let mut executable_owners = BTreeMap::<String, AgentId>::new();
        for source in sources {
            let id = AgentId::new(source.definition.id.clone()).map_err(str::to_owned)?;
            if definitions.contains_key(&id) {
                return Err(format!("duplicate enabled agent ID `{id}`"));
            }
            let executables = source
                .definition
                .process
                .executables
                .iter()
                .map(|value| normalized_program_name(value))
                .collect::<BTreeSet<_>>();
            for executable in &executables {
                if let Some(owner) = executable_owners.insert(executable.clone(), id.clone()) {
                    return Err(format!(
                        "agent `{id}` executable `{executable}` conflicts with agent `{owner}`"
                    ));
                }
            }
            let identity = AgentIdentity {
                id: id.clone(),
                name: source.definition.name.clone(),
                provider: source.provider,
                fingerprint: source.fingerprint,
            };
            definitions.insert(
                id,
                CompiledAgent {
                    identity,
                    executables,
                    argv_contains: source
                        .definition
                        .process
                        .argv_contains
                        .iter()
                        .map(|value| value.to_ascii_lowercase())
                        .collect(),
                    manifest: CompiledManifest::compile(&source.definition),
                },
            );
        }
        Ok(Self { definitions })
    }

    pub fn identity(&self, id: &AgentId) -> Option<AgentIdentity> {
        self.definitions.get(id).map(|value| value.identity.clone())
    }

    pub fn describe(&self) -> Vec<serde_json::Value> {
        self.definitions
            .values()
            .map(|definition| {
                serde_json::json!({
                    "id": definition.identity.id,
                    "name": definition.identity.name,
                    "provider": definition.identity.provider,
                    "fingerprint": definition.identity.fingerprint,
                })
            })
            .collect()
    }

    fn identify(&self, process: &ProcessInfo) -> Option<AgentIdentity> {
        let runtime = normalized_program_name(&process.name);
        let token = invocation_token(process).map(normalized_program_name);
        let exact = self
            .definitions
            .values()
            .filter(|definition| {
                definition.executables.contains(&runtime)
                    || token
                        .as_ref()
                        .is_some_and(|token| definition.executables.contains(token))
            })
            .collect::<Vec<_>>();
        if exact.len() == 1 {
            return Some(exact[0].identity.clone());
        }
        if !exact.is_empty() {
            return None;
        }
        let arguments = process.argv.join("\n").to_ascii_lowercase();
        let marker = self
            .definitions
            .values()
            .filter(|definition| {
                definition
                    .argv_contains
                    .iter()
                    .any(|needle| arguments.contains(needle))
            })
            .collect::<Vec<_>>();
        (marker.len() == 1).then(|| marker[0].identity.clone())
    }
}

#[derive(Debug, Clone)]
struct CompiledManifest {
    rules: Vec<CompiledRule>,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    /// Kept so a classification report can name the rule the author wrote, rather than an index
    /// that shifts whenever the manifest gains a rule.
    id: String,
    state: AgentRuleState,
    priority: u16,
    region: String,
    visible_idle: bool,
    skip_state_update: bool,
    gate: CompiledGate,
}

#[derive(Debug, Clone)]
struct CompiledGate {
    contains: Vec<String>,
    regex: Vec<regex::Regex>,
    line_regex: Vec<regex::Regex>,
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not_gate: Vec<CompiledGate>,
}

/// Why one pane's effective agent state is what it is, with the evidence behind it.
///
/// Read-only: a diagnostic must not perturb what it measures, so nothing here advances the idle
/// confirmation counter or commits a state.
#[derive(Debug, Clone, Serialize)]
pub struct AgentExplanation {
    pub agent: AgentId,
    pub label: String,
    pub provider: String,
    pub fingerprint: String,
    pub effective_state: AgentState,
    pub effective_status: AgentStatus,
    pub source: AgentSource,
    pub report: Option<ReportExplanation>,
    /// `reported`, `startup_grace`, `state_preserved`, `rule_matched`, or `no_rule_matched`.
    pub decision: &'static str,
    pub matched_rule: Option<String>,
    pub startup_grace_active: bool,
    pub pending_idle_confirmations: u8,
    pub osc_title: String,
    pub osc_progress: String,
    pub detection_bytes: usize,
    pub detection_rows: usize,
    pub rules: Vec<RuleExplanation>,
}

/// The held report, when one has authority.
///
/// Deliberately excludes the native session reference: this is a classification diagnostic, and
/// that value is disclosed only by a single-pane inspect. See [`AgentSessionRef`].
#[derive(Debug, Clone, Serialize)]
pub struct ReportExplanation {
    pub source: String,
    pub sequence: u64,
    pub state: AgentState,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleExplanation {
    pub id: String,
    pub state: &'static str,
    pub priority: u16,
    pub region: String,
    pub matched: bool,
    /// Whether classification stopped here. At most one rule is `decided`.
    pub decided: bool,
    pub visible_idle: bool,
    pub skip_state_update: bool,
    pub region_bytes: usize,
    pub region_preview: String,
    /// Normalized at compile time to lowercase, which is how the gate compares them.
    pub contains: Vec<String>,
    pub regex: Vec<String>,
    pub line_regex: Vec<String>,
    pub all_count: usize,
    pub any_count: usize,
    pub not_count: usize,
}

fn rule_state_label(state: AgentRuleState) -> &'static str {
    match state {
        AgentRuleState::Idle => "idle",
        AgentRuleState::Working => "working",
        AgentRuleState::Blocked => "blocked",
        AgentRuleState::Unknown => "unknown",
    }
}

/// The tail of the region a rule looked at, bounded and safe to print.
///
/// The tail rather than the head: every region a manifest can name is anchored at the bottom of
/// the buffer, so the newest text is what decided the match.
fn region_preview(text: &str) -> String {
    let mut start = text.len().saturating_sub(MAX_REGION_PREVIEW_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..]
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// The exact text agent classification runs against.
///
/// Shared by [`AgentRuntime::evaluate_terminal`] and [`AgentRuntime::explain`] on purpose: an
/// explanation built from a separately-derived snapshot could disagree with the classifier it
/// claims to describe, which is worse than no explanation at all.
fn detection_snapshot(terminal: &Terminal) -> String {
    let mut screen = terminal.latest_text(terminal.rows());
    if screen.len() > MAX_DETECTION_BYTES {
        let mut start = screen.len() - MAX_DETECTION_BYTES;
        while !screen.is_char_boundary(start) {
            start += 1;
        }
        screen = screen[start..].to_owned();
    }
    screen
}

fn agent_rule_state(state: AgentRuleState) -> Option<AgentState> {
    match state {
        AgentRuleState::Idle => Some(AgentState::Idle),
        AgentRuleState::Working => Some(AgentState::Working),
        AgentRuleState::Blocked => Some(AgentState::Blocked),
        AgentRuleState::Unknown => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct DetectionCandidate {
    state: AgentState,
    visible_idle: bool,
}

impl CompiledManifest {
    fn compile(definition: &AgentDefinition) -> Self {
        let mut rules = definition
            .rules
            .clone()
            .into_iter()
            .map(|rule| CompiledRule {
                id: rule.id,
                state: rule.state,
                priority: rule.priority,
                region: rule.region,
                visible_idle: rule.visible_idle,
                skip_state_update: rule.skip_state_update,
                gate: CompiledGate::compile(rule.gate),
            })
            .collect::<Vec<_>>();
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.priority));
        Self { rules }
    }

    /// Evaluate every rule and record its evidence.
    ///
    /// Unlike [`Self::detect`] this does not stop at the first match: a rule shadowed by a
    /// higher-priority one is exactly what a manifest author needs to see. The rule `detect` would
    /// have stopped at is flagged `decided`.
    fn explain(&self, screen: &str, title: &str, progress: &str) -> Vec<RuleExplanation> {
        let mut decided = false;
        self.rules
            .iter()
            .map(|rule| {
                let text = rule_region(screen, title, progress, &rule.region);
                let matched = rule.gate.matches(text);
                let explanation = RuleExplanation {
                    id: rule.id.clone(),
                    state: rule_state_label(rule.state),
                    priority: rule.priority,
                    region: rule.region.clone(),
                    matched,
                    decided: matched && !decided,
                    visible_idle: rule.visible_idle,
                    skip_state_update: rule.skip_state_update,
                    region_bytes: text.len(),
                    region_preview: region_preview(text),
                    contains: rule.gate.contains.clone(),
                    regex: rule
                        .gate
                        .regex
                        .iter()
                        .map(|pattern| pattern.as_str().to_owned())
                        .collect(),
                    line_regex: rule
                        .gate
                        .line_regex
                        .iter()
                        .map(|pattern| pattern.as_str().to_owned())
                        .collect(),
                    all_count: rule.gate.all.len(),
                    any_count: rule.gate.any.len(),
                    not_count: rule.gate.not_gate.len(),
                };
                decided |= matched;
                explanation
            })
            .collect()
    }

    fn detect(&self, screen: &str, title: &str, progress: &str) -> Option<DetectionCandidate> {
        for rule in &self.rules {
            let text = rule_region(screen, title, progress, &rule.region);
            if !rule.gate.matches(text) {
                continue;
            }
            if rule.skip_state_update {
                return None;
            }
            return agent_rule_state(rule.state).map(|state| DetectionCandidate {
                state,
                visible_idle: rule.visible_idle,
            });
        }
        Some(DetectionCandidate {
            state: AgentState::Idle,
            visible_idle: false,
        })
    }
}

impl CompiledGate {
    fn compile(gate: AgentGate) -> Self {
        Self {
            contains: gate
                .contains
                .into_iter()
                .map(|needle| needle.to_lowercase())
                .collect(),
            regex: compile_regexes(gate.regex),
            line_regex: compile_regexes(gate.line_regex),
            all: gate.all.into_iter().map(Self::compile).collect(),
            any: gate.any.into_iter().map(Self::compile).collect(),
            not_gate: gate.not_gate.into_iter().map(Self::compile).collect(),
        }
    }

    fn matches(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        self.matches_lower(text, &lower)
    }

    fn matches_lower(&self, text: &str, lower: &str) -> bool {
        self.contains.iter().all(|needle| lower.contains(needle))
            && self.regex.iter().all(|pattern| pattern.is_match(text))
            && self
                .line_regex
                .iter()
                .all(|pattern| text.lines().any(|line| pattern.is_match(line)))
            && self.all.iter().all(|gate| gate.matches_lower(text, lower))
            && (self.any.is_empty() || self.any.iter().any(|gate| gate.matches_lower(text, lower)))
            && !self
                .not_gate
                .iter()
                .any(|gate| gate.matches_lower(text, lower))
    }
}

fn compile_regexes(patterns: Vec<String>) -> Vec<regex::Regex> {
    patterns
        .into_iter()
        .map(|pattern| regex::Regex::new(&pattern).expect("embedded agent regex"))
        .collect()
}

fn detect_candidate(
    catalog: &AgentCatalog,
    kind: &AgentId,
    screen: &str,
    osc_title: &str,
    osc_progress: &str,
) -> Option<DetectionCandidate> {
    catalog
        .definitions
        .get(kind)
        .and_then(|definition| definition.manifest.detect(screen, osc_title, osc_progress))
}

#[cfg(test)]
fn detect_state(
    catalog: &AgentCatalog,
    kind: &AgentId,
    screen: &str,
    osc_title: &str,
    osc_progress: &str,
) -> AgentState {
    detect_candidate(catalog, kind, screen, osc_title, osc_progress)
        .map_or(AgentState::Idle, |candidate| candidate.state)
}

fn rule_region<'a>(screen: &'a str, title: &'a str, progress: &'a str, region: &str) -> &'a str {
    match region {
        "osc_title" => title,
        "osc_progress" => progress,
        "whole_recent" => screen,
        "after_last_prompt_marker" => after_last_prompt_marker(screen),
        "prompt_box_body" => prompt_box_body(screen).unwrap_or(""),
        "after_last_horizontal_rule" => after_last_horizontal_rule(screen),
        other => region_count(other, "bottom_non_empty_lines")
            .map_or("", |count| bottom_non_empty_lines(screen, count)),
    }
}

fn region_count(spec: &str, name: &str) -> Option<usize> {
    spec.strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')?
        .parse()
        .ok()
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines = content.lines().collect::<Vec<_>>();
    let Some(start) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    slice_from_line(content, &lines, start)
}

fn after_last_prompt_marker(content: &str) -> &str {
    let lines = content.lines().collect::<Vec<_>>();
    let Some(index) = lines
        .iter()
        .rposition(|line| *line == "›" || line.starts_with("› "))
    else {
        return content;
    };
    slice_from_line(content, &lines, index + 1)
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines = content.lines().collect::<Vec<_>>();
    let mut borders = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| is_horizontal_rule(line));
    let _bottom = borders.next()?;
    let (top, _) = borders.next()?;
    let start = line_offset(content, &lines, top + 1);
    let end_line = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map_or(lines.len(), |relative| top + 1 + relative);
    let end = line_offset(content, &lines, end_line);
    Some(&content[start..end])
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let lines = content.lines().collect::<Vec<_>>();
    let Some(index) = lines.iter().rposition(|line| is_horizontal_rule(line)) else {
        return content;
    };
    slice_from_line(content, &lines, index + 1)
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let rules = trimmed
        .chars()
        .take_while(|character| *character == '─')
        .count();
    if rules == 0 {
        return false;
    }
    let suffix_start = trimmed
        .char_indices()
        .nth(rules)
        .map_or(trimmed.len(), |(index, _)| index);
    trimmed[suffix_start..].trim_start().is_empty() || rules >= 3
}

fn slice_from_line<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    &content[line_offset(content, lines, index)..]
}

fn line_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

#[derive(Clone)]
pub struct ProbeTarget {
    pub pane_id: PaneId,
    pub child_pid: u32,
    pub control: PtyControl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessUpdate {
    pub pane_id: PaneId,
    pub process_group: Option<u32>,
    pub identity: Option<AgentIdentity>,
}

pub struct DetectorHandle {
    commands: mpsc::Sender<DetectorCommand>,
}

enum DetectorCommand {
    Targets(Vec<ProbeTarget>),
    Catalog(Arc<AgentCatalog>),
}

impl DetectorHandle {
    pub fn replace_targets(&self, targets: Vec<ProbeTarget>) {
        let _ = self.commands.send(DetectorCommand::Targets(targets));
    }

    pub fn replace_catalog(&self, catalog: Arc<AgentCatalog>) {
        let _ = self.commands.send(DetectorCommand::Catalog(catalog));
    }
}

pub fn start_detector(
    mut notify: impl FnMut(Vec<ProcessUpdate>) + Send + 'static,
) -> std::io::Result<DetectorHandle> {
    let (sender, receiver) = mpsc::channel::<DetectorCommand>();
    std::thread::Builder::new()
        .name("vvmux-agent-detector".into())
        .spawn(move || {
            let mut targets = Vec::new();
            let mut catalog = Arc::new(AgentCatalog::default());
            let mut cached =
                BTreeMap::<PaneId, (Option<u32>, Option<AgentIdentity>, Instant)>::new();
            loop {
                match receiver.recv_timeout(Duration::from_millis(400)) {
                    Ok(DetectorCommand::Targets(next)) => targets = next,
                    Ok(DetectorCommand::Catalog(next)) => {
                        catalog = next;
                        cached.clear();
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                while let Ok(command) = receiver.try_recv() {
                    match command {
                        DetectorCommand::Targets(next) => targets = next,
                        DetectorCommand::Catalog(next) => {
                            catalog = next;
                            cached.clear();
                        }
                    }
                }
                let mut updates = Vec::new();
                let live = targets
                    .iter()
                    .map(|target| target.pane_id)
                    .collect::<Vec<_>>();
                cached.retain(|pane, _| live.contains(pane));
                for target in &targets {
                    let group = target.control.foreground_process_group_id();
                    let previous = cached.get(&target.pane_id).cloned();
                    let needs_full =
                        previous
                            .as_ref()
                            .is_none_or(|(old_group, old_kind, checked)| {
                                *old_group != group
                                    || checked.elapsed()
                                        >= if old_kind.is_some() {
                                            FULL_PROCESS_RECHECK
                                        } else {
                                            Duration::from_millis(500)
                                        }
                            });
                    if !needs_full {
                        continue;
                    }
                    let identity = identify_foreground_agent(&catalog, target.child_pid, group);
                    let next = (group, identity.clone(), Instant::now());
                    if previous.is_none_or(|(old_group, old_kind, _)| {
                        old_group != group || old_kind != identity
                    }) {
                        updates.push(ProcessUpdate {
                            pane_id: target.pane_id,
                            process_group: group,
                            identity,
                        });
                    }
                    cached.insert(target.pane_id, next);
                }
                if !updates.is_empty() {
                    notify(updates);
                }
            }
        })?;
    Ok(DetectorHandle { commands: sender })
}

fn identify_foreground_agent(
    catalog: &AgentCatalog,
    child_pid: u32,
    group: Option<u32>,
) -> Option<AgentIdentity> {
    foreground_processes(child_pid, group)
        .into_iter()
        .find_map(|process| catalog.identify(&process))
}

#[derive(Debug)]
struct ProcessInfo {
    name: String,
    argv: Vec<String>,
}

fn invocation_token(process: &ProcessInfo) -> Option<&str> {
    let runtime = normalized_program_name(&process.name);
    match runtime.as_str() {
        "node" | "bun" => script_argument(&process.argv, &["-e", "--eval", "-p", "--print"]),
        runtime if runtime == "python" || runtime.starts_with("python3") => {
            python_script_argument(&process.argv)
        }
        "sh" | "bash" | "zsh" | "fish" => shell_command_argument(&process.argv),
        "cmd" => windows_command_argument(&process.argv),
        "powershell" | "pwsh" => powershell_command_argument(&process.argv),
        _ => process.argv.first().map(String::as_str),
    }
}

fn normalized_program_name(value: &str) -> String {
    let mut name = path_basename(value).trim().to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1", ".js", ".mjs", ".cjs", ".py"] {
        if name.ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    name
}

fn path_basename(value: &str) -> &str {
    value
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
}

fn script_argument<'a>(argv: &'a [String], eval_flags: &[&str]) -> Option<&'a str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            return arguments.next().map(String::as_str);
        }
        if eval_flags.iter().any(|flag| {
            argument == flag
                || argument.strip_prefix(flag).is_some_and(|rest| {
                    (!flag.starts_with("--") && !rest.is_empty()) || rest.starts_with('=')
                })
        }) {
            return None;
        }
        if argument.starts_with('-') {
            if matches!(
                argument.as_str(),
                "-r" | "--require" | "--loader" | "--import" | "--experimental-loader"
            ) {
                let _ = arguments.next();
            }
            continue;
        }
        return Some(argument);
    }
    None
}

fn python_script_argument(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-c" => return None,
            "-m" => return arguments.next().map(String::as_str),
            "--" => return arguments.next().map(String::as_str),
            value if value.starts_with('-') => continue,
            _ => return Some(argument),
        }
    }
    None
}

fn shell_command_argument(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument
            .strip_prefix('-')
            .is_some_and(|flags| flags.contains('c'))
        {
            return arguments.next().and_then(|command| command_token(command));
        }
        if !argument.starts_with('-') {
            return Some(argument);
        }
    }
    None
}

fn windows_command_argument(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if matches!(argument.to_ascii_lowercase().as_str(), "/c" | "/k") {
            return arguments.next().and_then(|command| command_token(command));
        }
    }
    None
}

fn powershell_command_argument(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.trim_matches('"').to_ascii_lowercase().as_str() {
            "-file" | "-f" | "/file" => return arguments.next().map(String::as_str),
            "-command" | "-c" | "/command" | "/c" => {
                return arguments.next().and_then(|command| command_token(command));
            }
            "-encodedcommand" | "-enc" | "/encodedcommand" | "/enc" => return None,
            "-configurationname" | "-executionpolicy" | "-outputformat" | "-psconsolefile"
            | "-version" | "-windowstyle" | "-workingdirectory" => {
                let _ = arguments.next();
            }
            value if value.starts_with('-') || value.starts_with('/') => continue,
            _ => return Some(argument),
        }
    }
    None
}

#[cfg(any(windows, test))]
fn split_process_command_line(line: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut argument = String::new();
    let mut quote = None;
    for character in line.chars() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(active), value) if value == active => quote = None,
            (None, value) if value.is_whitespace() => {
                if !argument.is_empty() {
                    arguments.push(std::mem::take(&mut argument));
                }
            }
            _ => argument.push(character),
        }
    }
    if !argument.is_empty() {
        arguments.push(argument);
    }
    arguments
}

fn command_token(mut command: &str) -> Option<&str> {
    loop {
        command = command.trim_start();
        let first = command.chars().next()?;
        let (token, rest) = if matches!(first, '\'' | '"') {
            let start = first.len_utf8();
            let end = command[start..]
                .find(first)
                .map_or(command.len(), |end| start + end);
            (
                &command[start..end],
                &command[end.saturating_add(first.len_utf8()).min(command.len())..],
            )
        } else {
            let end = command.find(char::is_whitespace).unwrap_or(command.len());
            (&command[..end], &command[end..])
        };
        if matches!(
            token.to_ascii_lowercase().as_str(),
            "&" | "." | "call" | "exec"
        ) {
            command = rest;
            continue;
        }
        return Some(token);
    }
}

#[cfg(target_os = "linux")]
fn foreground_processes(_child_pid: u32, group: Option<u32>) -> Vec<ProcessInfo> {
    use std::fs;
    use std::io::Read;
    let Some(group) = group else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for entry in entries.flatten().take(16_384) {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(tail) = stat.rsplit_once(')').map(|(_, tail)| tail) else {
            continue;
        };
        let fields = tail.split_whitespace().collect::<Vec<_>>();
        if fields.get(2).and_then(|value| value.parse::<u32>().ok()) != Some(group) {
            continue;
        }
        let name = fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let mut command_line = Vec::new();
        let _ = fs::File::open(format!("/proc/{pid}/cmdline")).and_then(|file| {
            file.take(MAX_PROCESS_ARGV_BYTES as u64)
                .read_to_end(&mut command_line)
        });
        let argv = command_line
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        processes.push(ProcessInfo { name, argv });
    }
    processes
}

#[cfg(target_os = "macos")]
fn foreground_processes(_child_pid: u32, group: Option<u32>) -> Vec<ProcessInfo> {
    const PROC_PGRP_ONLY: u32 = 2;
    let Some(group) = group else {
        return Vec::new();
    };
    let mut pids = vec![0 as libc::pid_t; 4096];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            group,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }
    let count = bytes as usize / std::mem::size_of::<libc::pid_t>();
    pids.into_iter()
        .take(count)
        .filter_map(|pid| u32::try_from(pid).ok())
        .filter_map(|pid| {
            macos_argv(pid).map(|argv| ProcessInfo {
                name: argv.first().cloned().unwrap_or_default(),
                argv,
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn macos_argv(pid: u32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size = 0;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size == 0
        || size > MAX_PROCESS_ARGV_BYTES
    {
        return None;
    }
    let mut bytes = vec![0_u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    bytes.truncate(size);
    if bytes.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(bytes[..4].try_into().ok()?);
    let rest = &bytes[4..];
    let executable_end = rest.iter().position(|byte| *byte == 0)?;
    let mut offset = executable_end;
    while rest.get(offset) == Some(&0) {
        offset += 1;
    }
    let mut argv = Vec::new();
    for part in rest[offset..]
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .take(argc.max(0) as usize)
    {
        argv.push(String::from_utf8_lossy(part).into_owned());
    }
    (!argv.is_empty()).then_some(argv)
}

#[cfg(windows)]
fn foreground_processes(child_pid: u32, _group: Option<u32>) -> Vec<ProcessInfo> {
    use std::collections::{HashMap, VecDeque};
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut rows = Vec::new();
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok && rows.len() < 16_384 {
        let length = entry
            .szExeFile
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExeFile.len());
        rows.push((
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            String::from_utf16_lossy(&entry.szExeFile[..length]),
            windows_command_line(entry.th32ProcessID),
        ));
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    let mut children = HashMap::<u32, Vec<(u32, String, Vec<String>)>>::new();
    let mut root = None;
    for (pid, parent, name, command_line) in rows {
        let argv = command_line
            .map(|line| split_process_command_line(&line))
            .filter(|arguments| !arguments.is_empty())
            .unwrap_or_else(|| vec![name.clone()]);
        if pid == child_pid {
            root = Some(ProcessInfo {
                name: name.clone(),
                argv: argv.clone(),
            });
        }
        children.entry(parent).or_default().push((pid, name, argv));
    }
    let mut queue = VecDeque::from([child_pid]);
    let mut output = root.into_iter().collect::<Vec<_>>();
    while let Some(parent) = queue.pop_front() {
        for (pid, name, argv) in children.remove(&parent).unwrap_or_default() {
            queue.push_back(pid);
            output.push(ProcessInfo { argv, name });
        }
    }
    output
}

#[cfg(windows)]
fn windows_command_line(pid: u32) -> Option<String> {
    use std::ffi::c_void;
    use std::mem::{MaybeUninit, size_of};
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, NTSTATUS, STATUS_SUCCESS, UNICODE_STRING,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Peb {
        reserved1: [u8; 2],
        being_debugged: u8,
        reserved2: [u8; 1],
        reserved3: [*mut c_void; 2],
        ldr: *mut c_void,
        process_parameters: *mut ProcessParameters,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CurrentDirectory {
        path: UNICODE_STRING,
        handle: HANDLE,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcessParameters {
        maximum_length: u32,
        length: u32,
        flags: u32,
        debug_flags: u32,
        console_handle: HANDLE,
        console_flags: u32,
        standard_input: HANDLE,
        standard_output: HANDLE,
        standard_error: HANDLE,
        current_directory: CurrentDirectory,
        dll_path: UNICODE_STRING,
        image_path_name: UNICODE_STRING,
        command_line: UNICODE_STRING,
        environment: *mut c_void,
    }

    unsafe fn read_value<T: Copy>(process: HANDLE, address: *const c_void) -> Option<T> {
        if address.is_null() {
            return None;
        }
        let mut value = MaybeUninit::<T>::uninit();
        let mut read = 0;
        let ok = unsafe {
            ReadProcessMemory(
                process,
                address,
                value.as_mut_ptr().cast(),
                size_of::<T>(),
                &mut read,
            )
        } != 0;
        (ok && read == size_of::<T>()).then(|| unsafe { value.assume_init() })
    }

    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if process.is_null() {
        return None;
    }
    let result = (|| {
        let mut basic = MaybeUninit::<PROCESS_BASIC_INFORMATION>::uninit();
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessBasicInformation,
                basic.as_mut_ptr().cast(),
                size_of::<PROCESS_BASIC_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if status != STATUS_SUCCESS as NTSTATUS {
            return None;
        }
        let basic = unsafe { basic.assume_init() };
        let peb = unsafe { read_value::<Peb>(process, basic.PebBaseAddress.cast()) }?;
        let parameters =
            unsafe { read_value::<ProcessParameters>(process, peb.process_parameters.cast()) }?;
        let command = parameters.command_line;
        if command.Buffer.is_null() || command.Length == 0 || command.Length % 2 != 0 {
            return None;
        }
        let mut buffer = vec![0_u16; usize::from(command.Length / 2)];
        let mut read = 0;
        let ok = unsafe {
            ReadProcessMemory(
                process,
                command.Buffer.cast(),
                buffer.as_mut_ptr().cast(),
                usize::from(command.Length),
                &mut read,
            )
        } != 0;
        (ok && read == usize::from(command.Length)).then(|| String::from_utf16_lossy(&buffer))
    })();
    unsafe { CloseHandle(process) };
    result
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn foreground_processes(_child_pid: u32, _group: Option<u32>) -> Vec<ProcessInfo> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> AgentCatalog {
        let manifests = [
            (
                "dev.vivido.agent.claude",
                include_str!("../builtin-plugins/agent-claude/vvmux-plugin.toml"),
            ),
            (
                "dev.vivido.agent.codex",
                include_str!("../builtin-plugins/agent-codex/vvmux-plugin.toml"),
            ),
            (
                "dev.vivido.agent.opencode",
                include_str!("../builtin-plugins/agent-opencode/vvmux-plugin.toml"),
            ),
            (
                "dev.vivido.agent.hermes",
                include_str!("../builtin-plugins/agent-hermes/vvmux-plugin.toml"),
            ),
        ];
        AgentCatalog::compile(
            manifests
                .into_iter()
                .flat_map(|(provider, source)| {
                    let manifest: vvmux_plugin_api::Manifest = toml::from_str(source).unwrap();
                    manifest
                        .agents
                        .into_iter()
                        .map(move |definition| AgentCatalogSource {
                            provider: provider.into(),
                            fingerprint: "test".into(),
                            definition,
                        })
                })
                .collect(),
        )
        .unwrap()
    }

    fn id(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    fn identity(catalog: &AgentCatalog, value: &str) -> AgentIdentity {
        catalog.identity(&id(value)).unwrap()
    }

    /// A state-only report, the shape every integration sends most of the time.
    fn state_report(
        identity: AgentIdentity,
        state: AgentState,
        source: &str,
        sequence: u64,
    ) -> AgentReport {
        AgentReport {
            identity,
            state,
            source: source.into(),
            sequence,
            message: None,
            session: None,
        }
    }

    fn custom_catalog() -> AgentCatalog {
        let manifest: vvmux_plugin_api::Manifest = toml::from_str(
            r#"manifest_version = 2
[plugin]
id = "com.example.openclaw"
name = "OpenClaw"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "test"
platforms = ["linux"]
permissions = []
[[agents]]
id = "openclaw"
name = "OpenClaw"
process = { executables = ["openclaw", "openclaw-cli"], argv_contains = ["@openclaw/cli"] }
"#,
        )
        .unwrap();
        AgentCatalog::compile(vec![AgentCatalogSource {
            provider: "com.example.openclaw".into(),
            fingerprint: "custom".into(),
            definition: manifest.agents.into_iter().next().unwrap(),
        }])
        .unwrap()
    }

    #[test]
    fn identifies_wrapped_agents() {
        let catalog = catalog();
        assert_eq!(
            catalog.identify(&ProcessInfo {
                name: "node".into(),
                argv: vec!["node".into(), "/x/@anthropic-ai/claude-code/cli.js".into()]
            }),
            Some(identity(&catalog, "claude"))
        );
        assert_eq!(
            catalog.identify(&ProcessInfo {
                name: "python".into(),
                argv: vec!["python".into(), "hermes_agent.py".into()]
            }),
            Some(identity(&catalog, "hermes"))
        );
        assert_eq!(
            catalog.identify(&ProcessInfo {
                name: "bash".into(),
                argv: vec!["bash".into(), "-lc".into(), "exec codex --full-auto".into()]
            }),
            Some(identity(&catalog, "codex"))
        );
        assert_eq!(
            catalog.identify(&ProcessInfo {
                name: "powershell.exe".into(),
                argv: vec![
                    "powershell.exe".into(),
                    "-NoProfile".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-Command".into(),
                    "& 'C:\\tools\\opencode.cmd' --continue".into(),
                ]
            }),
            Some(identity(&catalog, "opencode"))
        );
        assert_eq!(
            catalog.identify(&ProcessInfo {
                name: "node".into(),
                argv: vec![
                    "node".into(),
                    "--eval".into(),
                    "setTimeout(() => {}, 1000)".into(),
                    "/tmp/codex".into(),
                ]
            }),
            None
        );
        assert_eq!(
            split_process_command_line("powershell.exe -Command \"& opencode --continue\""),
            ["powershell.exe", "-Command", "& opencode --continue"]
        );
    }

    #[test]
    fn custom_agent_matches_exact_executables_and_packaged_argv() {
        let catalog = custom_catalog();
        for process in [
            ProcessInfo {
                name: "/usr/local/bin/openclaw".into(),
                argv: vec!["/usr/local/bin/openclaw".into()],
            },
            ProcessInfo {
                name: "node".into(),
                argv: vec!["node".into(), "/opt/@openclaw/cli/main.js".into()],
            },
        ] {
            assert_eq!(
                catalog.identify(&process).unwrap().id,
                AgentId::new("openclaw").unwrap()
            );
        }
    }

    #[test]
    fn catalog_removal_clears_only_the_removed_providers_runtime() {
        let full = catalog();
        let retained = custom_catalog();
        let mut removed_runtime = AgentRuntime::new();
        let mut retained_runtime = AgentRuntime::new();
        removed_runtime
            .report(
                state_report(
                    identity(&full, "codex"),
                    AgentState::Blocked,
                    "codex-hook",
                    1,
                ),
                false,
            )
            .unwrap();
        retained_runtime
            .report(
                state_report(
                    identity(&retained, "openclaw"),
                    AgentState::Working,
                    "openclaw-hook",
                    1,
                ),
                false,
            )
            .unwrap();

        assert!(removed_runtime.reconcile_catalog(&retained));
        assert!(!retained_runtime.reconcile_catalog(&retained));
        assert!(removed_runtime.snapshot().is_none());
        assert_eq!(
            retained_runtime.snapshot().unwrap().state,
            AgentState::Working
        );
        retained_runtime.clear_report("openclaw-hook", 2).unwrap();
    }

    #[test]
    fn report_sources_are_bounded_without_blocking_known_sources() {
        let catalog = catalog();
        let mut runtime = AgentRuntime::new();
        for index in 0..MAX_REPORT_SOURCES {
            runtime
                .report(
                    state_report(
                        identity(&catalog, "codex"),
                        AgentState::Working,
                        &format!("source-{index}"),
                        1,
                    ),
                    false,
                )
                .unwrap();
        }
        // A new source cannot claim a slot once the table is full.
        assert_eq!(
            runtime.report(
                state_report(
                    identity(&catalog, "codex"),
                    AgentState::Working,
                    "one-too-many",
                    1
                ),
                false,
            ),
            Err("agent report source limit reached")
        );
        // A source already in the table keeps working, so a full table degrades to "no new
        // reporters" rather than to "this pane is stuck".
        runtime
            .report(
                state_report(
                    identity(&catalog, "codex"),
                    AgentState::Blocked,
                    "source-0",
                    2,
                ),
                false,
            )
            .unwrap();
        assert_eq!(runtime.snapshot().unwrap().state, AgentState::Blocked);
        // Release also refuses to grow the table past the bound.
        assert_eq!(
            runtime.clear_report("another-new-source", 1),
            Err("agent report source limit reached")
        );
    }

    #[test]
    fn session_identity_is_reported_once_and_survives_state_only_reports() {
        let catalog = catalog();
        let mut runtime = AgentRuntime::new();
        runtime
            .report(
                AgentReport {
                    identity: identity(&catalog, "codex"),
                    state: AgentState::Working,
                    source: "codex-hook".into(),
                    sequence: 1,
                    message: None,
                    session: AgentSessionRef::new(Some("conversation-7".into()), None),
                },
                false,
            )
            .unwrap();
        assert_eq!(runtime.session().unwrap().id(), Some("conversation-7"));
        assert!(runtime.snapshot().unwrap().session_present);

        // Integrations report identity once and state repeatedly; the later state must not erase
        // the reference the resume path depends on.
        runtime
            .report(
                state_report(
                    identity(&catalog, "codex"),
                    AgentState::Blocked,
                    "codex-hook",
                    2,
                ),
                false,
            )
            .unwrap();
        assert_eq!(runtime.session().unwrap().id(), Some("conversation-7"));

        // An integration typically reports before the detector first observes the process, so the
        // startup-grace path must preserve the reference rather than treat it as stale evidence.
        assert!(!runtime.observe_process(Some(40), Some(identity(&catalog, "codex"))));
        assert_eq!(runtime.session().unwrap().id(), Some("conversation-7"));

        // A different foreground process is a different conversation.
        assert!(runtime.observe_process(Some(41), Some(identity(&catalog, "codex"))));
        assert!(runtime.session().is_none());
        assert!(runtime.snapshot().unwrap().message.is_none());
    }

    #[test]
    fn a_removed_provider_takes_its_session_reference_with_it() {
        let full = catalog();
        let mut runtime = AgentRuntime::new();
        runtime
            .report(
                AgentReport {
                    identity: identity(&full, "codex"),
                    state: AgentState::Blocked,
                    source: "codex-hook".into(),
                    sequence: 1,
                    message: Some("waiting for approval: write src/main.rs".into()),
                    session: AgentSessionRef::new(None, Some("/tmp/codex/session.json".into())),
                },
                false,
            )
            .unwrap();
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(
            snapshot.message.as_deref(),
            Some("waiting for approval: write src/main.rs")
        );
        assert!(snapshot.session_present);

        assert!(runtime.reconcile_catalog(&custom_catalog()));
        assert!(runtime.snapshot().is_none());
        assert!(runtime.session().is_none());
    }

    #[test]
    fn oversized_report_annotations_are_rejected() {
        let catalog = catalog();
        let mut runtime = AgentRuntime::new();
        let report = |message: Option<String>, session: Option<AgentSessionRef>| AgentReport {
            identity: identity(&catalog, "codex"),
            state: AgentState::Blocked,
            source: "codex-hook".into(),
            sequence: 1,
            message,
            session,
        };
        assert_eq!(
            runtime.report(report(Some("m".repeat(257)), None), false),
            Err("agent report message must contain 1..=256 bytes")
        );
        assert_eq!(
            runtime.report(report(Some(String::new()), None), false),
            Err("agent report message must contain 1..=256 bytes")
        );
        assert_eq!(
            runtime.report(
                report(None, AgentSessionRef::new(Some("s".repeat(257)), None)),
                false
            ),
            Err("agent session identity must contain 1..=256 bytes")
        );
        // Nothing was committed by a rejected report, including its sequence slot.
        assert!(runtime.snapshot().is_none());
        runtime
            .report(
                state_report(
                    identity(&catalog, "codex"),
                    AgentState::Working,
                    "codex-hook",
                    1,
                ),
                false,
            )
            .unwrap();
    }

    fn token_patch(tokens: &[(&str, Option<&str>)], ttl: Option<Duration>) -> AgentMetadataPatch {
        AgentMetadataPatch {
            tokens: tokens
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.map(ToOwned::to_owned)))
                .collect(),
            ttl,
            ..AgentMetadataPatch::default()
        }
    }

    #[test]
    fn metadata_patches_only_the_keys_it_names() {
        let now = Instant::now();
        let mut runtime = AgentRuntime::new();
        runtime
            .report_metadata(
                "hook",
                1,
                token_patch(&[("summary", Some("one")), ("model", Some("opus"))], None),
                now,
            )
            .unwrap();
        // A second patch updates one token, clears another, and leaves the rest alone.
        assert!(
            runtime
                .report_metadata(
                    "hook",
                    2,
                    token_patch(&[("summary", Some("two")), ("model", None)], None),
                    now,
                )
                .unwrap()
        );
        assert_eq!(
            runtime.metadata().tokens().collect::<Vec<_>>(),
            [("summary", "two")]
        );
        // Re-applying the same value is not a display change, so it must not repaint.
        assert!(
            !runtime
                .report_metadata(
                    "hook",
                    3,
                    token_patch(&[("summary", Some("two"))], None),
                    now
                )
                .unwrap()
        );
    }

    #[test]
    fn metadata_tokens_expire_only_when_due() {
        let now = Instant::now();
        let mut runtime = AgentRuntime::new();
        runtime
            .report_metadata(
                "hook",
                1,
                token_patch(&[("short", Some("one"))], Some(Duration::from_secs(1))),
                now,
            )
            .unwrap();
        runtime
            .report_metadata("hook", 2, token_patch(&[("kept", Some("two"))], None), now)
            .unwrap();

        assert_eq!(
            runtime.metadata().next_expiry(),
            Some(now + Duration::from_secs(1))
        );
        assert!(!runtime.expire_metadata(now));
        assert!(runtime.expire_metadata(now + Duration::from_secs(2)));
        assert_eq!(
            runtime.metadata().tokens().collect::<Vec<_>>(),
            [("kept", "two")]
        );
        // Nothing timed remains, so the actor has no metadata wake to schedule.
        assert!(runtime.metadata().next_expiry().is_none());
    }

    #[test]
    fn a_rejected_metadata_patch_leaves_the_previous_display_intact() {
        let now = Instant::now();
        let mut runtime = AgentRuntime::new();
        runtime
            .report_metadata("hook", 1, token_patch(&[("kept", Some("one"))], None), now)
            .unwrap();

        // Over the token ceiling: the whole patch is refused rather than partly applied.
        let mut overflowing = Vec::new();
        for index in 0..MAX_METADATA_TOKENS {
            overflowing.push((format!("token-{index}"), Some("value".to_owned())));
        }
        assert_eq!(
            runtime.report_metadata(
                "hook",
                2,
                AgentMetadataPatch {
                    tokens: overflowing,
                    ..AgentMetadataPatch::default()
                },
                now,
            ),
            Err("metadata token limit reached")
        );
        assert_eq!(
            runtime.report_metadata(
                "hook",
                2,
                token_patch(&[("wide", Some(&"v".repeat(129)))], None),
                now
            ),
            Err("metadata token value must contain at most 128 printable bytes")
        );
        // Control characters would corrupt the navigator row they are drawn into.
        assert_eq!(
            runtime.report_metadata(
                "hook",
                2,
                token_patch(&[("bad", Some("a\u{1b}[2Jb"))], None),
                now
            ),
            Err("metadata token value must contain at most 128 printable bytes")
        );
        assert_eq!(
            runtime.report_metadata(
                "hook",
                2,
                token_patch(
                    &[("late", Some("x"))],
                    Some(Duration::from_secs(25 * 60 * 60))
                ),
                now
            ),
            Err("metadata TTL must be from 1ms through 24h")
        );

        assert_eq!(
            runtime.metadata().tokens().collect::<Vec<_>>(),
            [("kept", "one")]
        );
        // No rejected patch consumed the sequence slot it was offered.
        runtime
            .report_metadata("hook", 2, token_patch(&[("kept", Some("two"))], None), now)
            .unwrap();
    }

    #[test]
    fn metadata_overrides_display_without_touching_lifecycle_state() {
        let catalog = catalog();
        let now = Instant::now();
        let mut runtime = AgentRuntime::new();
        runtime
            .report(
                state_report(
                    identity(&catalog, "codex"),
                    AgentState::Working,
                    "codex-hook",
                    1,
                ),
                false,
            )
            .unwrap();
        let before = runtime.snapshot().unwrap();

        runtime
            .report_metadata(
                "codex-hook",
                2,
                AgentMetadataPatch {
                    tokens: vec![("files".into(), Some("42".into()))],
                    display_agent: Some(Some("Codex (review)".into())),
                    state_labels: vec![(AgentStatus::Working, Some("indexing".into()))],
                    title: Some(Some("reviewing src/agent.rs".into())),
                    ..AgentMetadataPatch::default()
                },
                now,
            )
            .unwrap();

        assert_eq!(runtime.metadata().display_agent(), Some("Codex (review)"));
        assert_eq!(
            runtime.metadata().state_label(AgentStatus::Working),
            Some("indexing")
        );
        assert_eq!(runtime.metadata().title(), Some("reviewing src/agent.rs"));
        // The snapshot is what waiters, events, and notifications react to: display-only metadata
        // must leave it untouched, or a progress counter becomes a stream of transitions.
        assert_eq!(runtime.snapshot().unwrap(), before);
    }

    #[test]
    fn metadata_shares_the_report_sequence_table_and_agent_lifetime() {
        let catalog = catalog();
        let now = Instant::now();
        let mut runtime = AgentRuntime::new();
        runtime
            .report_metadata("hook", 5, token_patch(&[("a", Some("1"))], None), now)
            .unwrap();
        // Ordering is shared with state, so metadata cannot be applied out of order against it.
        assert_eq!(
            runtime.report(
                state_report(identity(&catalog, "codex"), AgentState::Working, "hook", 4),
                false
            ),
            Err("agent report sequence is stale")
        );
        runtime
            .report(
                state_report(identity(&catalog, "codex"), AgentState::Working, "hook", 6),
                false,
            )
            .unwrap();
        assert_eq!(runtime.metadata().tokens().count(), 1);

        // Annotations describe one agent, so they leave with it.
        assert!(!runtime.observe_process(Some(70), Some(identity(&catalog, "codex"))));
        assert_eq!(runtime.metadata().tokens().count(), 1);
        assert!(runtime.observe_process(Some(71), Some(identity(&catalog, "codex"))));
        assert!(runtime.metadata().is_empty());
    }

    /// A terminal holding exactly `screen`, so explain and detection see the same bytes.
    fn terminal_showing(screen: &str) -> Terminal {
        let mut terminal = Terminal::new(24, 80, 100);
        terminal.feed(screen.replace('\n', "\r\n").as_bytes());
        terminal
    }

    fn explained(runtime: &AgentRuntime, catalog: &AgentCatalog, screen: &str) -> AgentExplanation {
        runtime
            .explain(catalog, &terminal_showing(screen), Instant::now())
            .unwrap()
    }

    fn detected_runtime(catalog: &AgentCatalog, agent: &str) -> AgentRuntime {
        let mut runtime = AgentRuntime::new();
        runtime.identity = Some(identity(catalog, agent));
        // Past the startup grace, so classification is what decides rather than the grace window.
        runtime.identified_at = Instant::now() - STARTUP_GRACE;
        runtime
    }

    #[test]
    fn explain_names_the_rule_detection_actually_used() {
        let catalog = catalog();
        // Every screen here is one the detection tests already pin, so a divergence between
        // `explain` and `detect` fails rather than being explained away.
        for (agent, screen, expected) in [
            (
                "codex",
                "Allow command? esc to interrupt",
                AgentState::Blocked,
            ),
            ("opencode", "■■■■", AgentState::Working),
            (
                "hermes",
                "Dangerous command — enter to confirm",
                AgentState::Blocked,
            ),
        ] {
            let runtime = detected_runtime(&catalog, agent);
            let terminal = terminal_showing(screen);
            let explanation = runtime
                .explain(&catalog, &terminal, Instant::now())
                .unwrap();
            // Pin the reported input to the snapshot classification uses. Both call
            // `detection_snapshot`; this fails if `explain` ever grows its own derivation.
            assert_eq!(
                explanation.detection_bytes,
                detection_snapshot(&terminal).len(),
                "{agent}"
            );
            let decided = explanation
                .rules
                .iter()
                .find(|rule| rule.decided)
                .unwrap_or_else(|| panic!("{agent}: no rule decided"));

            assert_eq!(explanation.decision, "rule_matched", "{agent}");
            assert_eq!(
                explanation.matched_rule.as_deref(),
                Some(decided.id.as_str())
            );
            let expected_label = match expected {
                AgentState::Idle => "idle",
                AgentState::Working => "working",
                AgentState::Blocked => "blocked",
            };
            assert_eq!(decided.state, expected_label, "{agent}");
            assert!(decided.matched);
            // Exactly one rule may claim the decision.
            assert_eq!(
                explanation.rules.iter().filter(|rule| rule.decided).count(),
                1,
                "{agent}"
            );
            // Shadowed rules are still reported, which is the point of evaluating all of them.
            assert!(explanation.rules.len() > 1, "{agent}");
            assert_eq!(
                detect_state(&catalog, &id(agent), screen, "", ""),
                expected,
                "{agent}"
            );
        }
    }

    #[test]
    fn explain_credits_the_highest_priority_rule_when_several_match() {
        let catalog = catalog();
        let runtime = detected_runtime(&catalog, "codex");
        // Matches codex's `live_strong_blocker` (priority 900, "allow command?") and its
        // `weak_blocker` (priority 600, "[y/n]"). Both say blocked, so only the credited rule ID
        // distinguishes first-match-wins from last-match-wins.
        let explanation = explained(&runtime, &catalog, "Allow command? [y/n]");

        assert!(explanation.rules.iter().filter(|rule| rule.matched).count() >= 2);
        assert_eq!(
            explanation.matched_rule.as_deref(),
            Some("live_strong_blocker")
        );
        assert_eq!(
            explanation.rules.iter().filter(|rule| rule.decided).count(),
            1
        );
        // The shadowed rule is still reported, flagged as matching but not deciding.
        let shadowed = explanation
            .rules
            .iter()
            .find(|rule| rule.id == "weak_blocker")
            .unwrap();
        assert!(shadowed.matched);
        assert!(!shadowed.decided);
    }

    #[test]
    fn explain_reports_why_no_rule_decided() {
        let catalog = catalog();
        let runtime = detected_runtime(&catalog, "codex");
        let explanation = explained(&runtime, &catalog, "ordinary shell output");
        assert_eq!(explanation.decision, "no_rule_matched");
        assert!(explanation.matched_rule.is_none());
        assert!(explanation.rules.iter().all(|rule| !rule.decided));

        // A rule that preserves the previous state is a decision, and a distinct one.
        let claude = detected_runtime(&catalog, "claude");
        let preserved = explained(
            &claude,
            &catalog,
            "showing detailed transcript\nctrl+o to toggle\n? for shortcuts",
        );
        assert_eq!(preserved.decision, "state_preserved");
        assert!(
            preserved
                .rules
                .iter()
                .find(|rule| rule.decided)
                .unwrap()
                .skip_state_update
        );
    }

    #[test]
    fn explain_shows_a_report_overriding_a_disagreeing_screen() {
        let catalog = catalog();
        let mut runtime = detected_runtime(&catalog, "codex");
        runtime
            .report(
                AgentReport {
                    identity: identity(&catalog, "codex"),
                    state: AgentState::Idle,
                    source: "codex-hook".into(),
                    sequence: 1,
                    message: Some("finished".into()),
                    session: None,
                },
                false,
            )
            .unwrap();

        let explanation = explained(&runtime, &catalog, "Allow command? esc to interrupt");
        assert_eq!(explanation.decision, "reported");
        assert_eq!(explanation.source, AgentSource::Report);
        let report = explanation.report.as_ref().unwrap();
        assert_eq!(report.source, "codex-hook");
        assert_eq!(report.message.as_deref(), Some("finished"));
        // The screen still says blocked; surfacing that disagreement is the whole point.
        assert_eq!(
            explanation
                .rules
                .iter()
                .find(|rule| rule.decided)
                .unwrap()
                .state,
            "blocked"
        );
    }

    #[test]
    fn explain_is_read_only_and_bounds_its_evidence() {
        let catalog = catalog();
        let mut runtime = detected_runtime(&catalog, "codex");
        runtime.state = AgentState::Working;
        let terminal = terminal_showing(&format!("noise {}", "x".repeat(4096)));

        let before = runtime.snapshot();
        let explanation = runtime
            .explain(&catalog, &terminal, Instant::now())
            .unwrap();
        // A diagnostic must not advance the idle-confirmation machinery it is describing.
        assert_eq!(runtime.snapshot(), before);
        assert_eq!(runtime.pending_idle, None);
        assert_eq!(explanation.pending_idle_confirmations, 0);

        for rule in &explanation.rules {
            assert!(rule.region_preview.len() <= MAX_REGION_PREVIEW_BYTES);
            assert!(!rule.region_preview.chars().any(char::is_control));
        }

        // No identity means classification never ran, which is distinct from "nothing matched".
        assert!(
            AgentRuntime::new()
                .explain(&catalog, &terminal, Instant::now())
                .is_none()
        );
    }

    #[test]
    fn explain_reports_the_startup_grace_window() {
        let catalog = catalog();
        let mut runtime = AgentRuntime::new();
        runtime.identity = Some(identity(&catalog, "codex"));
        let explanation = explained(&runtime, &catalog, "Allow command? esc to interrupt");
        assert!(explanation.startup_grace_active);
        assert_eq!(explanation.decision, "startup_grace");
    }

    #[test]
    fn a_session_reference_is_redacted_in_diagnostics() {
        let session = AgentSessionRef::new(
            Some("secret-conversation".into()),
            Some("/home/user/.codex/secret.json".into()),
        )
        .unwrap();
        let rendered = format!("{session:?}");
        assert!(!rendered.contains("secret-conversation"));
        assert!(!rendered.contains("secret.json"));
        assert_eq!(rendered, "AgentSessionRef { id: true, path: true }");
    }

    #[test]
    fn codex_live_confirmation_is_blocked() {
        let catalog = catalog();
        assert_eq!(
            detect_state(
                &catalog,
                &id("codex"),
                "Allow command? esc to interrupt",
                "",
                ""
            ),
            AgentState::Blocked
        );
    }

    #[test]
    fn hidden_completion_becomes_done_until_seen() {
        let catalog = catalog();
        let mut runtime = AgentRuntime::new();
        runtime.identity = Some(identity(&catalog, "codex"));
        runtime.identified_at = Instant::now() - STARTUP_GRACE;
        runtime.commit(AgentState::Working, AgentSource::Screen, false);
        runtime.commit(AgentState::Idle, AgentSource::Screen, false);
        assert_eq!(runtime.snapshot().unwrap().status, AgentStatus::Done);
        runtime.mark_seen();
        assert_eq!(runtime.snapshot().unwrap().status, AgentStatus::Idle);
    }

    #[test]
    fn reports_are_ordered_and_scoped_per_runtime() {
        let catalog = catalog();
        let mut first = AgentRuntime::new();
        let mut second = AgentRuntime::new();
        first
            .report(
                state_report(
                    identity(&catalog, "opencode"),
                    AgentState::Working,
                    "test",
                    2,
                ),
                false,
            )
            .unwrap();
        second
            .report(
                state_report(identity(&catalog, "opencode"), AgentState::Idle, "test", 2),
                false,
            )
            .unwrap();
        assert!(
            first
                .report(
                    state_report(
                        identity(&catalog, "opencode"),
                        AgentState::Blocked,
                        "test",
                        1
                    ),
                    false
                )
                .is_err()
        );
        first.clear_report("test", 3).unwrap();
        assert_eq!(first.snapshot().unwrap().source, AgentSource::Screen);
        assert_eq!(second.snapshot().unwrap().source, AgentSource::Report);
    }

    #[test]
    fn opencode_progress_and_hermes_prompts_are_detected() {
        let catalog = catalog();
        assert_eq!(
            detect_state(&catalog, &id("opencode"), "■■■■", "", ""),
            AgentState::Working
        );
        assert_eq!(
            detect_state(
                &catalog,
                &id("hermes"),
                "Dangerous command — enter to confirm",
                "",
                ""
            ),
            AgentState::Blocked
        );
    }

    #[test]
    fn live_regions_do_not_reclassify_misleading_transcript_text() {
        let catalog = catalog();
        assert_eq!(
            detect_state(
                &catalog,
                &id("codex"),
                "Allow command?\n› explain that old output",
                "",
                ""
            ),
            AgentState::Idle
        );
        assert!(
            detect_candidate(
                &catalog,
                &id("claude"),
                "showing detailed transcript\nctrl+o to toggle\n? for shortcuts",
                "",
                ""
            )
            .is_none()
        );
    }

    #[test]
    fn visible_prompt_box_idle_is_distinct_from_fallback_idle() {
        let catalog = catalog();
        let visible =
            detect_candidate(&catalog, &id("claude"), "────────\n❯ \n────────", "", "").unwrap();
        assert_eq!(visible.state, AgentState::Idle);
        assert!(visible.visible_idle);
        let fallback =
            detect_candidate(&catalog, &id("claude"), "ordinary output", "", "").unwrap();
        assert_eq!(fallback.state, AgentState::Idle);
        assert!(!fallback.visible_idle);
    }

    #[test]
    fn foreground_replacement_clears_only_that_panes_report_authority() {
        let catalog = catalog();
        let mut runtime = AgentRuntime::new();
        runtime
            .report(
                state_report(
                    identity(&catalog, "opencode"),
                    AgentState::Working,
                    "plugin",
                    1,
                ),
                false,
            )
            .unwrap();
        assert!(!runtime.observe_process(Some(10), Some(identity(&catalog, "opencode"))));
        assert_eq!(runtime.snapshot().unwrap().source, AgentSource::Report);
        assert!(runtime.observe_process(Some(11), Some(identity(&catalog, "opencode"))));
        assert_eq!(runtime.snapshot().unwrap().source, AgentSource::Screen);
        assert!(
            runtime
                .report(
                    state_report(
                        identity(&catalog, "codex"),
                        AgentState::Blocked,
                        "plugin",
                        2
                    ),
                    false
                )
                .is_err()
        );

        runtime
            .report(
                state_report(
                    identity(&catalog, "opencode"),
                    AgentState::Working,
                    "plugin",
                    3,
                ),
                false,
            )
            .unwrap();
        assert!(!runtime.observe_process(Some(11), None));
        assert!(runtime.snapshot().is_none());
    }
}
