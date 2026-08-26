//! Bounded session recording, for reproducing what a session did.
//!
//! A recording answers "what happened" after the fact, which neither a screen snapshot nor a
//! transcript can: those describe one pane at one moment, and a session is several panes, a layout
//! that changed, agents whose state moved, and a client acknowledging renders.
//!
//! Three rules shape what goes in.
//!
//! It records **input classes, never input content.** A recording is a file, and typed input is
//! passwords, tokens, and whatever else was on its way to a program. Knowing that 14 bytes were
//! typed into pane 2 reproduces the shape of a session; knowing which 14 bytes is a credential
//! leak with a plausible excuse. Output is recorded, because that is the thing being reproduced —
//! and it is the same data `[session] pane_history` already gates behind an explicit opt-in, which
//! is why starting a recording is always an explicit act and never a default.
//!
//! It is **bounded**, and says so when it drops. A recording that grew without limit would be a
//! disk-filling bug wearing a feature's clothes.
//!
//! It is keyed by the **sequences the session already has**, with wall-clock time as an aside.
//! Replay reproduces state, not timing: the ordering is authoritative and reproducible, while
//! elapsed time between two events is a property of the machine that ran them.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::layout::PaneId;

/// Events one recording retains in memory before it is written.
const MAX_RECORDED_EVENTS: usize = 65_536;
/// Total recorded output bytes, across every pane.
const MAX_RECORDED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordedEvent {
    /// The session's shape when recording began, as the startup-layout schema describes it.
    Opened {
        session: String,
        layout: serde_json::Value,
    },
    /// Bytes a pane wrote.
    Output {
        pane_id: PaneId,
        base64: String,
    },
    /// That a pane was written to, and how much — never what.
    ///
    /// The one deliberate hole in a recording's fidelity. Replaying input would mean storing it,
    /// and a file that stores everything typed into a terminal is a credential dump.
    Input {
        pane_id: PaneId,
        bytes: usize,
        /// `typing`, `key`, `paste`, `mouse`, and so on: the method, not its payload.
        class: String,
    },
    Layout {
        layout_sequence: u64,
        layout: serde_json::Value,
    },
    PaneExited {
        pane_id: PaneId,
        code: Option<i64>,
        signal: Option<i32>,
    },
    AgentState {
        pane_id: PaneId,
        status: String,
    },
    /// Events the bound dropped, so a gap is never silent.
    Gap {
        events: usize,
        bytes: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedFrame {
    pub sequence: u64,
    pub session_sequence: u64,
    /// Milliseconds since recording began. Advisory: ordering is what replay uses.
    pub elapsed_ms: u64,
    #[serde(flatten)]
    pub event: RecordedEvent,
}

pub struct Recorder {
    path: PathBuf,
    started: Instant,
    sequence: u64,
    events: VecDeque<RecordedFrame>,
    bytes: usize,
    dropped_events: usize,
    dropped_bytes: usize,
}

impl Recorder {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            started: Instant::now(),
            sequence: 0,
            events: VecDeque::new(),
            bytes: 0,
            dropped_events: 0,
            dropped_bytes: 0,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn push(&mut self, session_sequence: u64, event: RecordedEvent) {
        let size = match &event {
            RecordedEvent::Output { base64, .. } => base64.len(),
            _ => 0,
        };
        self.sequence = self.sequence.saturating_add(1);
        self.events.push_back(RecordedFrame {
            sequence: self.sequence,
            session_sequence,
            elapsed_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            event,
        });
        self.bytes = self.bytes.saturating_add(size);
        while self.events.len() > MAX_RECORDED_EVENTS || self.bytes > MAX_RECORDED_BYTES {
            let Some(dropped) = self.events.pop_front() else {
                break;
            };
            if let RecordedEvent::Output { base64, .. } = &dropped.event {
                self.bytes = self.bytes.saturating_sub(base64.len());
                self.dropped_bytes = self.dropped_bytes.saturating_add(base64.len());
            }
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
    }

    /// Write the recording out as NDJSON.
    ///
    /// A gap frame goes first when anything was dropped, so a reader learns the recording is
    /// partial before it reads a single event rather than after trusting the whole thing.
    pub fn finish(self) -> io::Result<serde_json::Value> {
        let mut encoded = Vec::new();
        if self.dropped_events > 0 {
            let gap = RecordedFrame {
                sequence: 0,
                session_sequence: 0,
                elapsed_ms: 0,
                event: RecordedEvent::Gap {
                    events: self.dropped_events,
                    bytes: self.dropped_bytes,
                },
            };
            serde_json::to_writer(&mut encoded, &gap).map_err(io::Error::other)?;
            writeln!(encoded)?;
        }
        let events = self.events.len();
        for frame in &self.events {
            serde_json::to_writer(&mut encoded, frame).map_err(io::Error::other)?;
            writeln!(encoded)?;
        }
        // Owner-only and atomic, like every other file a session writes: a recording holds pane
        // output, which is whatever scrolled past.
        crate::runtime::write_private_atomic(&self.path, &encoded, "recording")?;
        Ok(serde_json::json!({
            "path": self.path.display().to_string(),
            "events": events,
            "dropped_events": self.dropped_events,
            "dropped_bytes": self.dropped_bytes,
        }))
    }
}

/// Replay a recording into a description of what the session became.
///
/// Deliberately not a re-execution. Replay does not spawn a process, does not write to a PTY, and
/// does not deliver a media payload: a recording is evidence about a session that already ran, and
/// running its commands again would be a different session with different side effects — possibly
/// destructive ones. What it reconstructs is terminal and layout state.
pub fn replay(path: &Path, pane_filter: Option<PaneId>) -> io::Result<serde_json::Value> {
    use std::io::BufRead as _;
    let file = std::fs::File::open(path)?;
    let mut terminals: std::collections::BTreeMap<PaneId, vvmux_terminal::Terminal> =
        std::collections::BTreeMap::new();
    let mut layout = serde_json::Value::Null;
    let mut session = String::new();
    let mut exits = Vec::new();
    let (mut events, mut gap) = (0_usize, None);

    for line in io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let frame: RecordedFrame = serde_json::from_str(&line).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid recording frame: {error}"),
            )
        })?;
        events += 1;
        match frame.event {
            RecordedEvent::Opened {
                session: name,
                layout: opened,
            } => {
                session = name;
                layout = opened;
            }
            RecordedEvent::Output { pane_id, base64 } => {
                if pane_filter.is_some_and(|wanted| wanted != pane_id) {
                    continue;
                }
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&base64)
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid recorded output")
                    })?;
                terminals
                    .entry(pane_id)
                    .or_insert_with(|| vvmux_terminal::Terminal::new(24, 80, 1_000))
                    .feed(&bytes);
            }
            RecordedEvent::Layout { layout: next, .. } => layout = next,
            RecordedEvent::PaneExited {
                pane_id,
                code,
                signal,
            } => exits.push(serde_json::json!({
                "pane_id": pane_id,
                "code": code,
                "signal": signal,
            })),
            RecordedEvent::Gap {
                events: dropped,
                bytes,
            } => {
                gap = Some(serde_json::json!({"events": dropped, "bytes": bytes}));
            }
            // Input carries no content by design, and an agent transition is metadata about a
            // process that is not being re-run. Both are in the file for a reader; neither
            // reconstructs terminal state.
            RecordedEvent::Input { .. } | RecordedEvent::AgentState { .. } => {}
        }
    }

    Ok(serde_json::json!({
        "session": session,
        "events": events,
        // Present only when the recording says it is partial.
        "gap": gap,
        "layout": layout,
        "exits": exits,
        "panes": terminals
            .iter()
            .map(|(pane_id, terminal)| serde_json::json!({
                "pane_id": pane_id,
                "text": terminal.visible_text(0),
                "columns": terminal.cols(),
                "rows": terminal.rows(),
            }))
            .collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(pane_id: PaneId, text: &str) -> RecordedEvent {
        use base64::Engine as _;
        RecordedEvent::Output {
            pane_id,
            base64: base64::engine::general_purpose::STANDARD.encode(text),
        }
    }

    #[test]
    fn a_recording_is_bounded_and_reports_what_it_dropped() {
        let mut recorder = Recorder::new(PathBuf::from("/dev/null"));
        for index in 0..(MAX_RECORDED_EVENTS + 16) {
            recorder.push(index as u64, output(1, "x"));
        }
        assert_eq!(recorder.len(), MAX_RECORDED_EVENTS);
        assert_eq!(recorder.dropped_events, 16);
    }

    #[test]
    fn replay_reconstructs_terminal_state_without_running_anything() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recording.ndjson");
        let mut recorder = Recorder::new(path.clone());
        recorder.push(
            1,
            RecordedEvent::Opened {
                session: "demo".into(),
                layout: serde_json::json!({"tabs": []}),
            },
        );
        recorder.push(2, output(1, "hello "));
        recorder.push(3, output(1, "world"));
        // Input is recorded as a class and a length, never as bytes.
        recorder.push(
            4,
            RecordedEvent::Input {
                pane_id: 1,
                bytes: 9,
                class: "typing".into(),
            },
        );
        recorder.push(
            5,
            RecordedEvent::PaneExited {
                pane_id: 1,
                code: Some(3),
                signal: None,
            },
        );
        let written = recorder.finish().unwrap();
        assert_eq!(written["events"], 5);

        // Nothing typed can be recovered from the file, because nothing typed was written to it.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"typing\""));
        assert!(!raw.contains("secret"));

        let replayed = replay(&path, None).unwrap();
        assert_eq!(replayed["session"], "demo");
        assert_eq!(replayed["exits"][0]["code"], 3);
        assert!(
            replayed["panes"][0]["text"]
                .as_str()
                .unwrap()
                .contains("hello world"),
            "{replayed}"
        );
        assert!(replayed["gap"].is_null());
    }
}
