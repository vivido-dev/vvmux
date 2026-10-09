//! Automation waiters, pane descriptions, and grid snapshots.

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
    pub(super) fn pane_description(
        &self,
        pane_id: PaneId,
        disclosure: AgentDisclosure,
    ) -> Option<serde_json::Value> {
        let pane = self.panes.get(&pane_id)?;
        let tab_index = self.tabs.iter().position(|tab| tab.contains(pane_id))?;
        let tab = &self.tabs[tab_index];
        let area = self.content_area();
        let tiled_geometry = tab.tree.as_ref().and_then(|tree| {
            tree.geometry(area)
                .into_iter()
                .find(|(pane, _)| *pane == pane_id)
        });
        let floating = tab.floating.get(pane_id);
        let (layer, outer) = if let Some(floating) = floating {
            (
                if floating.pinned {
                    "pinned"
                } else {
                    "floating"
                },
                floating.rect,
            )
        } else {
            (
                "tiled",
                tiled_geometry.map_or(Rect::default(), |(_, rect)| rect),
            )
        };
        let visible = self.pane_is_visibly_present(pane_id);
        let cursor = pane.terminal.cursor();
        let plugin = match &pane.role {
            PaneRole::Core => None,
            PaneRole::Plugin(owner) => Some(serde_json::json!({
                "plugin_id": owner.plugin_id,
                "plugin_instance": owner.plugin_instance,
                "package_digest": owner.package_digest,
                "entrypoint_id": owner.entrypoint_id,
                "accept_sync_input": owner.accept_sync_input,
            })),
        };
        let title = pane.terminal.title().or(match &pane.role {
            PaneRole::Plugin(owner) => Some(owner.title.as_str()),
            PaneRole::Core => None,
        });
        let projections = Self::topology_projections(tab, area);
        let neighbors = [
            ("left", Direction::Left),
            ("right", Direction::Right),
            ("up", Direction::Up),
            ("down", Direction::Down),
        ]
        .into_iter()
        .map(|(name, direction)| {
            (
                name.to_owned(),
                serde_json::to_value(directional_focus(&projections, pane_id, direction))
                    .unwrap_or(serde_json::Value::Null),
            )
        })
        .collect::<serde_json::Map<_, _>>();
        Some(serde_json::json!({
            "pane_id": pane_id,
            "pane_name": pane.name.as_ref().map(ToString::to_string),
            "tab_id": tab.id,
            "tab_name": tab.name,
            "split_path": tab.tree.as_ref().and_then(|tree| tree.split_path(pane_id)),
            "neighbors": neighbors,
            "active_tab": tab_index == self.active_tab,
            "focused": self.attached_focus_pane() == Some(pane_id),
            "visible": visible,
            "layer": layer,
            "zoomed": tab.zoomed == Some(pane_id),
            "sync_input": tab.sync_input,
            "transparent": pane.transparent,
            "title": title,
            // What media this pane carries, so "which pane is the browser / the document / the
            // desktop, and can I capture it" is one call rather than one round trip per pane.
            // Omitted rather than emitted empty, like the agent metadata below: most panes carry
            // no media and a bulk listing should not pay an empty container per pane to say so.
            "media": Some(self.vivid.pane_media_summary(pane_id))
                .filter(|summary| !summary.is_empty())
                .map(|summary| serde_json::json!({
                    "surfaces": summary.surfaces,
                    // True when a capture would produce pixels right now. Strictly stronger than
                    // having a visual track: an encoded-video pane has one and can never be
                    // composed here, which is exactly the distinction a caller needs up front.
                    "capturable": summary.capturable(),
                    "tracks": summary
                        .tracks
                        .iter()
                        .map(|track| serde_json::json!({
                            "kind": track.kind,
                            "capturable": track.capturable,
                            "producer_id": track.source.producer,
                            "context_id": track.source.context,
                            "surface_id": track.source.surface,
                            "track_id": track.source.track,
                        }))
                        .collect::<Vec<_>>(),
                })),
            "plugin": plugin,
            "agent": pane.agent.snapshot().map(|snapshot| {
                let mut agent = agent_json(snapshot);
                // The transition counter travels with the state it counts, so a caller can read a
                // baseline and a status in one call and know they describe the same moment.
                agent["change_sequence"] = pane.agent_change_seq.into();
                // Omitted rather than emitted empty: most panes never carry metadata, and a bulk
                // listing should not pay four empty containers per pane to say so.
                if !pane.agent.metadata().is_empty() {
                    agent["metadata"] = agent_metadata_json(pane.agent.metadata());
                }
                // Always present, unlike metadata: an alias is a target a caller may need to
                // discover, so `null` is a useful answer where an absent key would not be.
                agent["alias"] = serde_json::to_value(pane.agent.alias()).unwrap_or_default();
                if disclosure == AgentDisclosure::Full
                    && let Some(session) = pane.agent.session()
                {
                    agent["agent_session"] = serde_json::json!({
                        "id": session.id(),
                        "path": session.path(),
                    });
                }
                agent
            }),
            // The kind alone, never the command: the argv embeds the session identity, and this
            // is a broad listing rather than the exact-pane disclosure that identity is confined to.
            "pending_resume": pane.pending_resume.as_ref().map(|plan| serde_json::json!({
                "agent": plan.kind,
                "armed": true,
            })),
            "geometry": rect_json(outer),
            "content_geometry": rect_json(outer.content()),
            "columns": pane.terminal.cols(),
            "rows": pane.terminal.rows(),
            "history_size": pane.terminal.history_len(),
            "display_offset": pane.copy.as_ref().map_or(0, |copy| copy.offset),
            "copy_mode": pane.copy.is_some(),
            "copy": pane.copy.as_ref().map(|copy| serde_json::json!({
                "offset": copy.offset,
                "row": copy.row,
                "column": copy.column,
                "search_query": copy.search.as_ref().map(|search| search.query.as_str()),
            })),
            "cursor": { "row": cursor.0, "column": cursor.1, "visible": pane.terminal.modes().cursor_visible },
            "modes": terminal_mode_names(pane.terminal.modes()),
            "screen": if pane.terminal.alternate_screen() { "alternate" } else { "primary" },
            "process_state": if pane.exit_status.is_some() { "exited" } else { "running" },
            "process": self.pane_process_json(pane),
            "output_offset": pane.transcript.offset,
            "retained_output_from_offset": pane.transcript.start(),
            "outer_crop": self.pane_outer_crop_json(pane_id),
            "screen_sequence": pane.screen_sequence,
            "session_sequence": self.session_sequence,
        }))
    }

    pub(super) fn grid_snapshot(
        &self,
        pane_id: PaneId,
        start_line: Option<isize>,
        row_count: Option<u16>,
        since_screen: Option<u64>,
    ) -> Result<serde_json::Value, AutomationError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        let display_offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
        let mut full = true;
        let mut gap = None;
        let mut viewport_rows = None;
        if let Some(since) = since_screen {
            if start_line.is_some() || row_count.is_some() {
                return Err(AutomationError::new(
                    "invalid_params",
                    "--since-screen conflicts with explicit line ranges",
                ));
            }
            if since > pane.screen_sequence {
                return Err(AutomationError::new(
                    "sequence_gap",
                    "requested screen sequence is newer than the pane",
                ));
            }
            if since == pane.screen_sequence {
                full = false;
                viewport_rows = Some(Vec::new());
            } else if display_offset > 0 {
                gap = Some(serde_json::json!({
                    "requested_sequence": since,
                    "oldest_sequence": pane.screen_changes.front().map(|change| change.sequence),
                    "current_sequence": pane.screen_sequence,
                    "reason": "copy_view",
                }));
            } else {
                let oldest = pane
                    .screen_changes
                    .front()
                    .map_or(pane.screen_sequence, |change| change.sequence);
                if since.saturating_add(1) < oldest {
                    gap = Some(serde_json::json!({
                        "requested_sequence": since,
                        "oldest_sequence": oldest,
                        "current_sequence": pane.screen_sequence,
                        "reason": "history_evicted",
                    }));
                } else {
                    let mut changed = std::collections::BTreeSet::new();
                    let mut invalidated = false;
                    for change in pane
                        .screen_changes
                        .iter()
                        .filter(|change| change.sequence > since)
                    {
                        match &change.rows {
                            Some(rows) => changed.extend(rows.iter().copied()),
                            None => invalidated = true,
                        }
                    }
                    if !invalidated {
                        full = false;
                        viewport_rows = Some(changed.into_iter().collect());
                    }
                }
            }
        }

        let (range_start, count) = if let (Some(start), Some(count)) = (start_line, row_count) {
            (start, usize::from(count))
        } else {
            (-(display_offset as isize), pane.terminal.rows())
        };
        let available_start = -(pane.terminal.history_len() as isize);
        let available_end = pane.terminal.rows() as isize;
        if range_start < available_start
            || range_start > available_end
            || range_start.saturating_add(count as isize) > available_end
        {
            return Err(AutomationError::new(
                "invalid_params",
                format!("grid range must be within {available_start}..{available_end}"),
            ));
        }
        let selected_rows = viewport_rows.unwrap_or_else(|| (0..count).collect());
        let mut estimated_bytes = 4096_usize;
        for row_index in selected_rows.iter().copied().filter(|row| *row < count) {
            let line = range_start + row_index as isize;
            let Some(cells) = pane.terminal.viewport_line(line) else {
                continue;
            };
            estimated_bytes = estimated_bytes
                .checked_add(pane.terminal.cols().saturating_mul(160))
                .ok_or_else(|| {
                    AutomationError::new("limit_exceeded", "grid reply size overflows")
                })?;
            for cell in cells.iter().take(pane.terminal.cols()) {
                estimated_bytes = estimated_bytes
                    .checked_add(cell.combining.len())
                    .and_then(|size| {
                        size.checked_add(cell.hyperlink.as_ref().map_or(0, |link| {
                            link.uri.len() + link.id.as_ref().map_or(0, String::len)
                        }))
                    })
                    .ok_or_else(|| {
                        AutomationError::new("limit_exceeded", "grid reply size overflows")
                    })?;
            }
            if estimated_bytes > AUTOMATION_REPLY_LIMIT {
                return Err(AutomationError::new(
                    "limit_exceeded",
                    "estimated grid reply exceeds 16 MiB; request fewer rows",
                ));
            }
        }
        let mut styles = Vec::<serde_json::Value>::new();
        let mut style_ids = HashMap::<StyleKey, usize>::new();
        let mut rows = Vec::new();
        let mut returned_lines = Vec::new();
        for row_index in selected_rows {
            if row_index >= count {
                continue;
            }
            let line = range_start + row_index as isize;
            let Some(source_cells) = pane.terminal.viewport_line(line) else {
                continue;
            };
            let mut cells = source_cells.to_vec();
            cells.resize(pane.terminal.cols(), Cell::default());
            cells.truncate(pane.terminal.cols());
            let serialized = cells
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    let key = StyleKey::from(cell);
                    let style_id = *style_ids.entry(key.clone()).or_insert_with(|| {
                        let id = styles.len();
                        styles.push(style_json(&key));
                        id
                    });
                    let width = if cell.wide_continuation || cell.leading_wide_spacer {
                        0
                    } else if cells
                        .get(column + 1)
                        .is_some_and(|next| next.wide_continuation)
                    {
                        2
                    } else {
                        1
                    };
                    let text = if cell.wide_continuation || cell.leading_wide_spacer {
                        String::new()
                    } else if cell.tab_width.is_some() {
                        "\t".into()
                    } else {
                        let mut text = cell.ch.to_string();
                        text.push_str(&cell.combining);
                        text
                    };
                    serde_json::json!({
                        "text": text,
                        "width": width,
                        "kind": if cell.wide_continuation { "continuation" } else if cell.leading_wide_spacer { "leading_wide_spacer" } else if cell.tab_width.is_some() { "tab" } else { "character" },
                        "tab_width": cell.tab_width,
                        "style": style_id,
                    })
                })
                .collect::<Vec<_>>();
            returned_lines.push(line);
            rows.push(serde_json::json!({
                "grid_line": line,
                "viewport_row": ((line + display_offset as isize) >= 0
                    && (line + display_offset as isize) < pane.terminal.rows() as isize)
                    .then_some(line + display_offset as isize),
                "wrapped": pane.terminal.line_wrapped(line).unwrap_or(false),
                "cells": serialized,
            }));
        }
        let cursor = pane.terminal.cursor();
        let selection = pane.copy.as_ref().map(|copy| serde_json::json!({
            "cursor": { "row": copy.row, "column": copy.column },
            "start": copy.selection_start.map(|(line, column)| serde_json::json!({ "line": line, "column": column })),
        }));
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "screen_sequence": pane.screen_sequence,
            "session_sequence": self.session_sequence,
            "full": full,
            "gap": gap,
            "grid": { "columns": pane.terminal.cols(), "rows": pane.terminal.rows() },
            "returned_lines": {
                "start": returned_lines.first(),
                "end": returned_lines.last(),
            },
            "history_size": pane.terminal.history_len(),
            "display_offset": display_offset,
            "screen": if pane.terminal.alternate_screen() { "alternate" } else { "primary" },
            "terminal_modes": terminal_mode_names(pane.terminal.modes()),
            "cursor": { "line": cursor.0, "column": cursor.1, "visible": pane.terminal.modes().cursor_visible },
            "selection": selection,
            "styles": styles,
            "rows": rows,
        }))
    }

    pub(super) fn add_automation_waiter(&mut self, waiter: AutomationWaiter) {
        if self.automation_waiters.len() >= MAX_AUTOMATION_WAITERS {
            self.reply_automation_error(
                waiter.reply,
                AutomationError::new("limit_exceeded", "too many automation waiters"),
            );
            return;
        }
        self.automation_waiters.push(waiter);
        self.check_automation_waiters();
    }

    pub(super) fn next_automation_deadline(&self) -> Duration {
        let now = Instant::now();
        self.automation_waiters
            .iter()
            .map(|waiter| {
                let stable_ready = match (&waiter.kind, waiter.pane_id) {
                    (
                        AutomationWaitKind::ScreenStable {
                            quiet,
                            ignore_bottom,
                            ..
                        },
                        Some(pane),
                    ) => self.panes.get(&pane).map(|pane| {
                        last_meaningful_change(
                            &pane.screen_changes,
                            pane.last_screen_change,
                            pane.terminal.rows(),
                            *ignore_bottom,
                        ) + *quiet
                    }),
                    // A launch is unanswerable until its settle window ends, so the actor has to
                    // wake then even if nothing else happens — otherwise an agent that starts
                    // silently is only noticed at the next unrelated wake.
                    (AutomationWaitKind::AgentLaunch { ready_after, .. }, _) => Some(*ready_after),
                    // A settling capture is answered by frames going quiet, which is the absence
                    // of an event. Without this the actor would sleep through the quiet window and
                    // only notice at the next unrelated wake.
                    (
                        AutomationWaitKind::CaptureMedia {
                            settling: Some(settling),
                            ..
                        },
                        _,
                    ) => {
                        Some((settling.quiet_since + CAPTURE_SETTLE_QUIET).min(settling.give_up_at))
                    }
                    (
                        AutomationWaitKind::AgentPrompt {
                            phase: AgentPromptPhase::Stall { stall_deadline, .. },
                            ..
                        },
                        _,
                    ) => Some(*stall_deadline),
                    _ => None,
                };
                stable_ready
                    .map_or(waiter.deadline, |ready| ready.min(waiter.deadline))
                    .saturating_duration_since(now)
            })
            .min()
            .unwrap_or(IDLE_WAKE_INTERVAL)
    }

    pub(super) fn check_automation_waiters(&mut self) {
        let now = Instant::now();
        let waiters = std::mem::take(&mut self.automation_waiters);
        for waiter in waiters {
            if waiter.deadline <= now {
                if let AutomationWaitKind::MediaTrace {
                    after_sequence,
                    limit,
                    filter,
                } = waiter.kind
                {
                    let result =
                        self.media_trace
                            .query(after_sequence, limit, waiter.pane_id, filter);
                    self.reply_automation(waiter.reply, serde_json::to_value(result).unwrap());
                } else {
                    // A scaled capture holds the pane at capture density. Drop it back before
                    // reporting the timeout, or an expired wait strands the pane resized.
                    if matches!(waiter.kind, AutomationWaitKind::CaptureMedia { .. })
                        && let Some(pane_id) = waiter.pane_id
                    {
                        self.finish_scaled_capture(pane_id);
                    }
                    let (code, message) = self.automation_timeout_error(&waiter);
                    self.reply_automation_error(waiter.reply, AutomationError::new(code, message));
                }
                continue;
            }
            // Writing the capture and dropping the pane back both need `&mut self`, so this one
            // cannot resolve through `automation_waiter_result`.
            if let AutomationWaitKind::CaptureMedia {
                path,
                baseline,
                settling,
            } = &waiter.kind
            {
                let (path, baseline) = (path.clone(), baseline.clone());
                let settling = settling.clone();
                let Some(pane_id) = waiter.pane_id else {
                    continue;
                };
                match self.poll_scaled_capture(pane_id, &path, &baseline, settling.as_ref(), now) {
                    ScaledCapturePoll::Done(result) => {
                        self.finish_scaled_capture(pane_id);
                        match result {
                            Ok(value) => self.reply_automation(waiter.reply, value),
                            Err(error) => self.reply_automation_error(waiter.reply, error),
                        }
                    }
                    ScaledCapturePoll::Pending(next) => {
                        let mut waiter = waiter;
                        if let AutomationWaitKind::CaptureMedia { settling, .. } = &mut waiter.kind
                            && let Some(next) = next
                        {
                            *settling = Some(next);
                        }
                        self.automation_waiters.push(waiter);
                    }
                }
                continue;
            }
            match self.automation_waiter_result(&waiter, now) {
                Some(Ok(result)) => self.reply_automation(waiter.reply, result),
                Some(Err(error)) => self.reply_automation_error(waiter.reply, error),
                None => self.automation_waiters.push(waiter),
            }
        }
    }

    /// Name an expired wait as specifically as the wait's own kind allows.
    ///
    /// An agent-state wait that never saw an agent is almost always the wrong pane rather than a
    /// slow agent — the diagnosis a registration-time rejection used to give, kept without making
    /// the common "launch, then wait" flow fail. A launch that expires is not a generic timeout at
    /// all: the caller asked for an agent to be running, and it is not.
    pub(super) fn automation_timeout_error(
        &self,
        waiter: &AutomationWaiter,
    ) -> (&'static str, String) {
        if let AutomationWaitKind::AgentLaunch { agent, .. } = &waiter.kind {
            return (
                "agent_start_failed",
                format!("`{agent}` did not start in this pane before the timeout"),
            );
        }
        let never_detected = matches!(waiter.kind, AutomationWaitKind::AgentState { .. })
            && waiter.pane_id.is_some_and(|pane_id| {
                self.panes
                    .get(&pane_id)
                    .is_none_or(|pane| pane.agent.snapshot().is_none())
            });
        if never_detected {
            (
                "timeout",
                "automation wait timed out; no agent was ever detected in this pane".to_owned(),
            )
        } else {
            ("timeout", "automation wait timed out".to_owned())
        }
    }

    pub(super) fn automation_waiter_result(
        &self,
        waiter: &AutomationWaiter,
        now: Instant,
    ) -> Option<Result<serde_json::Value, AutomationError>> {
        match &waiter.kind {
            AutomationWaitKind::Media {
                after_virtual_revision,
                after_outer_revision,
            } => {
                let pane_id = waiter.pane_id?;
                let status = self.vivid.pane_status(
                    pane_id,
                    self.outer_media_projection(),
                    self.relay_metrics(),
                );
                let virtual_ready = after_virtual_revision
                    .is_none_or(|revision| status.virtual_projection_revision > revision);
                let outer_ready = after_outer_revision
                    .is_none_or(|revision| status.outer_projection_revision > revision);
                (virtual_ready && outer_ready).then(|| {
                    serde_json::to_value(status).map_err(|error| {
                        AutomationError::new("serialization_failed", error.to_string())
                    })
                })
            }
            AutomationWaitKind::MediaTrace {
                after_sequence,
                limit,
                filter,
            } => {
                let result =
                    self.media_trace
                        .query(*after_sequence, *limit, waiter.pane_id, *filter);
                (result.gap.is_some() || !result.events.is_empty()).then(|| {
                    serde_json::to_value(result).map_err(|error| {
                        AutomationError::new("serialization_failed", error.to_string())
                    })
                })
            }
            AutomationWaitKind::Completion {
                level,
                after_outer,
                after_session,
                result,
            } => {
                let ready = match level {
                    AutomationCompletion::Outer => {
                        if self.presenter_client().is_none_or(|client| !client.vivid) {
                            return Some(Err(AutomationError::new(
                                "missing_attachment",
                                "foreground Vivid bridge detached while waiting",
                            )));
                        }
                        self.outer_projection_revision > *after_outer
                    }
                    AutomationCompletion::Rendered => {
                        let Some(client) = self.witness_client() else {
                            return Some(Err(AutomationError::new(
                                "missing_attachment",
                                "terminal client detached while waiting for render",
                            )));
                        };
                        client.rendered_session_sequence >= *after_session
                    }
                };
                ready.then(|| {
                    let mut result = result.clone();
                    result["completion"] = serde_json::json!({
                        "level": match level {
                            AutomationCompletion::Outer => "outer",
                            AutomationCompletion::Rendered => "rendered",
                        },
                        "outer_projection_revision": self.outer_projection_revision,
                        "rendered_session_sequence": self.rendered_session_sequence(),
                    });
                    Ok(result)
                })
            }
            AutomationWaitKind::MediaTrack {
                identity,
                condition,
            } => {
                let pane_id = waiter.pane_id?;
                let status = self.vivid.pane_status(
                    pane_id,
                    self.outer_media_projection(),
                    self.relay_metrics(),
                );
                let track = status.tracks.iter().find(|track| {
                    track.producer_id == identity.producer_id
                        && track.context_id == identity.context_id
                        && track.surface_id == identity.surface_id
                        && track.track_id == identity.track_id
                });
                let Some(track) = track else {
                    return Some(Err(AutomationError::new(
                        "track_not_found",
                        "media track does not exist in the requested pane",
                    )));
                };
                let clock_started = track.milestones & (1 << 6) != 0;
                let eos = track.milestones & (1 << 7) != 0;
                let random_access = track.milestones & (1 << 3) != 0;
                let matched = match condition {
                    MediaTrackWaitCondition::Visible => track.visible,
                    MediaTrackWaitCondition::Hidden => !track.visible,
                    MediaTrackWaitCondition::OuterAttached => {
                        track.outer_mapping_fresh && track.outer_channel_generation.is_some()
                    }
                    MediaTrackWaitCondition::KeyframeNeeded => track.keyframe_needed,
                    MediaTrackWaitCondition::KeyframeRecovered => {
                        !track.keyframe_needed && random_access
                    }
                    MediaTrackWaitCondition::Playing => track.lifecycle == "playing",
                    MediaTrackWaitCondition::Paused => track.lifecycle == "live" && clock_started,
                    MediaTrackWaitCondition::Eos => eos || track.lifecycle == "ended",
                    MediaTrackWaitCondition::Lost => track.lifecycle == "lost",
                    MediaTrackWaitCondition::QueueDrained => {
                        track.queued_packets == 0 && track.queued_bytes == 0
                    }
                };
                matched.then(|| {
                    serde_json::to_value(track).map_err(|error| {
                        AutomationError::new("serialization_failed", error.to_string())
                    })
                })
            }
            AutomationWaitKind::Rendered { after_session } => {
                let Some(client) = self.witness_client() else {
                    return Some(Err(AutomationError::new(
                        "unsupported",
                        "attached client disconnected while waiting for render",
                    )));
                };
                (client.rendered_session_sequence >= *after_session).then(|| {
                    Ok(serde_json::json!({
                        "session_sequence": self.session_sequence,
                        "rendered_session_sequence": client.rendered_session_sequence,
                    }))
                })
            }
            AutomationWaitKind::AgentState { until, initial } => {
                let pane_id = waiter.pane_id?;
                let Some(pane) = self.panes.get(&pane_id) else {
                    return Some(Err(AutomationError::new(
                        "pane_not_found",
                        "pane closed while waiting",
                    )));
                };
                // No agent is "not matching yet", the same before detection and after an agent
                // exits. One rule, bounded by the caller's timeout.
                let snapshot = pane.agent.snapshot()?;
                until.contains(&snapshot.status).then(|| {
                    Ok(serde_json::json!({
                        "pane_id": pane_id,
                        "status": snapshot.status,
                        "initial_status": initial,
                        "agent": agent_json(snapshot),
                        "session_sequence": self.session_sequence,
                    }))
                })
            }
            AutomationWaitKind::AgentLaunch {
                agent,
                argv,
                ready_after,
            } => {
                let pane_id = waiter.pane_id?;
                if !self.panes.contains_key(&pane_id) {
                    return Some(Err(AutomationError::new(
                        "pane_not_found",
                        "pane closed while the agent was starting",
                    )));
                }
                // A shell that exited took the launch with it — the command was typed but never
                // became an agent. Reporting it now beats waiting out the whole timeout.
                if self
                    .exit_tombstones
                    .iter()
                    .any(|exit| exit.pane_id == pane_id)
                {
                    return Some(Err(AutomationError::new(
                        "agent_start_failed",
                        format!("pane exited before `{agent}` started"),
                    )));
                }
                let snapshot = self.panes.get(&pane_id)?.agent.snapshot();
                // Detection identifies the process before classification settles, so a different
                // agent is knowable immediately and is never going to become the right one.
                if let Some(found) = &snapshot
                    && found.kind != *agent
                {
                    return Some(Err(AutomationError::new(
                        "agent_kind_mismatch",
                        format!("pane is running {} rather than `{agent}`", found.label),
                    )));
                }
                if now < *ready_after {
                    return None;
                }
                // Detection holds a newly identified agent at `idle` regardless of its screen, so
                // answering during that window would report a status classification has not
                // actually reached. The two graces are independent: this one starts when the
                // command is typed, detection's starts when it first sees the process, which is
                // necessarily later.
                if self
                    .panes
                    .get(&pane_id)
                    .is_some_and(|pane| pane.agent.status_is_provisional(now))
                {
                    return None;
                }
                // Ready means the agent is up and has settled somewhere a caller can act on.
                // `working` is excluded deliberately: an agent painting its first screen can look
                // busy, and answering then would report readiness the caller cannot yet use.
                let snapshot = snapshot?;
                matches!(snapshot.status, AgentStatus::Idle | AgentStatus::Blocked).then(|| {
                    Ok(serde_json::json!({
                        "pane_id": pane_id,
                        "status": snapshot.status,
                        "argv": argv,
                        "agent": agent_json(snapshot),
                        "session_sequence": self.session_sequence,
                    }))
                })
            }
            AutomationWaitKind::AgentPrompt {
                phase,
                until,
                baseline_status,
            } => {
                let pane_id = waiter.pane_id?;
                let Some(pane) = self.panes.get(&pane_id) else {
                    if self
                        .exit_tombstones
                        .iter()
                        .any(|exit| exit.pane_id == pane_id)
                    {
                        if baseline_status.is_some_and(|status| until.contains(&status)) {
                            return Some(Ok(serde_json::json!({
                                "pane_id": pane_id,
                                "status": baseline_status.unwrap(),
                                "session_sequence": self.session_sequence,
                            })));
                        }
                        return Some(Err(AutomationError::new(
                            "agent_not_running",
                            "agent exited while waiting for prompt",
                        )));
                    }
                    return Some(Err(AutomationError::new(
                        "pane_not_found",
                        "pane closed while waiting for prompt",
                    )));
                };
                let Some(snapshot) = pane.agent.snapshot() else {
                    return if self
                        .exit_tombstones
                        .iter()
                        .any(|exit| exit.pane_id == pane_id)
                    {
                        if let Some(status) = baseline_status {
                            if until.contains(status) {
                                Some(Ok(serde_json::json!({
                                    "pane_id": pane_id,
                                    "status": status,
                                    "session_sequence": self.session_sequence,
                                })))
                            } else {
                                Some(Err(AutomationError::new(
                                    "agent_not_running",
                                    "agent exited while waiting for prompt",
                                )))
                            }
                        } else {
                            Some(Err(AutomationError::new(
                                "agent_not_running",
                                "agent exited while waiting for prompt",
                            )))
                        }
                    } else {
                        Some(Err(AutomationError::new(
                            "agent_not_ready",
                            format!("{pane_id} no longer has a detected agent"),
                        )))
                    };
                };
                if self
                    .exit_tombstones
                    .iter()
                    .any(|exit| exit.pane_id == pane_id)
                    && !until.contains(&snapshot.status)
                {
                    return Some(Err(AutomationError::new(
                        "agent_not_running",
                        "agent exited while waiting for prompt",
                    )));
                }
                match phase {
                    AgentPromptPhase::Stall {
                        baseline_seq,
                        stall_deadline,
                    } => {
                        if pane.agent_change_seq > *baseline_seq {
                            if until.contains(&snapshot.status) {
                                return Some(Ok(serde_json::json!({
                                    "pane_id": pane_id,
                                    "status": snapshot.status,
                                    "agent": agent_json(snapshot),
                                    "session_sequence": self.session_sequence,
                                })));
                            }
                            None
                        } else if now >= *stall_deadline {
                            Some(Err(AutomationError::new(
                                "agent_prompt_stalled",
                                format!(
                                    "agent status remained {:?} with no transition after {}ms (change_sequence={})",
                                    baseline_status.unwrap_or(snapshot.status),
                                    crate::agent_drive::AGENT_PROMPT_EFFECT_TIMEOUT.as_millis(),
                                    baseline_seq,
                                ),
                            )))
                        } else {
                            None
                        }
                    }
                    AgentPromptPhase::Settle => until.contains(&snapshot.status).then(|| {
                        Ok(serde_json::json!({
                            "pane_id": pane_id,
                            "status": snapshot.status,
                            "agent": agent_json(snapshot),
                            "session_sequence": self.session_sequence,
                        }))
                    }),
                }
            }
            AutomationWaitKind::Exit => {
                let pane_id = waiter.pane_id?;
                self.exit_tombstones
                    .iter()
                    .rev()
                    .find(|exit| exit.pane_id == pane_id)
                    .map(|exit| Ok(exit_result(pane_id, exit.status)))
                    .or_else(|| {
                        (!self.panes.contains_key(&pane_id)).then(|| {
                            Err(AutomationError::new(
                                "pane_not_found",
                                "pane does not exist and has no retained exit status",
                            ))
                        })
                    })
            }
            kind => {
                let pane_id = waiter.pane_id?;
                let Some(pane) = self.panes.get(&pane_id) else {
                    return Some(Err(AutomationError::new(
                        "pane_not_found",
                        "pane closed while waiting",
                    )));
                };
                match kind {
                    AutomationWaitKind::Text {
                        pattern,
                        after_screen,
                    } => {
                        if after_screen.is_some_and(|sequence| pane.screen_sequence <= sequence) {
                            return None;
                        }
                        let text = pane
                            .terminal
                            .visible_text(pane.copy.as_ref().map_or(0, |copy| copy.offset));
                        let matched = match pattern {
                            AutomationTextPattern::Literal(pattern) => text.contains(pattern),
                            AutomationTextPattern::Regex(pattern) => pattern.is_match(&text),
                        };
                        matched.then(|| {
                            Ok(serde_json::json!({
                                "pane_id": pane_id,
                                "screen_sequence": pane.screen_sequence,
                            }))
                        })
                    }
                    AutomationWaitKind::Output {
                        pattern,
                        after_offset,
                    } => {
                        let (bytes, gap) = pane.transcript.since(*after_offset);
                        if gap {
                            return Some(Err(AutomationError::new(
                                "sequence_gap",
                                format!(
                                    "output before offset {} has already been dropped",
                                    pane.transcript.start()
                                ),
                            )));
                        }
                        // Lossy rather than refused: a pane's output is bytes, and a multi-byte
                        // character split across two reads must not make a wait fail.
                        let text = String::from_utf8_lossy(&bytes);
                        let matched = match pattern {
                            AutomationTextPattern::Literal(pattern) => text.contains(pattern),
                            AutomationTextPattern::Regex(pattern) => pattern.is_match(&text),
                        };
                        matched.then(|| {
                            Ok(serde_json::json!({
                                "pane_id": pane_id,
                                "output_offset": pane.transcript.offset,
                                "screen_sequence": pane.screen_sequence,
                            }))
                        })
                    }
                    AutomationWaitKind::ShellCommand {
                        after_command_id,
                        started_screen,
                    } => {
                        let shell = pane.terminal.shell_integration();
                        (shell.completed_command_id > *after_command_id).then(|| {
                            Ok(serde_json::json!({
                                "pane_id": pane_id,
                                "command_id": shell.completed_command_id,
                                // Absent when the shell reported a boundary but no status, which
                                // is a different answer from "it exited zero".
                                "exit_code": shell.exit_code,
                                "cwd": pane_cwd(pane.child_pid),
                                "started_screen_sequence": started_screen,
                                "completed_screen_sequence": pane.screen_sequence,
                                "output_offset": pane.transcript.offset,
                            }))
                        })
                    }
                    AutomationWaitKind::Capture {
                        after_screen,
                        quiet,
                        rendered_after_session,
                        grid,
                    } => {
                        if after_screen.is_some_and(|sequence| pane.screen_sequence <= sequence) {
                            return None;
                        }
                        if let Some(quiet) = quiet
                            && now.saturating_duration_since(pane.last_screen_change) < *quiet
                        {
                            return None;
                        }
                        if let Some(after_session) = rendered_after_session {
                            let Some(client) = self.witness_client() else {
                                return Some(Err(AutomationError::new(
                                    "unsupported",
                                    "no attached client can acknowledge a render",
                                )));
                            };
                            if client.rendered_session_sequence < *after_session {
                                return None;
                            }
                        }
                        Some(self.capture_payload(pane_id, *grid))
                    }
                    AutomationWaitKind::ScreenChange { after_screen } => {
                        (pane.screen_sequence > *after_screen).then(|| {
                            Ok(serde_json::json!({
                                "pane_id": pane_id,
                                "screen_sequence": pane.screen_sequence,
                            }))
                        })
                    }
                    AutomationWaitKind::ScreenStable {
                        quiet,
                        ignore_bottom,
                        after_screen,
                    } => {
                        let newer =
                            after_screen.is_none_or(|sequence| pane.screen_sequence > sequence);
                        let since = now.saturating_duration_since(last_meaningful_change(
                            &pane.screen_changes,
                            pane.last_screen_change,
                            pane.terminal.rows(),
                            *ignore_bottom,
                        ));
                        (newer && since >= *quiet).then(|| {
                            Ok(serde_json::json!({
                                "pane_id": pane_id,
                                "screen_sequence": pane.screen_sequence,
                                "quiet_ms": quiet.as_millis(),
                                "ignore_bottom": ignore_bottom,
                            }))
                        })
                    }
                    _ => None,
                }
            }
        }
    }

    pub(super) fn complete_exit_waiters(&mut self, pane_id: PaneId, status: Option<PtyExitStatus>) {
        let waiters = std::mem::take(&mut self.automation_waiters);
        for waiter in waiters {
            if waiter.pane_id == Some(pane_id) && matches!(waiter.kind, AutomationWaitKind::Exit) {
                self.reply_automation(waiter.reply, exit_result(pane_id, status));
            } else {
                self.automation_waiters.push(waiter);
            }
        }
    }

    pub(super) fn rendered_session_sequence(&self) -> u64 {
        self.witness_client()
            .map_or(0, |client| client.rendered_session_sequence)
    }

    pub(super) fn mark_pane_screen_change(&mut self, pane_id: PaneId, rows: Option<Vec<usize>>) {
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        pane.screen_sequence = pane.screen_sequence.wrapping_add(1);
        pane.last_screen_change = Instant::now();
        pane.screen_changes.push_back(ScreenChange {
            sequence: pane.screen_sequence,
            rows,
            at: pane.last_screen_change,
        });
        while pane.screen_changes.len() > SCREEN_CHANGE_HISTORY {
            pane.screen_changes.pop_front();
        }
        let screen_sequence = pane.screen_sequence;
        self.session_sequence = self.session_sequence.wrapping_add(1);
        self.queue_plugin_state_event(
            "pane.screen_changed",
            pane_id.to_string(),
            serde_json::json!({
                "pane_id": pane_id,
                "screen_sequence": screen_sequence,
            }),
            Some(pane_id),
        );
    }
}

fn terminal_mode_names(modes: TerminalModes) -> Vec<&'static str> {
    let mut names = Vec::new();
    if modes.application_cursor {
        names.push("application_cursor");
    }
    if modes.application_keypad {
        names.push("application_keypad");
    }
    if modes.bracketed_paste {
        names.push("bracketed_paste");
    }
    if modes.mouse_clicks {
        names.push("mouse_clicks");
    }
    if modes.mouse_motion {
        names.push("mouse_motion");
    }
    if modes.sgr_mouse {
        names.push("sgr_mouse");
    }
    if modes.sgr_pixels {
        names.push("sgr_pixels");
    }
    if modes.keyboard_flags != 0 {
        names.push("kitty_keyboard");
    }
    if modes.focus_reporting {
        names.push("focus_reporting");
    }
    if modes.cursor_visible {
        names.push("cursor_visible");
    }
    names
}

fn style_json(style: &StyleKey) -> serde_json::Value {
    let mut attributes = Vec::new();
    if style.bold {
        attributes.push("bold");
    }
    if style.dim {
        attributes.push("dim");
    }
    if style.italic {
        attributes.push("italic");
    }
    match style.underline {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => attributes.push("underline"),
        UnderlineStyle::Double => attributes.push("double_underline"),
        UnderlineStyle::Curl => attributes.push("undercurl"),
        UnderlineStyle::Dotted => attributes.push("dotted_underline"),
        UnderlineStyle::Dashed => attributes.push("dashed_underline"),
    }
    if style.blink {
        attributes.push("blink");
    }
    if style.inverse {
        attributes.push("inverse");
    }
    if style.hidden {
        attributes.push("hidden");
    }
    if style.strikeout {
        attributes.push("strikeout");
    }
    serde_json::json!({
        "foreground": color_json(style.foreground),
        "background": color_json(style.background),
        "underline_color": style.underline_color.map(color_json),
        "attributes": attributes,
        "hyperlink": style.hyperlink.as_ref().map(|link| serde_json::json!({
            "id": link.id,
            "uri": link.uri,
        })),
    })
}

fn exit_result(pane_id: PaneId, status: Option<PtyExitStatus>) -> serde_json::Value {
    serde_json::json!({
        "pane_id": pane_id,
        "code": status.and_then(|status| status.code),
        "signal": status.and_then(|status| status.signal),
        "success": status.is_some_and(|status| status.success),
        "status_available": status.is_some(),
    })
}
