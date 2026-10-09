//! Attached clients: connection handling, presentation roles, and per-client display state.

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
    pub(super) fn handle_client(
        &mut self,
        id: u64,
        writer: SharedWriter,
        cancel: crate::platform::ConnectionCancel,
        message: ClientMessage,
    ) -> io::Result<()> {
        match message {
            ClientMessage::Attach {
                replace,
                target,
                display,
                vivid,
                kitty_graphics,
                outer,
                media,
                read_only,
            } => {
                let view = match target {
                    AttachmentTarget::Session => AttachmentView::Session,
                    AttachmentTarget::Pane { pane_id } => {
                        if !self.panes.contains_key(&pane_id) {
                            return crate::ipc::send(
                                &writer,
                                &ServerMessage::Error(format!("pane {pane_id} does not exist")),
                            );
                        }
                        AttachmentView::Pane(pane_id)
                    }
                    AttachmentTarget::Agent { alias } => {
                        let Some(pane_id) = self.pane_with_agent_alias(&alias) else {
                            return crate::ipc::send(
                                &writer,
                                &ServerMessage::Error(format!(
                                    "no live agent owns alias `{alias}`"
                                )),
                            );
                        };
                        AttachmentView::Pane(pane_id)
                    }
                };
                if self.clients.contains_key(&id) {
                    return crate::ipc::send(
                        &writer,
                        &ServerMessage::Error("this connection is already attached".into()),
                    );
                }
                // Clients share one view, as tmux clients share a session's current window: a
                // pane has one PTY size, so two clients cannot each see it laid out differently.
                if !replace && !self.clients.is_empty() && self.view != view {
                    return crate::ipc::send(
                        &writer,
                        &ServerMessage::Error(
                            "session already has an attached client showing a different view; \
                             attach with -d to replace it"
                                .into(),
                        ),
                    );
                }
                if replace {
                    self.detach_all_clients("replaced by another client");
                }
                if self.clients.len() >= MAX_ATTACHED_CLIENTS {
                    return crate::ipc::send(
                        &writer,
                        &ServerMessage::Error(format!(
                            "session already has the maximum of {MAX_ATTACHED_CLIENTS} attached clients"
                        )),
                    );
                }
                let first = self.clients.is_empty();
                if first {
                    self.view = view;
                    self.cancel_pointer_drag(true);
                    self.invalidate_mouse_selection_state();
                    self.end_float_mode(true);
                    self.clear_transient_ui();
                    self.ui_owner = None;
                }
                let display = normalized_display(
                    display,
                    if matches!(view, AttachmentView::Session) {
                        self.tab_view
                    } else {
                        TabView::Hidden
                    },
                );
                self.client_activity = self.client_activity.wrapping_add(1);
                let ipc = writer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .counters();
                self.clients.insert(
                    id,
                    AttachedClient {
                        id,
                        writer: Arc::clone(&writer),
                        display,
                        vivid,
                        media_enabled: media != crate::ipc::MediaRequest::Never,
                        shared_visuals: SharedVisualState::default(),
                        kitty_graphics,
                        read_only,
                        focused: true,
                        activity: self.client_activity,
                        // This client never received the session's earlier frames; its first frame
                        // is a forced full repaint.
                        frame_id: 0,
                        acknowledged_frame: 0,
                        rendered_session_sequence: 0,
                        frame_sequences: VecDeque::new(),
                        last_screen: None,
                        force_full: true,
                        render_pending: true,
                        status_tab_targets: Vec::new(),
                        sidebar_targets: Vec::new(),
                        reported_input_mode: None,
                        ipc,
                        #[cfg(windows)]
                        outer_bracketed_paste: None,
                        outer,
                    },
                );
                // Playback and host services have one owner; retained visuals are shared.
                let presents = (vivid || kitty_graphics)
                    && match media {
                        crate::ipc::MediaRequest::Claim => true,
                        crate::ipc::MediaRequest::IfVacant => self.presenter.is_none(),
                        crate::ipc::MediaRequest::Never => false,
                    };
                if presents {
                    // Even a clean replacement owns a different physical presenter and fresh
                    // decoder/audio devices. Timed ingress stays parked until this client applies
                    // its first authoritative projection.
                    self.set_presenter(Some(id));
                }
                crate::ipc::send(
                    &writer,
                    &ServerMessage::Attached {
                        session: self.name.clone(),
                        presenter: presents,
                    },
                )?;
                let _ = crate::ipc::send(&writer, &self.plugin_keymap_message());
                if first {
                    // Detached tabs were laid out against the placeholder host, so floats authored
                    // by percentages re-proportion onto the attaching host — and any float keeps
                    // fitting — before the first text or media projection is published.
                    self.last_display = self.policy_display().unwrap_or(display);
                    if matches!(view, AttachmentView::Session) {
                        let area = self.content_area();
                        for tab in &mut self.tabs {
                            tab.floating.reproportion(area);
                        }
                    } else if let AttachmentView::Pane(pane_id) = view
                        && let Some(pane) = self.panes.get_mut(&pane_id)
                    {
                        pane.copy = None;
                    }
                    self.force_full = true;
                    self.resize_all();
                    // After the resize, never before: an agent reads its terminal size as it
                    // starts, and one launched against the placeholder geometry would lay itself
                    // out for a window that does not exist. This is also why a resume waits for an
                    // attach at all.
                    self.fire_pending_resumes(self.direct_pane());
                } else {
                    self.refresh_layout_display();
                }
                self.sync_media(true);
                self.schedule_render();
            }
            ClientMessage::ClaimMedia => {
                let Some(client) = self.clients.get(&id) else {
                    return Ok(());
                };
                if self.presenter == Some(id) {
                    self.status_to(id, "this client already presents media");
                } else if !client.media_capable() {
                    self.status_to(id, "this terminal cannot present media");
                } else {
                    self.clients
                        .get_mut(&id)
                        .expect("attached client")
                        .media_enabled = true;
                    self.set_presenter(Some(id));
                    crate::ipc::send(&writer, &ServerMessage::MediaRole { presenter: true })?;
                    // Cell pixels follow the presenter.
                    self.refresh_layout_display();
                    self.sync_media(true);
                    self.schedule_render();
                }
            }
            ClientMessage::Input(bytes) => {
                if self.begin_client_input(id, false) {
                    self.input(bytes);
                }
            }
            ClientMessage::KeyInput { bytes, keys } => {
                if self.begin_client_input(id, false) {
                    let mut offset = 0;
                    for key in &keys {
                        if key.start < offset
                            || key.end <= key.start
                            || key.end > bytes.len()
                            || key.text.len() > 4096
                            || (!key.down && (!key.text.is_empty() || key.repeat))
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "invalid key input range",
                            ));
                        }
                        offset = key.end;
                    }
                    offset = 0;
                    for key in keys {
                        if key.start > offset {
                            self.input(bytes[offset..key.start].to_vec());
                        }
                        let consumed = self.attached_focus_pane().is_some_and(|pane| {
                            if !self.vivid.overlay_has_focus(pane) {
                                return false;
                            }
                            let mut consumed = false;
                            if key.physical != 0 {
                                consumed = self.vivid.overlay_key_event(
                                    pane,
                                    key.physical,
                                    key.down,
                                    key.repeat,
                                    key.modifiers,
                                );
                            }
                            if !key.text.is_empty() {
                                consumed |= self.vivid.overlay_text(pane, &key.text);
                            }
                            consumed
                        });
                        if !consumed {
                            self.input(bytes[key.start..key.end].to_vec());
                        }
                        offset = key.end;
                    }
                    if offset < bytes.len() {
                        self.input(bytes[offset..].to_vec());
                    }
                }
            }
            ClientMessage::Mouse(mouse) => {
                if self.begin_client_input(id, false) {
                    self.mouse(mouse, false);
                }
            }
            ClientMessage::PixelMouse(mouse) => {
                if self.begin_client_input(id, false) {
                    self.mouse(mouse, true);
                }
            }
            ClientMessage::Focus(focused) => {
                if let Some(client) = self.clients.get_mut(&id) {
                    client.focused = focused;
                    if !focused {
                        // There is no pointer-leave report, so blur is the only signal that the
                        // pointer is gone. Without this a link stays marked as hovered while the
                        // user works in another window.
                        self.set_hovered_link(None);
                    }
                }
            }
            ClientMessage::Resize(display) => {
                if let Some(client) = self.clients.get(&id) {
                    let display = normalized_display(
                        display,
                        if self.direct_pane().is_none() {
                            self.tab_view
                        } else {
                            TabView::Hidden
                        },
                    );
                    // A client may re-send its display without changing it: browser presenters
                    // report every dimension probe, not only real resizes. Relaying a phantom
                    // resize would bump `layout_revision`, so `should_sync_media` would rebuild
                    // the outer Vivid session on each one and destroy media that is still being
                    // projected. Only a display that actually changed is a resize.
                    if is_display_change(Some(client.display), display) {
                        self.client_activity = self.client_activity.wrapping_add(1);
                        if let Some(client) = self.clients.get_mut(&id) {
                            client.display = display;
                            client.activity = self.client_activity;
                            client.force_full = true;
                        }
                        // Whether the panes follow depends on `general.window_size`; this client
                        // repaints at its new size either way.
                        self.refresh_layout_display();
                        self.schedule_render();
                    }
                }
            }
            ClientMessage::Action(action) => {
                if self.direct_pane().is_none() && self.begin_client_input(id, true) {
                    self.action(action);
                }
            }
            ClientMessage::RenderAck(frame_id) => {
                if let Some(client) = self.clients.get_mut(&id) {
                    if frame_id < client.acknowledged_frame || frame_id > client.frame_id {
                        client.force_full = true;
                        client.render_pending = true;
                        self.pending_render = true;
                    } else {
                        client.acknowledged_frame = frame_id;
                        while let Some(&(sent_frame, sequence)) = client.frame_sequences.front() {
                            if sent_frame > frame_id {
                                break;
                            }
                            client.rendered_session_sequence =
                                client.rendered_session_sequence.max(sequence);
                            client.frame_sequences.pop_front();
                        }
                        // A change held back while this client's backlog was full is due now.
                        self.pending_render |= client.render_pending && !client.render_blocked();
                    }
                }
            }
            ClientMessage::RenderResync => {
                if let Some(client) = self.clients.get_mut(&id) {
                    // Treat the discarded backlog as acknowledged: those frames will never be
                    // displayed, and leaving them outstanding would stall the render gate.
                    client.acknowledged_frame = client.frame_id;
                    client.frame_sequences.clear();
                    client.force_full = true;
                    client.last_screen = None;
                    client.render_pending = true;
                    self.pending_render = true;
                }
            }
            ClientMessage::BridgeNeedKeyframes(requests) => {
                if self.is_presenter(id) {
                    for request in requests {
                        self.record_media_trace(
                            Some(request.source),
                            self.bridge_instance_id,
                            None,
                            MediaTraceKind::KeyframeRequest {
                                stage: MediaKeyframeStage::ProducerQueued,
                                minimum_epoch: request.minimum_epoch,
                                reason: request.reason,
                            },
                        );
                        let outcome = self.vivid.request_keyframe(
                            request.source,
                            request.minimum_epoch,
                            request.reason,
                        );
                        self.record_media_trace(
                            Some(request.source),
                            self.bridge_instance_id,
                            None,
                            MediaTraceKind::KeyframeRequest {
                                stage: match outcome {
                                    vivid_sdk::presenter::KeyframeRequestOutcome::Forwarded => {
                                        MediaKeyframeStage::ProducerWritten
                                    }
                                    vivid_sdk::presenter::KeyframeRequestOutcome::Damped => {
                                        MediaKeyframeStage::ProducerDamped
                                    }
                                    vivid_sdk::presenter::KeyframeRequestOutcome::Ignored => {
                                        MediaKeyframeStage::ProducerIgnored
                                    }
                                },
                                minimum_epoch: request.minimum_epoch,
                                reason: request.reason,
                            },
                        );
                    }
                }
            }
            ClientMessage::BridgeNeedFullFrames(sources) => {
                if let Some(client) = self.clients.get_mut(&id) {
                    for source in &sources {
                        client.shared_visuals.sent.remove(source);
                    }
                    self.last_media_projection = None;
                    self.sync_media(false);
                }
                if self.is_presenter(id) {
                    let exclusive = sources
                        .into_iter()
                        .filter(|source| !self.shared_visual_sources.contains(source))
                        .collect::<Vec<_>>();
                    self.vivid.request_full_frames(&exclusive, 1);
                }
            }
            ClientMessage::BridgeCapabilitiesChanged { reason_mask } => {
                if self.is_presenter(id) {
                    let _ = self.vivid.notify_capabilities_changed(reason_mask);
                }
            }
            ClientMessage::BridgeMediaAck {
                delivery_id,
                delivered,
            } => {
                if self.is_presenter(id) {
                    self.record_delivery_result(delivery_id, delivered);
                    let resync = self.vivid.complete_bridge_delivery(delivery_id, delivered);
                    if resync {
                        self.last_media_projection = None;
                        self.sync_media(true);
                    }
                }
            }
            ClientMessage::Microphone {
                bridge_instance_id,
                source,
                generation,
                bytes,
            } => {
                if self.is_presenter(id)
                    && self
                        .clients
                        .get(&id)
                        .is_some_and(|client| !client.read_only)
                    && self.bridge_instance_id == Some(bridge_instance_id)
                {
                    if bytes.is_empty() {
                        let _ = self.vivid.queue_microphone(source, generation, &bytes);
                        if self
                            .microphone_recipient
                            .is_some_and(|(key, epoch, _, _)| key == source && epoch == generation)
                        {
                            self.microphone_recipient = None;
                            self.schedule_render();
                        }
                    } else if let Some(request) =
                        self.vivid
                            .microphone_requests()
                            .into_iter()
                            .find(|request| {
                                request.source == source && request.generation == generation
                            })
                    {
                        if let Some((previous, epoch, _, _)) = self.microphone_recipient
                            && (previous != source || epoch != generation)
                        {
                            let _ = self.vivid.queue_microphone(previous, epoch, &[]);
                        }
                        if self
                            .vivid
                            .queue_microphone(source, generation, &bytes)
                            .unwrap_or(false)
                        {
                            self.microphone_recipient =
                                Some((source, generation, request.pane, Instant::now()));
                            self.schedule_render();
                        }
                    }
                }
            }
            ClientMessage::BridgeMediaReleased { delivery_id } => {
                if self.is_presenter(id) {
                    self.traced_recovery_deliveries.remove(&delivery_id);
                    self.vivid.release_bridge_delivery(delivery_id);
                }
            }
            ClientMessage::BridgeRetainedResult {
                bridge_instance_id,
                source,
                delivered,
            } => {
                let primary =
                    self.is_presenter(id) && self.bridge_instance_id == Some(bridge_instance_id);
                if primary {
                    self.retained_replay_requests.remove(&source);
                    self.retained_replay_inflight.remove(&source);
                    if delivered {
                        self.vivid.complete_retained_hydration(source);
                    }
                }
                if let Some(client) = self.clients.get_mut(&id)
                    && (primary
                        || client.shared_visuals.bridge_instance == Some(bridge_instance_id))
                    && client.shared_visuals.inflight.remove(&source)
                {
                    if delivered {
                        self.vivid.complete_retained_hydration(source);
                    } else {
                        client.shared_visuals.sent.remove(&source);
                    }
                    self.last_media_projection = None;
                    self.sync_media(false);
                }
            }
            ClientMessage::BridgeSnapshotRetry {
                reset_outer_session,
            } => {
                if let Some(client) = self.clients.get_mut(&id) {
                    client.shared_visuals.sent.clear();
                    client.shared_visuals.inflight.clear();
                }
                if self.is_presenter(id) {
                    self.record_media_trace(
                        None,
                        self.bridge_instance_id,
                        None,
                        MediaTraceKind::SnapshotRetry,
                    );
                    if reset_outer_session {
                        // Fragment and attachment identities are scoped to the outer session.
                        // Source-scoped recovery reuses that session and must preserve unrelated
                        // mappings; only a confirmed replacement invalidates all of them.
                        self.fragment_assignments.clear();
                        self.outer_attachment_generations.clear();
                        self.retained_replay_requests.clear();
                        self.retained_replay_inflight.clear();
                        self.last_media_projection = None;
                    } else {
                        self.retained_replay_requests
                            .extend(self.retained_replay_inflight.iter().copied());
                        self.last_media_projection = None;
                    }
                    self.sync_media(true);
                } else if let Some(client) = self.clients.get_mut(&id) {
                    let state = &mut client.shared_visuals;
                    state.pending_revision = None;
                    state.last_projection = None;
                    state.sent.clear();
                    state.inflight.clear();
                    self.last_media_projection = None;
                    self.sync_media(false);
                }
            }
            ClientMessage::BridgeApplied {
                bridge_instance_id,
                virtual_revision,
                outer_revision,
                outer_attachment_generations,
                recreated_retained_sources,
            } => {
                if !self.is_presenter(id) {
                    if let Some(client) = self.clients.get_mut(&id) {
                        let state = &mut client.shared_visuals;
                        if state.bridge_instance == Some(bridge_instance_id)
                            && state.pending_revision == Some(virtual_revision)
                        {
                            state.pending_revision = None;
                            state.applied_sources = std::mem::take(&mut state.pending_sources);
                            state.outer_revision = outer_revision;
                            state.attachment_generations =
                                outer_attachment_generations.into_iter().collect();
                            for source in recreated_retained_sources {
                                if !state.inflight.contains(&source) {
                                    state.sent.remove(&source);
                                }
                            }
                            if self.presenter_client().is_none_or(|client| !client.vivid) {
                                self.outer_apply_sequence =
                                    self.outer_apply_sequence.saturating_add(1);
                                self.outer_projection_revision = next_outer_compatibility_revision(
                                    self.outer_projection_revision,
                                    outer_revision,
                                );
                            }
                            self.last_media_projection = None;
                            self.sync_media(false);
                        }
                    }
                    return Ok(());
                }
                let instance_changed = self.bridge_instance_id != Some(bridge_instance_id);
                if self.is_presenter(id)
                    && bridge_apply_is_current(
                        self.bridge_instance_id,
                        self.outer_virtual_revision,
                        self.bridge_local_revision,
                        bridge_instance_id,
                        virtual_revision,
                        outer_revision,
                    )
                {
                    if instance_changed {
                        self.bridge_local_revision = 0;
                        self.outer_attachment_generations.clear();
                        self.retained_replay_requests.clear();
                        self.retained_replay_inflight.clear();
                    }
                    self.bridge_instance_id = Some(bridge_instance_id);
                    self.outer_virtual_revision = virtual_revision;
                    self.bridge_local_revision = outer_revision;
                    self.outer_apply_sequence = self.outer_apply_sequence.saturating_add(1);
                    self.outer_projection_revision = next_outer_compatibility_revision(
                        self.outer_projection_revision,
                        outer_revision,
                    );
                    let attachment_count =
                        u16::try_from(outer_attachment_generations.len()).unwrap_or(u16::MAX);
                    self.outer_attachment_generations =
                        outer_attachment_generations.into_iter().collect();
                    let resident_sources = self
                        .outer_attachment_generations
                        .keys()
                        .copied()
                        .collect::<HashSet<_>>();
                    self.retained_replay_requests
                        .retain(|source| resident_sources.contains(source));
                    self.retained_replay_inflight
                        .retain(|source| resident_sources.contains(source));
                    self.record_media_trace(
                        None,
                        Some(bridge_instance_id),
                        None,
                        MediaTraceKind::ProjectionApplied {
                            virtual_revision,
                            bridge_local_revision: outer_revision,
                            attachment_count,
                        },
                    );
                    let mut retry_retained = false;
                    if let Some(client) = self.clients.get_mut(&id) {
                        for source in &recreated_retained_sources {
                            if !client.shared_visuals.inflight.contains(source) {
                                client.shared_visuals.sent.remove(source);
                            }
                        }
                    }
                    if let Some(applied) = self.pending_media_projections.remove(&virtual_revision)
                    {
                        self.pending_media_projections
                            .retain(|revision, _| *revision > virtual_revision);
                        let requests = retained_replays_after_apply(
                            &recreated_retained_sources,
                            &applied.retained_replay_candidates,
                            &applied.retained_replays,
                            &self.retained_replay_inflight,
                        );
                        retry_retained = !requests.is_empty();
                        self.retained_replay_requests.extend(requests);
                        self.record_projection_sources(&applied.sources, applied.gateway_revision);
                        self.vivid
                            .activate_bridge_projection_at_resets(&applied.decoder_reset_serials);
                    }
                    self.check_automation_waiters();
                    if retry_retained {
                        // The just-applied outer projection recreated a retained track after the
                        // snapshot was prepared against stale residency. Publish the same
                        // projection once more and force only those missing bodies across VVMX.
                        self.last_media_projection = None;
                        self.sync_media(true);
                    }
                }
            }
            ClientMessage::BridgeTrace {
                bridge_instance_id,
                event,
            } => {
                if !self.is_presenter(id)
                    && matches!(event.kind, MediaTraceKind::BridgeClientAttached { .. })
                    && let Some(client) = self.clients.get_mut(&id)
                    && client.vivid
                {
                    let state = &mut client.shared_visuals;
                    if state.bridge_instance != Some(bridge_instance_id) {
                        if state.bridge_instance.is_some() {
                            state.pending_revision = None;
                            state.last_projection = None;
                            state.sent.clear();
                            state.inflight.clear();
                            state.applied_sources.clear();
                            state.attachment_generations.clear();
                        }
                        state.bridge_instance = Some(bridge_instance_id);
                        self.last_media_projection = None;
                        self.sync_media(false);
                    }
                }
                if self.is_presenter(id) {
                    if matches!(event.kind, MediaTraceKind::BridgeClientAttached { .. })
                        || self.bridge_instance_id.is_none()
                    {
                        if self.bridge_instance_id != Some(bridge_instance_id) {
                            self.bridge_local_revision = 0;
                            self.outer_attachment_generations.clear();
                            self.retained_replay_requests.clear();
                            self.retained_replay_inflight.clear();
                        }
                        self.bridge_instance_id = Some(bridge_instance_id);
                    }
                    if self.bridge_instance_id == Some(bridge_instance_id) {
                        self.record_media_trace(
                            event.source,
                            Some(bridge_instance_id),
                            Some(event.origin_monotonic_us),
                            event.kind,
                        );
                    }
                }
            }
            ClientMessage::OverlayInput {
                bridge_instance_id,
                surface,
                body,
            } => {
                if self.is_presenter(id)
                    && self.bridge_instance_id == Some(bridge_instance_id)
                    && self.vivid.relay_overlay_input(surface, &body)?
                    && let Some(pane) = self.vivid.pane_for_overlay_surface(surface)
                    && self.vivid.overlay_has_focus(pane)
                    && self.attached_focus_pane() != Some(pane)
                    && let Some(tab) = self.active_tab_mut()
                    && tab.contains(pane)
                {
                    tab.set_focus(pane);
                    self.projection_changed();
                }
            }
            ClientMessage::OverlayHostProfiles {
                bridge_instance_id,
                profiles,
            } => {
                if self.is_presenter(id) && self.bridge_instance_id == Some(bridge_instance_id) {
                    self.vivid.set_overlay_host_profiles(&profiles);
                }
            }
            ClientMessage::OverlayEnvironment {
                bridge_instance_id,
                body,
            } => {
                if self.is_presenter(id) && self.bridge_instance_id == Some(bridge_instance_id) {
                    let envelope = vivid_protocol::messages::decode_control(&body)?;
                    let environment = vivid_protocol::overlay::wire::EnvironmentChanged::decode(
                        0,
                        &vivid_protocol::cbor::Value::Map(envelope.payload),
                    )?;
                    for pane in self.panes.keys() {
                        self.vivid
                            .set_overlay_environment(*pane, environment.environment.clone());
                    }
                }
            }
            ClientMessage::OverlayHostReply {
                id: request_id,
                response,
            } => {
                if self.is_presenter(id) {
                    self.vivid
                        .complete_overlay_host_request(request_id, response);
                }
            }
            ClientMessage::BridgePosition {
                bridge_instance_id,
                source,
                position,
            } => {
                if self.is_presenter(id) && self.bridge_instance_id == Some(bridge_instance_id) {
                    self.vivid.apply_outer_position(source, position);
                }
            }
            ClientMessage::BridgeHold {
                bridge_instance_id,
                source,
                hold,
            } => {
                if self.is_presenter(id) && self.bridge_instance_id == Some(bridge_instance_id) {
                    self.vivid.apply_downstream_hold(source, hold);
                }
            }
            ClientMessage::BridgeIncompatiblePlayback {
                bridge_instance_id,
                source,
                decoder_reset_serial,
            } => {
                if self.is_presenter(id) && self.bridge_instance_id == Some(bridge_instance_id) {
                    self.vivid
                        .reject_incompatible_playback(source, decoder_reset_serial);
                }
            }
            ClientMessage::BridgePlaybackState {
                bridge_instance_id,
                decoder_reset_serial,
                source,
                state,
                eos_state,
            } => {
                if self.is_presenter(id) && self.bridge_instance_id == Some(bridge_instance_id) {
                    self.record_media_trace(
                        Some(source),
                        self.bridge_instance_id,
                        None,
                        MediaTraceKind::PlaybackState { state, eos_state },
                    );
                    self.vivid
                        .apply_outer_playback(source, decoder_reset_serial, state, eos_state);
                }
            }
            ClientMessage::BridgeMetrics(metrics) => {
                if self.is_presenter(id) {
                    self.bridge_metrics = metrics;
                }
            }
            ClientMessage::Detach => {
                if self.client_is(id) {
                    self.detach_client(id, Some("detached"));
                }
            }
            ClientMessage::Kill => {
                self.shutdown.store(true, Ordering::Release);
                self.send_to_clients(&ServerMessage::Detached {
                    reason: "session killed".into(),
                });
            }
            ClientMessage::FloatingEdit { mode_id, command } => {
                if self.begin_client_input(id, false) && self.ui_takes_current_input() {
                    self.float_edit(mode_id, command);
                }
            }
            ClientMessage::Ping => {
                crate::ipc::send(&writer, &ServerMessage::Pong)?;
            }
            ClientMessage::Automation(request) => {
                self.handle_automation(id, writer, cancel, request);
            }
        }
        Ok(())
    }

    pub(super) fn client_is(&self, id: u64) -> bool {
        self.clients.contains_key(&id)
    }

    pub(super) fn is_presenter(&self, id: u64) -> bool {
        self.presenter == Some(id) && self.clients.contains_key(&id)
    }

    pub(super) fn presenter_client(&self) -> Option<&AttachedClient> {
        self.presenter.and_then(|id| self.clients.get(&id))
    }

    /// The client that most recently attached, resized, or sent input.
    pub(super) fn latest_client(&self) -> Option<&AttachedClient> {
        self.clients.values().max_by_key(|client| client.activity)
    }

    /// The client whose acknowledgements automation waits on: the presenter, whose frames are the
    /// ones a media-aware caller is waiting to see, else whoever is using the session.
    pub(super) fn witness_client(&self) -> Option<&AttachedClient> {
        self.presenter_client().or_else(|| self.latest_client())
    }

    /// Whether anyone is looking: some attached client's host terminal holds focus.
    pub(super) fn any_client_focused(&self) -> bool {
        self.clients.values().any(|client| client.focused)
    }

    pub(super) fn send_to_clients(&self, message: &ServerMessage) {
        for client in self.clients.values() {
            let _ = crate::ipc::send(&client.writer, message);
        }
    }

    /// A status line for one client, such as a refusal only that client's user asked for.
    pub(super) fn status_to(&self, id: u64, message: &str) {
        if let Some(client) = self.clients.get(&id) {
            let _ = crate::ipc::send(&client.writer, &ServerMessage::Status(message.into()));
        }
    }

    pub(super) fn send_plugin_keymap(&self) {
        self.send_to_clients(&self.plugin_keymap_message());
    }

    pub(super) fn plugin_keymap_message(&self) -> ServerMessage {
        ServerMessage::PluginKeymap {
            generation: self.plugin_registration_generation,
            bindings: self.plugin_keybindings.clone(),
        }
    }

    /// The pane every client shows over its whole terminal, when the shared view is one pane.
    pub(super) fn direct_pane(&self) -> Option<PaneId> {
        match (self.clients.is_empty(), self.view) {
            (false, AttachmentView::Pane(pane_id)) => Some(pane_id),
            _ => None,
        }
    }

    /// UI that belongs to whichever client opened it: menus, prompts, float editing, and drags.
    pub(super) fn owned_ui_active(&self) -> bool {
        self.transient_ui_active()
            || self.float_modal.is_some()
            || self.pointer_drag.is_some()
            || self.mouse_selection_drag.is_some()
    }

    /// Whether the transient UI is drawn on this client's terminal.
    pub(super) fn ui_visible_to(&self, id: u64) -> bool {
        self.ui_owner.is_none_or(|owner| owner == id)
    }

    /// Whether the message being handled may drive the transient UI. Another client's keys skip
    /// it and reach the focused pane; its pointer events are dropped until the UI closes.
    /// Automation, which has no client, drives it as it always did.
    pub(super) fn ui_takes_current_input(&self) -> bool {
        self.current_client.is_none()
            || self.ui_owner.is_none()
            || self.ui_owner == self.current_client
    }

    /// Admit one input-bearing message from client `id`, or refuse it for a read-only client.
    ///
    /// Makes `id` the current client and, while no transient UI is open, its owner-to-be, so a
    /// menu opened by this message belongs to this client. `supersede` is for deliberate
    /// commands: they close another client's open UI instead of being refused behind it.
    pub(super) fn begin_client_input(&mut self, id: u64, supersede: bool) -> bool {
        let Some(client) = self.clients.get_mut(&id) else {
            return false;
        };
        if client.read_only {
            return false;
        }
        self.client_activity = self.client_activity.wrapping_add(1);
        client.activity = self.client_activity;
        self.current_client = Some(id);
        if supersede && self.ui_owner.is_some_and(|owner| owner != id) && self.owned_ui_active() {
            self.cancel_pointer_drag(true);
            self.mouse_selection_drag = None;
            self.end_float_mode(true);
            self.clear_transient_ui();
            self.schedule_render();
        }
        if !self.owned_ui_active() {
            self.ui_owner = Some(id);
        }
        // `latest` sizing follows whoever is typing.
        if self.config.general.window_size == crate::config::WindowSize::Latest {
            self.refresh_layout_display();
        }
        true
    }

    /// The display panes are laid out for, from the attached clients by `general.window_size`.
    ///
    /// Columns and rows follow the policy. Canonical cell pixels follow the presenter, or the
    /// latest sizing client when vacant; other bridges map that layout to their own target.
    pub(super) fn policy_display(&self) -> Option<DisplayMetrics> {
        // A read-only watcher resizing its window must not re-lay out the panes under whoever is
        // typing; it counts only while nobody else is attached.
        let typists = self.clients.values().any(|client| !client.read_only);
        let sizing = self
            .clients
            .values()
            .filter(|client| !typists || !client.read_only)
            .map(|client| (client.activity, client.display))
            .collect::<Vec<_>>();
        let latest = sizing.iter().max_by_key(|(activity, _)| *activity)?.1;
        let (columns, rows) = match self.config.general.window_size {
            crate::config::WindowSize::Latest => (latest.columns, latest.rows),
            crate::config::WindowSize::Smallest => (
                sizing.iter().map(|(_, display)| display.columns).min()?,
                sizing.iter().map(|(_, display)| display.rows).min()?,
            ),
            crate::config::WindowSize::Largest => (
                sizing.iter().map(|(_, display)| display.columns).max()?,
                sizing.iter().map(|(_, display)| display.rows).max()?,
            ),
        };
        let cells = self
            .presenter_client()
            .map_or(latest, |client| client.display);
        Some(DisplayMetrics {
            columns,
            rows,
            cell_width: cells.cell_width,
            cell_height: cells.cell_height,
        })
    }

    /// Re-derive the layout display and, when it moved, lay every pane out for it.
    ///
    /// Geometry-bound interaction — drags, float editing, pointer state — is cancelled, exactly as
    /// a host resize always did. Returns whether the layout changed.
    pub(super) fn refresh_layout_display(&mut self) -> bool {
        let Some(next) = self.policy_display() else {
            return false;
        };
        if !is_display_change(Some(self.last_display), next) {
            return false;
        }
        self.last_display = next;
        self.cancel_pointer_drag(true);
        self.invalidate_mouse_selection_state();
        self.end_float_mode(true);
        self.clear_transient_ui();
        self.force_full = true;
        if self.direct_pane().is_none() {
            // Deterministic host-resize behavior: origin-ful floats re-proportion to their
            // percents, the rest clamp size before position, per float.
            let area = self.content_area();
            for tab in &mut self.tabs {
                tab.floating.reproportion(area);
            }
            self.relayout();
        } else {
            self.resize_all();
            self.sync_media(true);
            self.schedule_render();
        }
        true
    }

    /// Drop everything bound to the presenter's physical bridge.
    ///
    /// The next presenter owns a different outer presenter with fresh decoders and devices, so
    /// nothing here may carry over. Timed ingress stays parked until a presenter applies its first
    /// authoritative projection.
    pub(super) fn release_presentation(&mut self) {
        self.vivid.deactivate_bridge();
        self.vivid.revoke_microphones();
        self.microphone_recipient = None;
        self.bridge_instance_id = None;
        self.bridge_local_revision = 0;
        self.pending_media_projections.clear();
        self.outer_attachment_generations.clear();
        self.retained_replay_requests.clear();
        self.retained_replay_inflight.clear();
        self.traced_recovery_deliveries.clear();
        self.fragment_assignments.clear();
        self.last_projection_warning = None;
        self.record_projection_sources(&HashSet::new(), self.vivid.revision());
        // Kitty uploads belonged to the old presenter's terminal, and the new one's placeholder
        // cells are blanked or not depending on what it can show.
        self.clear_kitty_graphics();
        self.force_full = true;
    }

    /// Move the exclusive role. The demoted client keeps its retained subscription; the caller
    /// tells a promoted client and publishes its first exclusive projection.
    ///
    /// Sends nothing to the promoted client, and neither relayouts nor syncs media: an attach
    /// must put its reply on the stream before any projection.
    pub(super) fn set_presenter(&mut self, next: Option<u64>) {
        if self.presenter == next {
            return;
        }
        let previous = std::mem::replace(&mut self.presenter, next);
        if let Some(client) = previous.and_then(|id| self.clients.get_mut(&id)) {
            client.shared_visuals = SharedVisualState {
                bridge_instance: self.bridge_instance_id,
                ..SharedVisualState::default()
            };
        }
        if previous.is_some() {
            self.record_media_trace(
                None,
                self.bridge_instance_id,
                None,
                MediaTraceKind::BridgeClientDetached,
            );
        }
        self.release_presentation();
        if let Some(previous) = previous.and_then(|id| self.clients.get(&id)) {
            let _ = crate::ipc::send(
                &previous.writer,
                &ServerMessage::MediaRole { presenter: false },
            );
        }
        if let Some(client) = next.and_then(|id| self.clients.get(&id)) {
            let vivid = client.vivid;
            self.client_ipc = Some(Arc::clone(&client.ipc));
            self.bridge_metrics = crate::metrics::BridgeMetrics::default();
            self.record_media_trace(
                None,
                None,
                None,
                MediaTraceKind::BridgeClientAttached { vivid },
            );
        }
    }

    /// Detach one client. `reason` is sent first, for a client that is still listening.
    ///
    /// Every other client stays attached and keeps its render state; only what this client owned
    /// — the media role, an open menu or drag — is released.
    pub(super) fn detach_client(&mut self, id: u64, reason: Option<&str>) {
        let Some(client) = self.clients.remove(&id) else {
            return;
        };
        if let Some(reason) = reason {
            let _ = crate::ipc::send(
                &client.writer,
                &ServerMessage::Detached {
                    reason: reason.into(),
                },
            );
        }
        if self.current_client == Some(id) {
            self.current_client = None;
        }
        if self.ui_owner == Some(id) || self.clients.is_empty() {
            self.cancel_pointer_drag(true);
            self.invalidate_mouse_selection_state();
            self.end_float_mode(true);
            self.clear_transient_ui();
            self.ui_owner = None;
        }
        if self.presenter == Some(id) {
            self.set_presenter(None);
            // The role is never handed on implicitly, so say where it went: nowhere, until one
            // of these clients asks for it.
            for client in self
                .clients
                .values()
                .filter(|client| client.media_capable())
            {
                let _ = crate::ipc::send(
                    &client.writer,
                    &ServerMessage::Status(
                        "the media presenter detached; prefix M presents media here".into(),
                    ),
                );
            }
        }
        if self.clients.is_empty() {
            self.force_full = true;
            self.vivid.deactivate_bridge();
            self.shared_visual_sources.clear();
        } else {
            self.refresh_layout_display();
            self.sync_media(true);
            self.schedule_render();
        }
    }

    pub(super) fn detach_all_clients(&mut self, reason: &str) {
        let ids = self.clients.keys().copied().collect::<Vec<_>>();
        for id in ids {
            self.detach_client(id, Some(reason));
        }
    }

    pub(super) fn attached_focus_pane(&self) -> Option<PaneId> {
        self.direct_pane()
            .or_else(|| self.active_tab().map(|tab| tab.focused))
    }

    pub(super) fn attached_projections(&self, area: Rect) -> Vec<PaneProjection> {
        if let Some(pane_id) = self.direct_pane() {
            return self
                .panes
                .contains_key(&pane_id)
                .then_some(PaneProjection {
                    pane_id,
                    outer: area,
                    content: area,
                    layer: PaneLayer::Tiled,
                    focused: true,
                })
                .into_iter()
                .collect();
        }
        self.active_tab()
            .map(|tab| visible_projections(tab, area))
            .unwrap_or_default()
    }
}
