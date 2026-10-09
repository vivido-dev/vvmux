//! Session snapshots: capture, restore, scrollback history, and agent resumes.

// clippy exempts wildcard imports from test builds, so the expectation is compiled in only
// where the lint can fire; a bare `expect` would be unfulfilled under `--cfg test`.
#![cfg_attr(
    not(test),
    expect(
        clippy::wildcard_imports,
        reason = "a part of the session actor, sharing its module's private vocabulary"
    )
)]

use super::*;

impl SessionActor {
    /// Mark the session shape as diverged from the last snapshot written.
    ///
    /// Cheap and idempotent on purpose: it is called from every mutation that changes shape, and a
    /// burst of them costs one snapshot rather than one each.
    pub(super) fn mark_snapshot_dirty(&mut self) {
        if self.snapshot_paths.is_none() {
            return;
        }
        self.snapshot_dirty = true;
        self.snapshot_due
            .get_or_insert_with(|| Instant::now() + SNAPSHOT_DEBOUNCE);
    }

    /// Start or stop persisting this session, following the live config.
    ///
    /// Turning it off discards the snapshot already on disk. Leaving a stale one behind would mean
    /// a session that opted out still gets restored from whatever it looked like when it did, which
    /// is the opposite of what the setting says.
    pub(super) fn apply_snapshot_setting(&mut self) {
        if self.config.session.auto_snapshot {
            if self.snapshot_paths.is_none() {
                self.snapshot_paths = crate::runtime::SnapshotPaths::for_session(&self.name).ok();
            }
            // Turning pane history off has to remove what is already written, not merely stop
            // adding to it, and it must not wait for a shape change that may never come.
            if !self.config.session.pane_history
                && let Some(paths) = self.snapshot_paths.clone()
                && let Err(error) = crate::session_state::clear(&paths.history)
            {
                log::warn!(
                    event = "session.history.discard.failure",
                    error:% = error;
                    "could not discard pane history"
                );
            }
            self.mark_snapshot_dirty();
            return;
        }
        let Some(paths) = self.snapshot_paths.take() else {
            return;
        };
        self.snapshot_dirty = false;
        self.snapshot_due = None;
        for path in [&paths.snapshot, &paths.history] {
            if let Err(error) = crate::session_state::clear(path) {
                log::warn!(
                    event = "session.snapshot.discard.failure",
                    path:% = crate::logging::redact_path(path),
                    error:% = error;
                    "could not discard a session snapshot"
                );
            }
        }
    }

    pub(super) fn next_snapshot_deadline(&self) -> Duration {
        self.snapshot_due.map_or(Duration::MAX, |due| {
            due.saturating_duration_since(Instant::now())
        })
    }

    /// Capture on the actor, write on a worker.
    ///
    /// The capture reads live state, so it has to run here; the write is filesystem I/O, which the
    /// actor must never block on. One write is in flight at a time — a second would race the first
    /// to the same path for no benefit — and a change arriving during one re-arms the debounce
    /// rather than being lost.
    pub(super) fn flush_snapshot(&mut self) {
        if self.snapshot_due.is_some_and(|due| Instant::now() < due) {
            return;
        }
        self.snapshot_due = None;
        if !self.snapshot_dirty {
            return;
        }
        if self.snapshot_writing {
            self.snapshot_due = Some(Instant::now() + SNAPSHOT_WRITE_RETRY);
            return;
        }
        let Some(paths) = self.snapshot_paths.clone() else {
            self.snapshot_dirty = false;
            return;
        };
        let Some((snapshot, history)) = self.capture_session_snapshot() else {
            // A session with nothing saveable in it — every pane a plugin pane, say — has no shape
            // worth recording. Leave any previous snapshot alone rather than blanking it.
            self.snapshot_dirty = false;
            return;
        };
        let keep_history = self.config.session.pane_history;
        self.snapshot_dirty = false;
        self.snapshot_writing = true;
        let sender = self.sender.clone();
        let spawn = std::thread::Builder::new()
            .name("vvmux-session-snapshot".into())
            .spawn(move || {
                // Both files in one pass, so shape and history always describe the same generation.
                let result = crate::session_state::save_snapshot(&paths.snapshot, &snapshot)
                    .and_then(|()| write_history(&paths.history, keep_history, &history));
                let _ = sender.send(ActorEvent::SnapshotWritten {
                    result: result.map_err(|error| error.to_string()),
                });
            });
        if spawn.is_err() {
            self.snapshot_writing = false;
            self.snapshot_dirty = true;
        }
    }

    /// The live session as a persistable snapshot, or `None` when it has no saveable shape.
    pub(super) fn capture_session_snapshot(&self) -> Option<(SessionSnapshot, SessionHistory)> {
        let mut capture = SnapshotCapture::default();
        let layout = self.capture_layout_with_extras(Some(&mut capture)).ok()?;
        let history = if self.config.session.pane_history {
            self.capture_session_history(&capture)
        } else {
            SessionHistory::default()
        };
        Some((SessionSnapshot::new(layout, capture.extras), history))
    }

    /// Every pane's scrollback, keyed by the same slots the shape snapshot uses.
    ///
    /// Driven by the capture's own slot map rather than by a second walk, so history can never
    /// describe a pane the shape does not.
    pub(super) fn capture_session_history(&self, capture: &SnapshotCapture) -> SessionHistory {
        let mut remaining = HISTORY_MAX_SESSION_BYTES;
        let tabs = capture
            .slots
            .iter()
            .map(|slots| TabHistory {
                panes: slots
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, pane_id)| {
                        let history = self.capture_pane_history(slot, *pane_id)?;
                        // Spent in slot order, so a session of many panes keeps whole panes rather
                        // than a fragment of each.
                        let cost: usize = history
                            .rows
                            .iter()
                            .flat_map(|row| row.runs.iter())
                            .map(|run| run.text.len().saturating_add(8))
                            .sum();
                        remaining = remaining.checked_sub(cost.max(1))?;
                        Some(history)
                    })
                    .collect(),
            })
            .collect();
        SessionHistory::new(tabs)
    }

    /// Write the current shape before the session goes away, without a worker to wait on.
    ///
    /// The debounce exists so a busy session does not write constantly; it must not be the reason a
    /// clean shutdown loses the last few seconds of shape.
    pub(super) fn write_snapshot_now(&mut self) {
        let Some(paths) = self.snapshot_paths.clone() else {
            return;
        };
        let Some((snapshot, history)) = self.capture_session_snapshot() else {
            return;
        };
        let result =
            crate::session_state::save_snapshot(&paths.snapshot, &snapshot).and_then(|()| {
                write_history(&paths.history, self.config.session.pane_history, &history)
            });
        if let Err(error) = result {
            log::warn!(
                event = "session.snapshot.write.failure",
                error:% = error;
                "could not write the session snapshot"
            );
        }
    }

    /// Describe the live session in the startup-layout schema.
    ///
    /// Only core shell panes are captured: plugin panes are host-owned and must never be revived
    /// as shells, and zoom and synchronized input are projection/tab state rather than layout.
    /// Weights are rescaled into the parser's accepted range while preserving their ratio.
    pub(super) fn capture_layout(&self) -> io::Result<LayoutFile> {
        self.capture_layout_with_extras(None)
    }

    /// Capture the live session's shape, and optionally everything a layout file cannot describe.
    ///
    /// One traversal produces both, because the two are keyed to each other: `lower` assigns a pane
    /// slot per label in the order this walk emits them, so extras keyed by slot are only correct
    /// while they are collected by the same walk that assigns the labels. Building them in a second
    /// pass would leave two orderings to keep in agreement, and the skipped-tab rule below is
    /// exactly the kind of thing that would drift.
    pub(super) fn capture_layout_with_extras(
        &self,
        mut extras: Option<&mut SnapshotCapture>,
    ) -> io::Result<LayoutFile> {
        let area = self.content_area();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut tabs = Vec::new();
        let mut total_panes = 0_usize;
        for (tab_index, tab) in self.tabs.iter().enumerate() {
            let mut labels: Vec<(PaneId, String)> = Vec::new();
            let tiled = tab
                .tree
                .as_ref()
                .and_then(|tree| self.capture_node(tree, home.as_deref(), &mut labels));
            let mut floating = Vec::new();
            let mut float_extras = Vec::new();
            for float in tab.floating.panes() {
                let Some(pane) = self.core_pane(float.pane_id) else {
                    continue;
                };
                let label = format!("p{}", labels.len() + 1);
                // A float that still carries its birth geometry is recorded exactly as it was
                // born: the origin percents are the truth, while measuring the rectangle can
                // only reproduce them through placeholder-area rounding. A float a user placed
                // by hand is measured, as before.
                let origin = float.origin;
                float_extras.push(FloatExtras {
                    slot: labels.len(),
                    // A layout file records a float's size but not where it sat, because a
                    // hand-written layout should not have to place windows. A snapshot describes a
                    // session that existed, so it records both.
                    x_percent: origin
                        .and_then(|origin| origin.x_percent)
                        .unwrap_or_else(|| saved_position_percent(float.rect.x, area.width)),
                    y_percent: origin
                        .and_then(|origin| origin.y_percent)
                        .unwrap_or_else(|| saved_position_percent(float.rect.y, area.height)),
                });
                labels.push((float.pane_id, label.clone()));
                floating.push(LayoutFloat::new(
                    label,
                    saved_cwd(&pane.spawn_cwd, home.as_deref()),
                    origin.map_or_else(
                        || saved_percent(float.rect.width, area.width),
                        |origin| origin.width_percent,
                    ),
                    origin.map_or_else(
                        || saved_percent(float.rect.height, area.height),
                        |origin| origin.height_percent,
                    ),
                    float.pinned,
                    !pane.transparent,
                ));
            }
            if tiled.is_none() && floating.is_empty() {
                // Skipped here and skipped in the extras below, so the two stay index-aligned.
                continue;
            }
            if let Some(capture) = extras.as_deref_mut() {
                if tab_index == self.active_tab {
                    capture.extras.active_tab = capture.extras.tabs.len();
                }
                // The slot-to-pane mapping is only knowable here, and only for as long as this walk
                // runs: slots are positions in `labels`, and nothing persisted records a pane ID.
                capture
                    .slots
                    .push(labels.iter().map(|(pane_id, _)| *pane_id).collect());
                capture.extras.tabs.push(TabExtras {
                    zoomed: tab.zoomed.and_then(|zoomed| {
                        labels.iter().position(|(pane_id, _)| *pane_id == zoomed)
                    }),
                    sync_input: tab.sync_input,
                    floats: float_extras,
                    panes: labels
                        .iter()
                        .enumerate()
                        .map(|(slot, (pane_id, _))| self.capture_pane_extras(slot, *pane_id))
                        .collect(),
                });
            }
            let focus = labels
                .iter()
                .find(|(pane_id, _)| *pane_id == tab.focused)
                .map(|(_, label)| label.clone());
            total_panes = total_panes.saturating_add(labels.len());
            if total_panes > MAX_LAYOUT_PANES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a saved layout holds at most {MAX_LAYOUT_PANES} panes"),
                ));
            }
            tabs.push(LayoutTab::new(tab.name.clone(), focus, tiled, floating));
            if tabs.len() > MAX_LAYOUT_TABS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a saved layout holds at most {MAX_LAYOUT_TABS} tabs"),
                ));
            }
        }
        if tabs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "this session has no shell panes to save",
            ));
        }
        Ok(LayoutFile::from_tabs(tabs))
    }

    /// Arm the panes whose agents can reopen the conversation they had.
    ///
    /// Arms rather than launches: a restored pane is sitting at the placeholder geometry with no
    /// client watching, and a full-screen agent started there would paint itself for a terminal
    /// nobody is looking at and then have to be told the real size. The launch happens on attach.
    ///
    /// Everything here is re-validated rather than trusted. A snapshot is a file, so the agent kind,
    /// the alias, and the session reference are all reparsed, and the reporting source is checked
    /// against the provider that owns the kind — the resume becomes a command line on the user's
    /// machine, so a reference from anywhere else is not one this may act on.
    /// Give restored panes back the names they had.
    ///
    /// This is the whole point of a pane name: the restore reassigned every pane ID, so a caller
    /// holding one is now addressing whichever pane inherited that number. A name comes back
    /// attached to the pane it was given to.
    ///
    /// A name from the file is re-parsed rather than trusted, and a duplicate is dropped rather
    /// than allowed to shadow the pane that already took it — the snapshot cannot have contained
    /// one, but it is file content and this is the only place that can still say no.
    pub(super) fn restore_pane_names(
        &mut self,
        tab_extras: &TabExtras,
        slot_ids: &[PaneId],
        failed: &HashSet<PaneId>,
    ) {
        for pane_extras in &tab_extras.panes {
            let Some(pane_id) = slot_ids.get(pane_extras.slot).copied() else {
                continue;
            };
            if failed.contains(&pane_id) {
                continue;
            }
            let Some(name) = pane_extras
                .name
                .as_deref()
                .and_then(|name| crate::layout::PaneName::new(name).ok())
            else {
                continue;
            };
            if self.pane_with_name(&name).is_some() {
                continue;
            }
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.name = Some(name);
            }
        }
    }

    /// The pane holding `name`, if one does.
    ///
    /// A linear scan for the same reason [`Self::pane_with_agent_alias`] uses one: a session holds
    /// at most [`MAX_SESSION_PANES`] panes, and an index would be a second copy
    /// of state to invalidate everywhere a name is cleared.
    pub(super) fn pane_with_name(&self, name: &crate::layout::PaneName) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|(_, pane)| pane.name.as_ref() == Some(name))
            .map(|(pane_id, _)| *pane_id)
    }

    pub(super) fn arm_pane_resumes(
        &mut self,
        tab_extras: &TabExtras,
        slot_ids: &[PaneId],
        failed: &HashSet<PaneId>,
        resumed: &mut HashSet<String>,
    ) {
        if !self.config.session.resume_agents {
            return;
        }
        for pane_extras in &tab_extras.panes {
            let Some(pane_id) = slot_ids.get(pane_extras.slot).copied() else {
                continue;
            };
            if failed.contains(&pane_id) {
                continue;
            }
            let Some(agent) = pane_extras.agent.as_ref() else {
                continue;
            };
            let Some(plan) = resume_plan(agent) else {
                continue;
            };
            // Reserved before arming, so the second pane naming one conversation restores as the
            // plain shell it will stay rather than racing the first to reopen it.
            if !resumed.insert(plan.dedupe_key.clone()) {
                continue;
            }
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.pending_resume = Some(plan);
            }
        }
    }

    /// Give a resumed agent back the name it had, once it is actually running.
    ///
    /// Waits for detection rather than racing it: the resume is typed at a shell, and the agent's
    /// own process appears afterwards. Uniqueness is checked here rather than when the name was
    /// recorded, because the session it is rejoining is not the one it left — another pane may
    /// already hold the name.
    pub(super) fn adopt_pending_aliases(&mut self) {
        let ready = self
            .panes
            .iter()
            .filter(|(_, pane)| pane.pending_alias.is_some() && pane.agent.snapshot().is_some())
            .map(|(pane_id, _)| *pane_id)
            .collect::<Vec<_>>();
        for pane_id in ready {
            let Some(alias) = self
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| pane.pending_alias.take())
            else {
                continue;
            };
            if self.pane_with_agent_alias(&alias).is_some() {
                continue;
            }
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.agent.adopt_alias(alias);
            }
        }
    }

    /// The shell a restored pane is sitting at, when it is sitting at one.
    ///
    /// Cheap by construction rather than by approximation. The foreground process group equalling
    /// the pane's own child means nothing has taken the terminal from the shell, which is a
    /// `tcgetpgrp` call; the shell's *name* needs no lookup at all, because this session chose it
    /// when it spawned the pane. Refuses a shell this build cannot quote for, exactly as
    /// `agent-start` does.
    pub(super) fn restored_pane_shell(&self, pane_id: PaneId) -> Option<String> {
        let pane = self.panes.get(&pane_id)?;
        if pane.child_pid == 0
            || pane
                .control
                .foreground_process_group_id()
                .is_some_and(|group| group != pane.child_pid)
        {
            return None;
        }
        let shell = self
            .config
            .general
            .shell
            .as_ref()
            .map(|path| OsString::from(path.as_os_str()))
            .or_else(default_shell)
            .unwrap_or_else(fallback_shell);
        let name = Path::new(&shell)
            .file_name()?
            .to_string_lossy()
            .into_owned();
        crate::agent_drive::is_pane_shell(&name).then_some(name)
    }

    /// Run every armed resume, now that a client has supplied real geometry.
    ///
    /// One attempt each, success or failure: a pane whose shell is busy, or whose write fails, is
    /// left as the working shell it already is rather than retried into an unclear state. Nothing is
    /// waiting on a reply, so a failure is reported to nobody and costs only the resume.
    pub(super) fn fire_pending_resumes(&mut self, only: Option<PaneId>) {
        if !self.config.session.resume_agents {
            return;
        }
        let armed = self
            .panes
            .iter()
            .filter(|(pane_id, pane)| {
                pane.pending_resume.is_some() && only.is_none_or(|only| **pane_id == only)
            })
            .map(|(pane_id, _)| *pane_id)
            .collect::<Vec<_>>();
        for pane_id in armed {
            let Some(plan) = self
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| pane.pending_resume.take())
            else {
                continue;
            };
            // The pane's own shell has to be what is in the foreground, or the resume would be
            // typed into whatever is. `agent-start` answers this by scanning the process table on a
            // worker, because it cannot know what a user's pane is running. Restore can: this pane
            // was spawned by this session moments ago with a shell this session chose, so the
            // foreground-group check alone settles it, and the actor never touches `/proc`.
            // Resolved now, against the catalog that actually applies, and refused if the
            // provider no longer declares a resume or the reporting source does not own the kind.
            let Some(argv) =
                self.agent_catalog
                    .resume_argv(&plan.kind, &plan.source, &plan.session)
            else {
                continue;
            };
            let Some(shell) = self.restored_pane_shell(pane_id) else {
                continue;
            };
            if let Err(error) = self.type_agent_command(pane_id, &argv, &shell) {
                log::warn!(
                    event = "agent.resume.failure",
                    agent:% = plan.kind,
                    pane = pane_id,
                    error = error.message.as_str();
                    "could not resume an agent"
                );
                continue;
            }
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.pending_alias = plan.alias;
            }
        }
    }

    /// One pane's scrollback, bounded and stripped to text and style.
    ///
    /// Bounded twice on purpose: by lines, so a pane that scrolled a gigabyte contributes what a
    /// person would scroll back through, and by bytes, so a pane of very wide lines cannot reach the
    /// same total. herdr serializes the entire scrollback of every pane with no cap at all; that is
    /// the one part of its persistence not worth copying.
    pub(super) fn capture_pane_history(&self, slot: usize, pane_id: PaneId) -> Option<PaneHistory> {
        let pane = self.panes.get(&pane_id)?;
        let available = pane.terminal.history_len();
        let rows = pane.terminal.history_tail(HISTORY_MAX_ROWS);
        let mut styles: Vec<HistoryStyle> = Vec::new();
        let mut style_ids: HashMap<HistoryStyle, usize> = HashMap::new();
        let mut captured = Vec::new();
        let mut bytes = 0_usize;
        // Newest first, so the byte budget drops the oldest lines rather than the ones a person is
        // most likely to want.
        for (cells, wrapped) in rows.iter().rev() {
            let mut runs: Vec<HistoryRun> = Vec::new();
            for cell in *cells {
                if cell.wide_continuation || cell.leading_wide_spacer {
                    continue;
                }
                let style = history_style(cell);
                let next = *style_ids.entry(style.clone()).or_insert_with(|| {
                    styles.push(style);
                    styles.len() - 1
                });
                let text = if cell.tab_width.is_some() {
                    "\t".to_owned()
                } else {
                    let mut text = cell.ch.to_string();
                    text.push_str(&cell.combining);
                    text
                };
                match runs.last_mut() {
                    Some(run) if run.style == next => run.text.push_str(&text),
                    _ => runs.push(HistoryRun { style: next, text }),
                }
            }
            // Trailing blanks are most of a terminal line and carry nothing.
            while runs
                .last()
                .is_some_and(|run| run.text.trim_end_matches(' ').is_empty())
            {
                runs.pop();
            }
            if let Some(run) = runs.last_mut() {
                let trimmed = run.text.trim_end_matches(' ').len();
                run.text.truncate(trimmed);
            }
            bytes = bytes.saturating_add(
                runs.iter()
                    .map(|run| run.text.len().saturating_add(8))
                    .sum::<usize>(),
            );
            if bytes > HISTORY_MAX_PANE_BYTES {
                break;
            }
            captured.push(HistoryRow {
                wrapped: *wrapped,
                runs,
            });
        }
        captured.reverse();
        if captured.iter().all(|row| row.runs.is_empty()) {
            return None;
        }
        Some(PaneHistory {
            slot,
            truncated: captured.len() < available,
            styles,
            rows: captured,
        })
    }

    /// What a snapshot records about one pane beyond its place in the layout.
    pub(super) fn capture_pane_extras(&self, slot: usize, pane_id: PaneId) -> PaneExtras {
        let agent = self.panes.get(&pane_id).and_then(|pane| {
            let snapshot = pane.agent.snapshot();
            let alias = pane.agent.alias();
            let session = pane.agent.session();
            // Nothing worth recording about an agent that has none of the three.
            (snapshot.is_some() || alias.is_some()).then(|| PaneAgentExtras {
                alias: alias.map(ToString::to_string),
                kind: snapshot.map(|snapshot| snapshot.kind.to_string()),
                session_source: pane.agent.session_source().map(ToOwned::to_owned),
                session_id: session
                    .and_then(|session| session.id())
                    .map(ToOwned::to_owned),
                session_path: session
                    .and_then(|session| session.path())
                    .map(ToOwned::to_owned),
            })
        });
        PaneExtras {
            slot,
            title: self
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.terminal.title())
                .map(ToOwned::to_owned),
            name: self
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.name.as_ref())
                .map(ToString::to_string),
            agent,
        }
    }

    /// Capture one tiled subtree, collapsing away branches whose panes are not saveable so the
    /// surviving siblings keep their own shape.
    pub(super) fn capture_node(
        &self,
        node: &TiledNode,
        home: Option<&Path>,
        labels: &mut Vec<(PaneId, String)>,
    ) -> Option<LayoutNode> {
        match node {
            TiledNode::Leaf(pane_id) => {
                let pane = self.core_pane(*pane_id)?;
                let label = format!("p{}", labels.len() + 1);
                labels.push((*pane_id, label.clone()));
                Some(LayoutNode::leaf(
                    label,
                    saved_cwd(&pane.spawn_cwd, home),
                    !pane.transparent,
                ))
            }
            TiledNode::Split {
                axis,
                first,
                second,
                first_weight,
                second_weight,
            } => {
                let captured_first = self.capture_node(first, home, labels);
                let captured_second = self.capture_node(second, home, labels);
                match (captured_first, captured_second) {
                    (Some(first), Some(second)) => Some(LayoutNode::split(
                        *axis,
                        saved_sizes(*first_weight, *second_weight),
                        vec![first, second],
                    )),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }

    pub(super) fn core_pane(&self, pane_id: PaneId) -> Option<&Pane> {
        self.panes
            .get(&pane_id)
            .filter(|pane| matches!(pane.role, PaneRole::Core))
    }
}

/// A cell's appearance, without anything that would act if it came back.
///
/// Notably absent: the hyperlink. It round-trips fine, but restoring it would make a URL from a
/// previous session clickable in this one, and the URI came from whatever was running in that pane.
/// Write or remove the pane-history file, following the setting.
///
/// Removed rather than left alone when the setting is off: it holds whatever scrolled past, so
/// turning the setting off has to take the data with it, not merely stop adding to it.
fn write_history(path: &Path, enabled: bool, history: &SessionHistory) -> io::Result<()> {
    if enabled {
        crate::session_state::save_history(path, history)
    } else {
        crate::session_state::clear(path)
    }
}

fn history_style(cell: &Cell) -> HistoryStyle {
    HistoryStyle {
        fg: history_color(cell.foreground),
        bg: history_color(cell.background),
        bold: cell.bold,
        dim: cell.dim,
        italic: cell.italic,
        underline: match cell.underline_style {
            UnderlineStyle::None => 0,
            UnderlineStyle::Single => 1,
            UnderlineStyle::Double => 2,
            UnderlineStyle::Curl => 3,
            UnderlineStyle::Dotted => 4,
            UnderlineStyle::Dashed => 5,
        },
        blink: cell.blink,
        inverse: cell.inverse,
        hidden: cell.hidden,
        strikeout: cell.strikeout,
    }
}

/// A pane's directory as a saved layout should spell it: `~/`-relative when it is under `$HOME`,
/// so the file stays portable, and omitted when the path is not valid UTF-8.
fn saved_cwd(cwd: &Path, home: Option<&Path>) -> Option<String> {
    if let Some(home) = home
        && let Ok(rest) = cwd.strip_prefix(home)
        && !rest.as_os_str().is_empty()
    {
        return rest.to_str().map(|rest| format!("~/{rest}"));
    }
    cwd.to_str().map(str::to_owned)
}

/// Rescale a live split's weights into the 1..=1000 the layout parser accepts, preserving ratio.
fn saved_sizes(first: u32, second: u32) -> Vec<u32> {
    let total = u64::from(first) + u64::from(second);
    if total == 0 {
        return vec![1, 1];
    }
    let scaled = ((u64::from(first) * 1000) / total).clamp(1, 999) as u32;
    vec![scaled, 1000 - scaled]
}

/// A float's size as a percentage of the content area, inside the parser's accepted range.
/// Where a float sat, as a percentage of the host area.
///
/// Unlike [`saved_percent`], which describes an extent and so has a 10% floor, a position of zero is
/// an ordinary answer: a float flush against the left edge is where the user put it. Its inverse
/// lives in [`crate::layout`] as `percent_of`: restoring a recorded position into a live area is
/// the same percent-to-cells math float placement uses.
fn saved_position_percent(offset: u16, available: u16) -> u16 {
    if available == 0 {
        return 0;
    }
    ((u32::from(offset) * 100) / u32::from(available)).min(100) as u16
}

/// Turn one pane's recorded agent into a resume plan, or nothing if it cannot be trusted.
fn resume_plan(agent: &PaneAgentExtras) -> Option<AgentResumePlan> {
    let kind = crate::agent::AgentId::new(agent.kind.clone()?).ok()?;
    let source = agent.session_source.clone()?;
    let session =
        crate::agent::AgentSessionRef::new(agent.session_id.clone(), agent.session_path.clone())?;
    session.validate().ok()?;
    Some(AgentResumePlan {
        // Reparsed rather than carried: an alias that no longer fits the grammar is dropped
        // rather than becoming a target nothing else could have created.
        alias: agent
            .alias
            .as_deref()
            .and_then(|alias| crate::agent::AgentAlias::new(alias).ok()),
        dedupe_key: format!(
            "{source}\0{kind}\0{}\0{}",
            session.id().unwrap_or_default(),
            session.path().unwrap_or_default()
        ),
        kind,
        source,
        session,
    })
}
