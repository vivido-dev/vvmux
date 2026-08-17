//! Session state that outlives the daemon which produced it.
//!
//! A session's *shape* — its tabs, splits, floats, focus, and the working directories its panes
//! started in — is captured continuously and restored when the hidden server comes back. This module
//! owns the on-disk form of that state and nothing else: it decides how a snapshot is encoded,
//! bounded, and validated, and leaves when to capture and how to apply to the session actor.
//!
//! Three rules shape everything here.
//!
//! **A snapshot is untrusted input.** It is a file, so it may have been edited, truncated, or written
//! by a different build. Nothing in it is believed: sizes are bounded before allocation, the schema
//! is read before the body, and validated newtypes such as agent IDs and aliases are stored as plain
//! strings and re-validated by the caller on the way back in. Anything unusable makes the session
//! start fresh, never fail to start.
//!
//! **A snapshot is private.** It records the working directories a user is in, the titles their panes
//! carry, and native agent session identity, which is capability-adjacent — it names a resumable
//! conversation on the user's agent account. Both files are owner-only, in an owner-only directory
//! ([`crate::runtime::SnapshotPaths`]), and neither is ever added to a debug bundle.
//!
//! **A snapshot is not a layout file.** It embeds a [`LayoutFile`] for the shape, so restore reuses
//! the same lowering that `startup.toml` goes through, but it is a separate document in a separate
//! directory. `startup.toml` is something users write, share, and commit; this is machine state
//! holding secrets-adjacent material, and the two must not be the same file.
//!
//! Two documents share all of this: the shape snapshot, which is always written when a session
//! persists at all, and the opt-in pane history, which is large, holds whatever scrolled past a
//! terminal, and is therefore kept in its own file so enabling or disabling it never rewrites the
//! shape.
#![allow(dead_code)]

use std::io;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::layout_file::LayoutFile;

/// The on-disk generation of every document in this module.
///
/// Bump this whenever an existing field changes meaning, or when a nested type that carries
/// `deny_unknown_fields` — [`LayoutFile`] does — gains a field. Adding a new optional field to the
/// extras below needs no bump, because they are all `#[serde(default)]` and tolerate absence in both
/// directions.
pub const SNAPSHOT_SCHEMA: u16 = 1;

/// Ceiling for a shape snapshot.
///
/// A session is capped at [`crate::layout_file::MAX_LAYOUT_PANES`] panes, each contributing a path, a
/// title, and at most one agent session reference, so a legitimate snapshot is a few tens of
/// kilobytes. This is roughly four times that: large enough that no real session is refused, small
/// enough that a corrupted length is not a memory problem.
pub const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024;

/// Ceiling for the pane-history *file*, on both read and write.
///
/// Deliberately above the capture budget rather than equal to it. Capture measures the text it
/// collects; the file additionally carries JSON structure and escaping, so a capture that filled a
/// 4 MiB budget exactly would serialize past it and be refused on every write. The headroom means a
/// maximal capture always fits, while a corrupted or hostile length is still bounded before it is
/// allocated.
///
/// A hard cap either way, unlike herdr, which serializes the entire scrollback of every pane with no
/// limit at all.
pub const MAX_HISTORY_BYTES: u64 = 8 * 1024 * 1024;

/// Everything needed to rebuild a session's shape.
///
/// The shape lives in `layout` as an ordinary [`LayoutFile`] so restore can hand it to the same
/// lowering `startup.toml` uses. Everything a layout file deliberately does not describe — which tab
/// was active, which pane was zoomed, where a float sat, what an agent was named — lives in `extras`,
/// keyed by the pane slot indices that lowering assigns. Keeping them in one file means shape and
/// extras can never disagree about which panes exist.
#[derive(Debug, Deserialize, Serialize)]
pub struct SessionSnapshot {
    pub schema: u16,
    pub layout: LayoutFile,
    #[serde(default)]
    pub extras: SnapshotExtras,
}

/// Session state a layout file does not carry.
///
/// Every field defaults, so a snapshot written before a field existed restores as if that part of the
/// session were untouched rather than being rejected.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SnapshotExtras {
    /// Index into `tabs`, clamped by the restorer — a file may name a tab that no longer exists.
    pub active_tab: usize,
    pub tabs: Vec<TabExtras>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TabExtras {
    /// Slot of the zoomed leaf, if the tab was zoomed. Zoom is a projection of one leaf and never
    /// part of the layout tree, which is why it cannot live in the [`LayoutFile`].
    pub zoomed: Option<usize>,
    pub sync_input: bool,
    pub floats: Vec<FloatExtras>,
    pub panes: Vec<PaneExtras>,
}

/// Where a float sat, as a percentage of the host area.
///
/// A layout file records a float's *size* but not its position, because a hand-written layout should
/// not have to place windows. A snapshot is describing a session that existed, so it records both.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct FloatExtras {
    pub slot: usize,
    pub x_percent: u16,
    pub y_percent: u16,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PaneExtras {
    pub slot: usize,
    pub title: Option<String>,
    pub agent: Option<PaneAgentExtras>,
}

/// What was known about the agent running in a pane.
///
/// `alias` and `kind` are plain strings rather than their validated newtypes on purpose: this is file
/// content, and the newtype's guarantee has to be re-established by parsing on load rather than
/// assumed because the field has a type. `session_id` and `session_path` mirror
/// `crate::agent::AgentSessionRef`'s own two halves instead of collapsing to herdr's single
/// kind-and-value pair, which cannot represent an integration that reports both.
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PaneAgentExtras {
    /// The name a user gave this agent, if any.
    pub alias: Option<String>,
    /// The agent kind that was detected, as a provider ID.
    pub kind: Option<String>,
    /// The reporting source that supplied the session reference, checked before any resume is built
    /// from it.
    pub session_source: Option<String>,
    pub session_id: Option<String>,
    pub session_path: Option<String>,
}

/// Redacted for the same reason `AgentSessionRef`'s is: this type exists partly to be withheld, and a
/// derived `Debug` would leak a resumable session identity through any diagnostic that formats a
/// snapshot.
impl std::fmt::Debug for PaneAgentExtras {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaneAgentExtras")
            .field("alias", &self.alias)
            .field("kind", &self.kind)
            .field("session_source", &self.session_source)
            .field("session_id", &self.session_id.is_some())
            .field("session_path", &self.session_path.is_some())
            .finish()
    }
}

impl SessionSnapshot {
    pub fn new(layout: LayoutFile, extras: SnapshotExtras) -> Self {
        Self {
            schema: SNAPSHOT_SCHEMA,
            layout,
            extras,
        }
    }
}

/// The one field every document here shares, read before the rest of the body.
///
/// Parsed on its own so a snapshot from a newer vvmux is reported as exactly that, rather than as
/// whichever field happened to fail first. Unknown fields are ignored, which is the point.
#[derive(Deserialize)]
struct SchemaProbe {
    schema: u16,
}

/// Read a persisted document.
///
/// `Ok(None)` means the file is simply not there — the ordinary first-run case, and not a problem.
/// `Err` means a file exists but cannot be used: too large, unsafe permissions, written by a newer
/// build, or malformed. Callers are expected to report the error and continue with a fresh session:
/// losing a restore is a disappointment, but failing to start is a broken multiplexer. That policy
/// lives at the call site rather than here so this function stays honest about what it found.
pub fn load<T: DeserializeOwned>(path: &Path, limit: u64, what: &str) -> io::Result<Option<T>> {
    let bytes = match crate::runtime::read_private_bytes(path, false, limit, what) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let probe: SchemaProbe = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{what} is not a vvmux session document: {error}"),
        )
    })?;
    if probe.schema > SNAPSHOT_SCHEMA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{what} has schema {} but this vvmux understands {SNAPSHOT_SCHEMA}; it was written \
                 by a newer build",
                probe.schema
            ),
        ));
    }
    let document = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{what} is malformed: {error}"),
        )
    })?;
    Ok(Some(document))
}

/// Write a persisted document privately and atomically, refusing to exceed its own bound.
///
/// The size is checked against `limit` before anything touches the filesystem, so an over-budget
/// capture is a caught bug rather than a file that cannot be read back.
pub fn save<T: Serialize>(path: &Path, document: &T, limit: u64, what: &str) -> io::Result<()> {
    let bytes = serde_json::to_vec(document).map_err(io::Error::other)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{what} would be {} bytes, over its {limit}-byte budget",
                bytes.len()
            ),
        ));
    }
    crate::runtime::write_private_atomic(path, &bytes, "state")
}

/// Remove a persisted document, treating an already-absent file as success.
pub fn clear(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// What a pane's screen looked like, in a form the ANSI parser never sees again.
///
/// Text and style only. Hyperlinks, graphics placements, media anchors, cursor position, and
/// terminal modes are all dropped: a hyperlink is an attacker-influenced URL that would come back
/// clickable, graphics transfer state is explicitly never retained session state, and an anchor
/// belongs to a capability that died with the daemon that issued it. What survives is what a person
/// would recognize as "what was on the screen".
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionHistory {
    pub schema: u16,
    pub tabs: Vec<TabHistory>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TabHistory {
    pub panes: Vec<PaneHistory>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PaneHistory {
    /// The pane slot this belongs to, keyed exactly as [`PaneExtras`] is.
    pub slot: usize,
    /// Whether older lines were dropped to stay inside the budget.
    pub truncated: bool,
    /// Distinct styles, referenced by index from every run below.
    pub styles: Vec<HistoryStyle>,
    /// Scrollback lines, oldest first.
    pub rows: Vec<HistoryRow>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HistoryRow {
    /// Whether this line continues into the next, so a restored log rewraps as it did.
    pub wrapped: bool,
    pub runs: Vec<HistoryRun>,
}

/// A stretch of text sharing one style, which is how a terminal line actually looks: a prompt, a
/// path, a diff marker. Storing per cell would multiply a line by its width for no fidelity.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HistoryRun {
    pub style: usize,
    pub text: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(default)]
pub struct HistoryStyle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<HistoryColor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<HistoryColor>,
    #[serde(skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub dim: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub underline: u8,
    #[serde(skip_serializing_if = "is_false")]
    pub blink: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub inverse: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub hidden: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub strikeout: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryColor {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u8) -> bool {
    *value == 0
}

impl SessionHistory {
    pub fn new(tabs: Vec<TabHistory>) -> Self {
        Self {
            schema: SNAPSHOT_SCHEMA,
            tabs,
        }
    }
}

pub fn load_history(path: &Path) -> io::Result<Option<SessionHistory>> {
    load(path, MAX_HISTORY_BYTES, "session pane history")
}

pub fn save_history(path: &Path, history: &SessionHistory) -> io::Result<()> {
    save(path, history, MAX_HISTORY_BYTES, "session pane history")
}

pub fn load_snapshot(path: &Path) -> io::Result<Option<SessionSnapshot>> {
    load(path, MAX_SNAPSHOT_BYTES, "session snapshot")
}

pub fn save_snapshot(path: &Path, snapshot: &SessionSnapshot) -> io::Result<()> {
    save(path, snapshot, MAX_SNAPSHOT_BYTES, "session snapshot")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout_file::{LayoutFile, LayoutNode, LayoutTab};

    fn temporary_directory() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    /// A two-pane, one-tab session: enough shape that a serialization bug shows up as a difference
    /// rather than as an empty document that happens to round-trip.
    fn sample() -> SessionSnapshot {
        let layout = LayoutFile::from_tabs(vec![LayoutTab::new(
            Some("work".to_owned()),
            Some("left".to_owned()),
            Some(LayoutNode::split(
                crate::ipc::Axis::Vertical,
                vec![600, 400],
                vec![
                    LayoutNode::leaf("left".to_owned(), Some("/tmp".to_owned()), false),
                    LayoutNode::leaf("right".to_owned(), None, true),
                ],
            )),
            Vec::new(),
        )]);
        SessionSnapshot::new(
            layout,
            SnapshotExtras {
                active_tab: 0,
                tabs: vec![TabExtras {
                    zoomed: Some(1),
                    sync_input: true,
                    floats: vec![FloatExtras {
                        slot: 1,
                        x_percent: 20,
                        y_percent: 30,
                    }],
                    panes: vec![PaneExtras {
                        slot: 0,
                        title: Some("editor".to_owned()),
                        agent: Some(PaneAgentExtras {
                            alias: Some("reviewer".to_owned()),
                            kind: Some("claude".to_owned()),
                            session_source: Some("vvmux:claude".to_owned()),
                            session_id: Some("abc123".to_owned()),
                            session_path: None,
                        }),
                    }],
                }],
            },
        )
    }

    fn round_trip(snapshot: &SessionSnapshot) -> SessionSnapshot {
        let directory = temporary_directory();
        let path = directory.path().join("session.json");
        save_snapshot(&path, snapshot).unwrap();
        load_snapshot(&path).unwrap().unwrap()
    }

    /// `LayoutFile` and the extras are compared through their serialized form because the layout
    /// types intentionally expose no field accessors — and because equality of the encoding is
    /// exactly the property a persisted document needs.
    fn encoded(snapshot: &SessionSnapshot) -> String {
        serde_json::to_string(snapshot).unwrap()
    }

    #[test]
    fn a_snapshot_survives_a_write_and_a_read() {
        let original = sample();
        let restored = round_trip(&original);
        assert_eq!(encoded(&original), encoded(&restored));
        assert_eq!(restored.schema, SNAPSHOT_SCHEMA);
        assert_eq!(restored.layout.counts(), (1, 2));
        let pane = &restored.extras.tabs[0].panes[0];
        let agent = pane.agent.as_ref().unwrap();
        assert_eq!(agent.alias.as_deref(), Some("reviewer"));
        assert_eq!(agent.session_id.as_deref(), Some("abc123"));
        assert_eq!(restored.extras.tabs[0].zoomed, Some(1));
        assert!(restored.extras.tabs[0].sync_input);
    }

    #[test]
    fn a_missing_snapshot_is_not_an_error() {
        let directory = temporary_directory();
        let absent = directory.path().join("nothing.json");
        assert!(load_snapshot(&absent).unwrap().is_none());
    }

    /// The whole point of the schema probe: a file from a future vvmux must be reported as such, not
    /// as a parse failure in whichever field changed.
    #[test]
    fn a_newer_schema_is_refused_by_name() {
        let directory = temporary_directory();
        let path = directory.path().join("session.json");
        let mut snapshot = sample();
        snapshot.schema = SNAPSHOT_SCHEMA + 1;
        save_snapshot(&path, &snapshot).unwrap();
        let error = load_snapshot(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("newer build"),
            "unexpected message: {error}"
        );
    }

    #[test]
    fn a_malformed_snapshot_is_refused_rather_than_guessed() {
        let directory = temporary_directory();
        for (name, contents) in [
            ("truncated.json", "{\"schema\":1,\"layout\":"),
            ("empty.json", ""),
            ("no-schema.json", "{\"layout\":{\"tabs\":[]}}"),
            ("wrong-shape.json", "[1,2,3]"),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, contents).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let error = load_snapshot(&path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{name}");
        }
    }

    /// An absent extras block must restore as "nothing extra was recorded", so a snapshot written
    /// before a field existed still restores its shape.
    #[test]
    fn extras_default_when_absent() {
        let directory = temporary_directory();
        let path = directory.path().join("session.json");
        let layout = serde_json::to_string(&sample().layout).unwrap();
        std::fs::write(
            &path,
            format!("{{\"schema\":{SNAPSHOT_SCHEMA},\"layout\":{layout}}}"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let restored = load_snapshot(&path).unwrap().unwrap();
        assert_eq!(restored.extras.active_tab, 0);
        assert!(restored.extras.tabs.is_empty());
        assert_eq!(restored.layout.counts(), (1, 2));
    }

    /// An unknown field in the extras must not fail the load: the extras are the part expected to
    /// grow, and an older build reading a newer file should lose the field, not the session.
    #[test]
    fn unknown_extras_fields_are_ignored() {
        let directory = temporary_directory();
        let path = directory.path().join("session.json");
        let layout = serde_json::to_string(&sample().layout).unwrap();
        std::fs::write(
            &path,
            format!(
                "{{\"schema\":{SNAPSHOT_SCHEMA},\"layout\":{layout},\
                 \"extras\":{{\"active_tab\":0,\"tabs\":[],\"invented_later\":true}}}}"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(load_snapshot(&path).unwrap().is_some());
    }

    #[test]
    fn an_over_budget_document_is_refused_before_it_is_written() {
        let directory = temporary_directory();
        let path = directory.path().join("session.json");
        let error = save(&path, &sample(), 16, "session snapshot").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("budget"),
            "unexpected message: {error}"
        );
        assert!(!path.exists(), "a refused save must leave no file behind");
    }

    /// A file over its bound must be refused by length rather than parsed, so a corrupted or hostile
    /// length can never become a large allocation.
    #[test]
    fn an_over_budget_file_is_refused_without_parsing() {
        let directory = temporary_directory();
        let path = directory.path().join("session.json");
        std::fs::write(&path, "x".repeat(64)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let error = load::<SessionSnapshot>(&path, 16, "session snapshot").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("exceeds"),
            "unexpected message: {error}"
        );
    }

    #[test]
    fn clearing_an_absent_document_succeeds() {
        let directory = temporary_directory();
        let path = directory.path().join("session.json");
        clear(&path).unwrap();
        save_snapshot(&path, &sample()).unwrap();
        assert!(path.exists());
        clear(&path).unwrap();
        assert!(!path.exists());
    }

    /// A snapshot records working directories, pane titles, and resumable agent identity, so the file
    /// mode is part of the feature rather than a detail.
    #[cfg(unix)]
    #[test]
    fn a_written_snapshot_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temporary_directory();
        let path = directory.path().join("session.json");
        save_snapshot(&path, &sample()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// A group- or world-readable snapshot is refused on read too, so a file whose mode was widened
    /// after it was written is not silently trusted.
    #[cfg(unix)]
    #[test]
    fn a_widened_snapshot_is_refused_on_read() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temporary_directory();
        let path = directory.path().join("session.json");
        save_snapshot(&path, &sample()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = load_snapshot(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    /// A resumable session identity must not reach a log through a formatted snapshot.
    #[test]
    fn agent_session_identity_is_redacted_in_debug_output() {
        let snapshot = sample();
        let formatted = format!("{snapshot:?}");
        assert!(
            !formatted.contains("abc123"),
            "session identity leaked: {formatted}"
        );
        assert!(formatted.contains("session_id: true"), "{formatted}");
        assert!(formatted.contains("reviewer"), "{formatted}");
    }
}
