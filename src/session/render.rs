//! Rendering, client resize, and media projection to attached clients.

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
    pub(super) fn resize_all(&mut self) {
        let area = self.content_area();
        let display = self.layout_display();
        let mut resize_failures = Vec::new();
        let mut resized_panes = 0_u64;
        let projections = if self.direct_pane().is_some() {
            self.attached_projections(area)
        } else {
            self.tabs
                .iter()
                .flat_map(|tab| visible_projections(tab, area))
                .collect()
        };
        // Hidden ordinary floats keep consuming PTY output but are not resized while
        // hidden; a re-shown float is resized here on the next relayout if its content
        // dimensions changed.
        for projection in projections {
            if let Some(pane) = self.panes.get_mut(&projection.pane_id) {
                let content = projection.content;
                // A pane squeezed to nothing still has a live program behind it, and neither
                // a terminal grid nor a PTY has a zero dimension. Such a pane keeps a single
                // cell so the window can shrink past its frame and grow back with the pane
                // and its program intact.
                let columns = content.width.max(1);
                let rows = content.height.max(1);
                // A capture in flight raises this pane's advertised cell size so its producer
                // re-renders at a higher density. It rides the ordinary metrics path so the
                // change is a normal resize to the producer and reverts the same way.
                let (cell_width, cell_height) =
                    scaled_cells(display.cell_width, display.cell_height, pane.capture_scale);
                let vivid_cells = vivid_cell_size(cell_width, cell_height);
                let metrics = (content.width, content.height, vivid_cells.0, vivid_cells.1);
                let dimensions_changed = pane.terminal.rows() != rows as usize
                    || pane.terminal.cols() != columns as usize;
                let metrics_changed = pane.vivid_metrics != Some(metrics);
                if dimensions_changed {
                    pane.terminal.resize(rows as usize, columns as usize);
                    pane.screen_sequence = pane.screen_sequence.wrapping_add(1);
                    pane.last_screen_change = Instant::now();
                    pane.screen_changes.push_back(ScreenChange {
                        sequence: pane.screen_sequence,
                        rows: None,
                        at: pane.last_screen_change,
                    });
                    while pane.screen_changes.len() > SCREEN_CHANGE_HISTORY {
                        pane.screen_changes.pop_front();
                    }
                    resized_panes = resized_panes.wrapping_add(1);
                }
                if dimensions_changed || metrics_changed {
                    let pixel_width = u32::from(columns)
                        .checked_mul(u32::from(cell_width))
                        .and_then(|value| u16::try_from(value).ok())
                        .unwrap_or(0);
                    let pixel_height = u32::from(rows)
                        .checked_mul(u32::from(cell_height))
                        .and_then(|value| u16::try_from(value).ok())
                        .unwrap_or(0);
                    if pane
                        .control
                        .resize_with_pixels(columns, rows, pixel_width, pixel_height)
                        .is_err()
                    {
                        resize_failures.push(projection.pane_id);
                    }
                }
                if metrics_changed {
                    self.vivid.update_metrics(
                        projection.pane_id,
                        content.width,
                        content.height,
                        vivid_cells,
                    );
                    pane.vivid_metrics = Some(metrics);
                }
            }
        }
        self.session_sequence = self.session_sequence.wrapping_add(resized_panes);
        resize_failures.sort_unstable();
        resize_failures.dedup();
        for pane in resize_failures {
            // A resize the PTY refused leaves the pane at its previous size; it is not evidence
            // that the program behind it died. Only its exit closes a pane, so a window the user
            // can drag back open never costs them a shell.
            self.status(&format!("pane {pane} PTY resize failed"));
        }
    }

    pub(super) fn render(&mut self) {
        self.flush_plugin_state_events();
        if std::mem::take(&mut self.force_full) {
            for client in self.clients.values_mut() {
                client.force_full = true;
            }
        }
        // A client whose frames are queued but not yet displayed keeps its change pending:
        // producing more cannot make that terminal any more current, and the extra bytes compete
        // with media on the same connection. Every other client is rendered regardless, so one
        // slow remote viewer never holds back the rest.
        let due = self
            .clients
            .values()
            .filter(|client| client.render_pending && !client.render_blocked())
            .map(|client| client.id)
            .collect::<Vec<_>>();
        if !due.is_empty() {
            // Put the media projection on the ordered presenter stream before the terminal frame
            // that exposes the new tab. The client can then reconcile the retained scene
            // concurrently with terminal painting instead of always showing pane text first and
            // the image later.
            self.sync_media(false);
        }
        for id in due {
            self.render_client(id);
        }
        // A blocked client waits for its acknowledgement, which re-arms the render; it must not
        // keep the loop waking at the render interval while a stalled terminal reads nothing.
        self.pending_render = self
            .clients
            .values()
            .any(|client| client.render_pending && !client.render_blocked());
    }

    /// Compose, diff, and send one client's frame.
    pub(super) fn render_client(&mut self, id: u64) {
        let Some(client) = self.clients.get(&id) else {
            return;
        };
        let display = client.display;
        // Graphics bytes reach only the presenter; every other terminal gets blanked placeholders.
        let kitty_graphics = client.kitty_graphics && self.presenter == Some(id);
        let with_ui = self.ui_visible_to(id);
        let mut screen = self.compose_screen(display, with_ui);
        let status_tab_targets = std::mem::take(&mut self.status_tab_targets);
        let sidebar_targets = std::mem::take(&mut self.sidebar_targets);
        if !kitty_graphics {
            screen.suppress_kitty_placeholders();
        }
        let mut kitty_prefix = if kitty_graphics {
            self.kitty_transfers.drain_pending()
        } else {
            Vec::new()
        };
        #[cfg(windows)]
        let focused_bracketed_paste = self
            .attached_focus_pane()
            .and_then(|pane_id| self.panes.get(&pane_id))
            .is_some_and(|pane| pane.terminal.modes().bracketed_paste);
        let session_sequence = self.session_sequence;
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        let frame_full = client.force_full || !kitty_prefix.is_empty();
        client.frame_id = client.frame_id.wrapping_add(1);
        // Mutated only by the Windows bracketed-paste prepend below.
        let mut bytes = ansi_diff(client.last_screen.as_ref(), &screen, frame_full);
        if !kitty_prefix.is_empty() {
            kitty_prefix.extend_from_slice(&bytes);
            bytes = kitty_prefix;
        }
        #[cfg(windows)]
        let bracketed_paste_transition =
            bracketed_paste_transition(client.outer_bracketed_paste, focused_bracketed_paste);
        #[cfg(windows)]
        if let Some(transition) = bracketed_paste_transition {
            prepend_bracketed_paste_transition(&mut bytes, transition);
        }
        let sent = crate::ipc::send_render_record(
            &client.writer,
            client.frame_id,
            session_sequence,
            frame_full,
            &bytes,
        )
        .is_ok();
        if !sent {
            // Dropping the client here is the only signal it gets, so say why. Previously a
            // failed frame silently detached the session while the client kept running against a
            // frozen outer scene, which is indistinguishable from a hang.
            self.detach_client(id, Some("frame delivery failed"));
            return;
        }
        client.last_screen = Some(screen);
        client.force_full = false;
        client.render_pending = false;
        client.status_tab_targets = status_tab_targets;
        client.sidebar_targets = sidebar_targets;
        client
            .frame_sequences
            .push_back((client.frame_id, session_sequence));
        while client.frame_sequences.len() > 1024 {
            client.frame_sequences.pop_front();
        }
        #[cfg(windows)]
        if bracketed_paste_transition.is_some() {
            client.outer_bracketed_paste = Some(focused_bracketed_paste);
        }
    }

    /// Draw the session as one client sees it: the shared layout at its origin, and the tab list
    /// and prompts at the edges of this client's own terminal. `with_ui` draws the transient UI,
    /// which only its owner sees.
    pub(super) fn compose_screen(
        &mut self,
        display: DisplayMetrics,
        with_ui: bool,
    ) -> ScreenBuffer {
        let theme = self.config.resolved_theme();
        let session_view = self.direct_pane().is_none();
        let mut screen = ScreenBuffer::new(display.columns, display.rows);
        let area = self.content_area();
        let projections = self.attached_projections(area);
        if !projections.is_empty() {
            // Composition follows the ordered projection list; later projections overwrite
            // earlier frames and cells, and the status row is drawn last.
            let sync_input = session_view && self.active_tab().is_some_and(|tab| tab.sync_input);
            let mut cursor: Option<((u16, u16), usize)> = None;
            for (index, projection) in projections.iter().enumerate() {
                let Some(pane) = self.panes.get(&projection.pane_id) else {
                    continue;
                };
                let active = projection.focused;
                let title = pane
                    .terminal
                    .title()
                    .map_or_else(|| format!("pane {}", pane.id), ToOwned::to_owned);
                let copy_suffix = pane.copy.as_ref().map_or("", |_| " [copy]");
                let search_suffix = pane
                    .copy
                    .as_ref()
                    .and_then(|copy| copy.search.as_ref())
                    .filter(|search| !search.query.is_empty())
                    .map_or_else(String::new, |search| format!(" [search: {}]", search.query));
                let sync_suffix = if sync_input { " [sync]" } else { "" };
                let pin_suffix = if projection.layer == PaneLayer::Pinned {
                    " [pin]"
                } else {
                    ""
                };
                // A held pane outlives its process; say so, or it looks like a live shell that
                // has stopped responding.
                let exit_suffix = pane.exit_status.map_or("", |_| " [exited]");
                if session_view {
                    screen.draw_frame(
                        projection.outer,
                        &format!(
                            " {title}{copy_suffix}{search_suffix}{sync_suffix}{pin_suffix}{exit_suffix} "
                        ),
                        theme.frame(active),
                    );
                }
                let content = projection.content;
                let offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
                screen.draw_terminal(content, &pane.terminal, offset);
                if !pane.transparent {
                    // Filling here rather than in a pass over the finished buffer is what keeps
                    // the projection order authoritative: a pane drawn later still overwrites an
                    // opaque pane beneath it. The frame is included so the border does not stay
                    // see-through around solid content.
                    screen.fill_default_background(projection.outer, theme.pane_background);
                }
                if let Some(copy) = &pane.copy {
                    for found in &copy.matches {
                        let row = found.line + copy.offset as isize;
                        if row < 0 || row >= usize::from(content.height) as isize {
                            continue;
                        }
                        let start = found.start_column.min(usize::from(content.width));
                        let width = found
                            .end_column
                            .saturating_sub(found.start_column)
                            .min(usize::from(content.width).saturating_sub(start));
                        let style = if copy.current == Some(*found) {
                            theme.search_current()
                        } else {
                            theme.search_match()
                        };
                        screen.restyle(
                            content.x.saturating_add(start as u16),
                            content.y.saturating_add(row as u16),
                            width as u16,
                            style,
                        );
                    }
                }
                if let Some(selection) = pane.mouse_selection {
                    for (row, column, width) in mouse_selection_runs(
                        &pane.terminal,
                        selection,
                        offset,
                        usize::from(content.width),
                        usize::from(content.height),
                    ) {
                        screen.invert(
                            content.x.saturating_add(column as u16),
                            content.y.saturating_add(row as u16),
                            width as u16,
                        );
                    }
                }
                if self.config.hyperlinks.enabled {
                    // Only the pane under the pointer gets a hovered link, so an identically
                    // targeted link in another pane stays at its resting mark.
                    let hovered = self
                        .hovered_link
                        .as_ref()
                        .filter(|hovered| hovered.pane == projection.pane_id)
                        .map(|hovered| &hovered.link);
                    let resting = self
                        .config
                        .hyperlinks
                        .persistent_style
                        .then(|| LinkStyle::resting(theme.hyperlink));
                    screen.style_links(
                        content,
                        hovered,
                        resting,
                        LinkStyle::hovered(theme.hyperlink),
                    );
                }
                if active {
                    if let Some(copy) = &pane.copy {
                        cursor = Some((
                            (
                                content.x
                                    + copy.column.min(content.width.saturating_sub(1) as usize)
                                        as u16,
                                content.y
                                    + copy.row.min(content.height.saturating_sub(1) as usize)
                                        as u16,
                            ),
                            index,
                        ));
                    } else if pane.terminal.modes().cursor_visible {
                        let (row, column) = pane.terminal.cursor();
                        cursor = Some((
                            (
                                content.x
                                    + column.min(content.width.saturating_sub(1) as usize) as u16,
                                content.y
                                    + row.min(content.height.saturating_sub(1) as usize) as u16,
                            ),
                            index,
                        ));
                    }
                }
            }
            // The cursor comes only from the focused projection and hides when a later
            // projection covers its cell.
            screen.cursor = cursor.and_then(|((x, y), index)| {
                projections[index + 1..]
                    .iter()
                    .all(|later| !later.outer.contains(x, y))
                    .then_some((x, y))
            });
        }
        if with_ui && session_view && self.agent_navigator.is_some() {
            self.draw_agent_navigator(&mut screen, theme);
        } else if with_ui && session_view && self.tab_navigator.is_some() {
            self.draw_tab_navigator(&mut screen, theme);
        } else if with_ui && session_view && self.pane_menu.is_some() {
            self.draw_pane_menu(&mut screen, theme);
        }
        self.status_tab_targets.clear();
        self.sidebar_targets.clear();
        let view = if session_view {
            self.tab_view
        } else {
            TabView::Hidden
        };
        if session_view && screen.rows > 0 {
            let rename_prompt = self
                .tab_rename
                .as_ref()
                .filter(|_| with_ui)
                .and_then(|rename| {
                    self.tabs
                        .iter()
                        .position(|tab| tab.id == rename.tab_id)
                        .map(|index| format!("rename tab {}: {}", index + 1, rename.value))
                });
            let close_prompt = self
                .close_pane_confirmation
                .filter(|_| with_ui)
                .map(|confirmation| format!("kill pane {}? (y/n)", confirmation.pane_id));
            let save_prompt = self
                .save_layout_prompt
                .as_ref()
                .filter(|_| with_ui)
                .map(|prompt| match &prompt.stage {
                    SaveLayoutStage::Editing { value } => format!("save layout: {value}"),
                    SaveLayoutStage::Confirm { path } => {
                        format!("overwrite {}? (y/n)", path.display())
                    }
                });
            let search_prompt = self.active_tab().and_then(|tab| {
                self.panes
                    .get(&tab.focused)
                    .and_then(|pane| pane.copy.as_ref())
                    .and_then(|copy| copy.search.as_ref())
                    .and_then(|search| {
                        search.prompt.as_ref().map(|prompt| {
                            let leader = match search.direction {
                                SearchDirection::Forward => '/',
                                SearchDirection::Backward => '?',
                            };
                            format!("{leader}{prompt}")
                        })
                    })
            });
            let prompt = rename_prompt
                .or(close_prompt)
                .or(save_prompt)
                .or(search_prompt);
            let prompt_active = prompt.is_some();
            let sidebar = view.sidebar_text_rect(screen.columns, screen.rows);
            let message_width = sidebar.map_or(screen.columns, |rect| rect.width);
            let message = prompt
                .or_else(|| self.active_status_notice().map(ToOwned::to_owned))
                // Below notices on purpose: a notice reports something the user cannot recover
                // once it scrolls past, while the hover preview returns the moment they point at
                // the link again.
                .or_else(|| {
                    self.hovered_link
                        .as_ref()
                        .map(|hovered| hyperlink_status_text(&hovered.link.uri, message_width))
                });
            let mic_prefix = if let Some((_, _, pane, last_packet)) = self.microphone_recipient
                && self.bridge_instance_id.is_some()
                && last_packet.elapsed() <= MICROPHONE_ACTIVE_WINDOW
            {
                Some(format!("MIC pane {pane} | "))
            } else {
                None
            };
            let style = theme.status();
            let prompt_cursor = !(with_ui
                && (self.agent_navigator.is_some() || self.tab_navigator.is_some()))
                && prompt_active;
            if let Some(row) = view.bar_row(screen.rows) {
                let status = message.unwrap_or_else(|| {
                    let (text, targets) =
                        tab_status_layout(&self.tabs, self.active_tab, screen.columns);
                    self.status_tab_targets = targets;
                    text
                });
                let status = if let Some(prefix) = mic_prefix {
                    for (range, _) in &mut self.status_tab_targets {
                        range.start += prefix.len();
                        range.end = (range.end + prefix.len()).min(usize::from(screen.columns));
                    }
                    format!("{prefix}{status}")
                } else {
                    status
                };
                if theme.status_fill {
                    // Paint the whole row first: a status background that stops where the text
                    // does reads as a rendering bug rather than a bar.
                    screen.fill_row(row, style);
                }
                screen.draw_text(0, row, &status, style);
                if prompt_cursor {
                    screen.cursor = Some((
                        status.chars().count().min(usize::from(screen.columns - 1)) as u16,
                        row,
                    ));
                }
            } else if let Some(sidebar) = sidebar {
                let message = match (mic_prefix, message) {
                    (Some(prefix), Some(message)) => Some(format!("{prefix}{message}")),
                    (Some(prefix), None) => Some(prefix.trim_end_matches(" | ").to_owned()),
                    (None, message) => message,
                };
                // A prompt is being typed at its end, so a narrow sidebar shows its tail and
                // keeps one cell for the cursor.
                let message = message.map(|message| {
                    if prompt_active {
                        tail_chars(&message, usize::from(sidebar.width.saturating_sub(1)))
                    } else {
                        message
                    }
                });
                self.draw_sidebar(&mut screen, theme, message.as_deref());
                if prompt_cursor && let Some(message) = &message {
                    screen.cursor =
                        Some((sidebar.x + message.chars().count() as u16, screen.rows - 1));
                }
            } else if let Some(prompt) = message.filter(|_| prompt_active) {
                // With no tab list there is no status row, but a prompt still has to be seen while
                // it is typed: draw it over the bottom row until it closes.
                let row = screen.rows - 1;
                let prompt = tail_chars(&prompt, usize::from(screen.columns - 1));
                screen.fill_row(row, style);
                screen.draw_text(0, row, &prompt, style);
                if prompt_cursor {
                    screen.cursor = Some((prompt.chars().count() as u16, row));
                }
            }
        }
        screen
    }

    pub(super) fn sync_media(&mut self, force: bool) {
        self.sync_media_inner(force, None);
    }

    pub(super) fn sync_media_before_delivery(&mut self, source: vivid_sdk::presenter::SourceKey) {
        self.sync_media_inner(false, Some(source));
    }

    pub(super) fn sync_shared_visuals(
        &mut self,
        force: bool,
        key: MediaProjectionKey,
        snapshot: &vivid_sdk::presenter::ProjectionSnapshot,
        surfaces: &[BridgeSurface],
        tracks: &[BridgeSource],
        nodes: &[BridgeNode],
    ) {
        let retained = snapshot
            .sources
            .iter()
            .filter(|source| {
                source.live
                    && matches!(
                        source.descriptor,
                        vivid_sdk::presenter::SourceDescriptor::Image(_)
                            | vivid_sdk::presenter::SourceDescriptor::Raster(_)
                    )
            })
            .collect::<Vec<_>>();
        let keys = retained
            .iter()
            .map(|source| bridge_key(source.key))
            .collect::<HashSet<_>>();
        let surface_keys = keys
            .iter()
            .map(|key| BridgeSurfaceKey {
                producer: key.producer,
                context: key.context,
                surface: key.surface,
            })
            .collect::<HashSet<_>>();
        self.shared_visual_sources.clone_from(&keys);
        for client in self
            .clients
            .values_mut()
            .filter(|client| client.vivid && client.media_enabled)
        {
            let state = &mut client.shared_visuals;
            state.applied_sources.retain(|source, reset| {
                retained.iter().any(|current| {
                    bridge_key(current.key) == *source && current.decoder_reset_serial == *reset
                })
            });
            let primary = Some(client.id) == self.presenter;
            if !primary && state.pending_revision.is_some() {
                continue;
            }
            state.sent.retain(|source, _| keys.contains(source));
            state.inflight.retain(|source| keys.contains(source));
            if !primary && (force || state.last_projection != Some(key)) {
                let Some(revision) = state.revision.checked_add(1) else {
                    continue;
                };
                let message = ServerMessage::MediaSnapshot {
                    microphones: Vec::new(),
                    revision,
                    surfaces: surfaces
                        .iter()
                        .filter(|surface| surface_keys.contains(&surface.key))
                        .cloned()
                        .collect(),
                    tracks: tracks
                        .iter()
                        .filter(|track| keys.contains(&track.key))
                        .cloned()
                        .collect(),
                    nodes: nodes
                        .iter()
                        .filter(|node| surface_keys.contains(&node.surface))
                        .cloned()
                        .collect(),
                    videos_needing_keyframes: Vec::new(),
                };
                if crate::ipc::send(&client.writer, &message).is_err() {
                    continue;
                }
                state.revision = revision;
                state.pending_revision = Some(revision);
                state.pending_sources = retained
                    .iter()
                    .map(|source| (bridge_key(source.key), source.decoder_reset_serial))
                    .collect();
                state.last_projection = Some(key);
            }
            for source in &retained {
                let source_key = bridge_key(source.key);
                let version = (
                    source.decoder_reset_serial,
                    source.last_inner_record_sequence,
                );
                if state.inflight.contains(&source_key)
                    || state.sent.get(&source_key) == Some(&version)
                {
                    continue;
                }
                let sent = match &source.descriptor {
                    vivid_sdk::presenter::SourceDescriptor::Raster(_) => source
                        .retained_raster
                        .as_ref()
                        .and_then(|raster| retained_raster_body(raster).ok())
                        .is_some_and(|body| {
                            send_media_body(
                                &client.writer,
                                0,
                                source_key,
                                vivid_protocol::messages::RASTER_FRAME,
                                &body,
                            )
                        }),
                    vivid_sdk::presenter::SourceDescriptor::Image(_) => {
                        source.retained.as_ref().is_some_and(|body| {
                            send_media_body(
                                &client.writer,
                                0,
                                source_key,
                                vivid_protocol::messages::IMAGE_DATA,
                                body,
                            )
                        })
                    }
                    _ => false,
                };
                if sent {
                    state.sent.insert(source_key, version);
                    state.inflight.insert(source_key);
                }
            }
        }
        if self.presenter_client().is_none_or(|client| !client.vivid) {
            let applied = self
                .clients
                .values()
                .filter(|client| client.vivid && client.media_enabled)
                .flat_map(|client| {
                    client
                        .shared_visuals
                        .applied_sources
                        .iter()
                        .map(|(key, reset)| (*key, *reset))
                })
                .collect();
            self.vivid.activate_bridge_projection_at_resets(&applied);
        }
    }

    pub(super) fn sync_media_inner(
        &mut self,
        force: bool,
        live_delivery_source: Option<vivid_sdk::presenter::SourceKey>,
    ) {
        let media_revision = self.vivid.revision();
        if media_revision != self.last_plugin_media_revision {
            self.last_plugin_media_revision = media_revision;
            self.queue_plugin_state_event(
                "media.changed",
                "media".into(),
                serde_json::json!({"media_revision": media_revision}),
                None,
            );
            self.schedule_render();
        }
        // Only the selected presenter activates timed ingress. Retained subscribers can inspect
        // and project their own content while that role is vacant.
        if !self
            .clients
            .values()
            .any(|client| client.vivid && client.media_enabled)
        {
            self.vivid.deactivate_bridge();
            self.shared_visual_sources.clear();
            return;
        }
        let writer = self
            .presenter_client()
            .filter(|client| client.vivid)
            .map(|client| Arc::clone(&client.writer));
        let projections = self.attached_projections(self.content_area());
        if projections.is_empty() {
            return;
        }
        let pane_priority = if self.direct_pane().is_some() {
            projections
                .iter()
                .map(|projection| projection.pane_id)
                .collect()
        } else {
            self.active_tab()
                .map(|tab| projection_pane_priority(tab, &projections))
                .unwrap_or_default()
        };
        let area = self.content_area();
        let panes = projections
            .iter()
            .map(|projection| projection.pane_id)
            .collect::<HashSet<_>>();
        let viewport_offsets = panes
            .iter()
            .filter_map(|pane_id| {
                let offset = self
                    .panes
                    .get(pane_id)
                    .and_then(|pane| pane.copy.as_ref())
                    .map_or(0, |copy| copy.offset);
                (offset != 0).then_some((*pane_id, offset))
            })
            .collect::<HashMap<_, _>>();
        let projection_key = MediaProjectionKey {
            virtual_revision: self.vivid.revision(),
            layout_revision: self.layout_revision,
        };
        if !should_sync_media(force, self.last_media_projection, projection_key) {
            return;
        }
        // Preparing a snapshot parks falling edges immediately but does not wake rising edges.
        // The matching BridgeApplied acknowledgement publishes those sources below.
        let mut snapshot = if writer.is_some() {
            self.vivid
                .prepare_projection_snapshot_with_viewports(&panes, &viewport_offsets)
        } else {
            self.vivid
                .inspect_projection_snapshot_with_viewports(&panes, &viewport_offsets)
        };
        let projection_key = MediaProjectionKey {
            virtual_revision: snapshot.revision,
            layout_revision: self.layout_revision,
        };
        let live_nodes = snapshot.live_nodes.iter().copied().collect::<HashSet<_>>();
        self.fragment_assignments
            .retain(|logical, _| live_nodes.contains(logical));
        let display = self.layout_display();
        let surfaces = snapshot
            .surfaces
            .iter()
            .map(|surface| BridgeSurface {
                overlay_layouts: surface.overlay_layouts.clone(),
                key: BridgeSurfaceKey {
                    producer: surface.producer,
                    context: surface.context,
                    surface: surface.surface,
                },
                logical_width: surface.logical_width,
                logical_height: surface.logical_height,
                capture_policy: surface.capture_policy,
                descriptor: BridgeSourceDescriptor {
                    role: surface.semantic_descriptor.role,
                    title: surface.semantic_descriptor.title.clone(),
                    content_revision: surface.semantic_descriptor.content_revision,
                    semantic_availability: surface.semantic_descriptor.semantic_availability,
                    locator: surface.semantic_descriptor.locator.clone(),
                },
                overlay_window: surface.overlay_window.and_then(|window| {
                    let projection = projections
                        .iter()
                        .find(|projection| projection.pane_id == surface.pane)?;
                    Some(project_overlay_window(window, projection.content, display))
                }),
            })
            .collect::<Vec<_>>();
        let sources = snapshot
            .sources
            .iter()
            .map(|source| BridgeSource {
                key: bridge_key(source.key),
                kind: bridge_source_kind(
                    source.key,
                    &source.descriptor,
                    source.raster_delta_operation_limit,
                ),
                decoder_reset_serial: source.decoder_reset_serial,
                live: source.live,
                active: source.active,
                audio_gain: source.audio_gain.map(vivid_sdk::AudioGain::raw),
                capture_policy: source.capture_policy,
                descriptor: source.semantic_descriptor.as_ref().map(|descriptor| {
                    BridgeSourceDescriptor {
                        role: descriptor.role,
                        title: descriptor.title.clone(),
                        content_revision: descriptor.content_revision,
                        semantic_availability: descriptor.semantic_availability,
                        locator: descriptor.locator.clone(),
                    }
                }),
                playing: source.playing,
                play_request: bridge_play_request(source.play_request),
                eos_epoch: source.eos_epoch,
                causation_id: source.causation_id,
            })
            .collect::<Vec<_>>();
        let pane_rank = pane_priority
            .iter()
            .enumerate()
            .map(|(rank, pane)| (*pane, rank))
            .collect::<HashMap<_, _>>();
        snapshot.nodes.sort_by(|left, right| {
            pane_rank
                .get(&left.pane)
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(&pane_rank.get(&right.pane).copied().unwrap_or(usize::MAX))
                .then_with(|| right.config.node.z_index.cmp(&left.config.node.z_index))
                .then_with(|| left.producer.cmp(&right.producer))
                .then_with(|| left.config.node.node_id.cmp(&right.config.node.node_id))
        });
        let mut nodes = Vec::new();
        let mut fragment_omissions = 0_usize;
        let mut arithmetic_omissions = 0_usize;
        let mut quota_omissions = 0_usize;
        for (logical_index, logical) in snapshot.nodes.iter().enumerate() {
            let Some(projection_index) = projections
                .iter()
                .position(|projection| projection.pane_id == logical.pane)
            else {
                continue;
            };
            let projection = projections[projection_index];
            let occluders = projections[projection_index + 1..]
                .iter()
                .filter_map(|higher| from_cells(higher.outer))
                .collect::<Vec<_>>();
            let logical_key = (logical.producer, logical.config.node.node_id);
            let projected =
                match project_logical_node(logical, projection.content, area, &occluders) {
                    Ok(projected) => projected,
                    Err(ProjectionIssue::FragmentLimit) => {
                        fragment_omissions += 1;
                        let _ = self
                            .fragment_assignments
                            .entry(logical_key)
                            .or_default()
                            .assign(&[]);
                        continue;
                    }
                    Err(ProjectionIssue::Arithmetic) => {
                        arithmetic_omissions += 1;
                        let _ = self
                            .fragment_assignments
                            .entry(logical_key)
                            .or_default()
                            .assign(&[]);
                        continue;
                    }
                };
            let fragment_rects = projected
                .fragments
                .iter()
                .map(|fragment| fragment.clip)
                .collect::<Vec<_>>();
            let Some(assignments) = self
                .fragment_assignments
                .entry(logical_key)
                .or_default()
                .assign(&fragment_rects)
            else {
                arithmetic_omissions += 1;
                continue;
            };
            if nodes.len().saturating_add(assignments.len()) > MAX_PROJECTED_NODES {
                quota_omissions = snapshot.nodes.len() - logical_index;
                break;
            }
            for ((fragment_id, clip), mut bridge) in assignments.into_iter().zip(
                projected
                    .fragments
                    .into_iter()
                    .map(|fragment| fragment.node),
            ) {
                bridge.fragment = fragment_id;
                bridge.clip = BridgeClipRect {
                    x: clip.x,
                    y: clip.y,
                    width: clip.width,
                    height: clip.height,
                };
                nodes.push(bridge);
            }
        }
        if (fragment_omissions != 0 || arithmetic_omissions != 0 || quota_omissions != 0)
            && self.last_projection_warning != Some(projection_key)
        {
            self.status(&format!(
                "media projection omitted nodes (fragment-limit:{fragment_omissions}, arithmetic:{arithmetic_omissions}, quota:{quota_omissions})"
            ));
            self.last_projection_warning = Some(projection_key);
        }
        let Some(writer) = writer else {
            self.sync_shared_visuals(
                force,
                projection_key,
                &snapshot,
                &surfaces,
                &sources,
                &nodes,
            );
            self.last_media_projection = Some(projection_key);
            return;
        };
        let videos_needing_keyframes = snapshot
            .videos_needing_keyframes
            .iter()
            .copied()
            .map(bridge_key)
            .collect();
        let projected_source_keys = sources
            .iter()
            .map(|source| source.key)
            .collect::<HashSet<_>>();
        let projected_decoder_reset_serials = sources
            .iter()
            .map(|source| (source.key, source.decoder_reset_serial))
            .collect::<HashMap<_, _>>();
        let retained_replay_candidates = snapshot
            .sources
            .iter()
            .filter(|source| {
                source.first_visible_presented
                    && (source.retained.is_some() || source.retained_raster.is_some())
            })
            .map(|source| bridge_key(source.key))
            .collect::<HashSet<_>>();
        let surface_count = u16::try_from(surfaces.len()).unwrap_or(u16::MAX);
        let track_count = u16::try_from(sources.len()).unwrap_or(u16::MAX);
        let node_count = u16::try_from(nodes.len()).unwrap_or(u16::MAX);
        let projection_revision = self.media_projection_revision.wrapping_add(1);
        if crate::ipc::send(
            &writer,
            &ServerMessage::MediaSnapshot {
                microphones: self.vivid.microphone_requests(),
                revision: projection_revision,
                surfaces: surfaces.clone(),
                tracks: sources.clone(),
                nodes: nodes.clone(),
                videos_needing_keyframes,
            },
        )
        .is_err()
        {
            return;
        }
        let still_applied = self
            .traced_projected_sources
            .intersection(&projected_source_keys)
            .copied()
            .collect::<HashSet<_>>();
        self.record_projection_sources(&still_applied, projection_key.virtual_revision);
        self.pending_media_projections.insert(
            projection_revision,
            PendingMediaProjection {
                sources: projected_source_keys,
                decoder_reset_serials: projected_decoder_reset_serials,
                retained_replay_candidates,
                retained_replays: HashSet::new(),
                gateway_revision: projection_key.virtual_revision,
            },
        );
        while self.pending_media_projections.len() > MAX_PENDING_MEDIA_PROJECTIONS {
            self.pending_media_projections.pop_first();
        }
        self.record_media_trace(
            None,
            self.bridge_instance_id,
            None,
            MediaTraceKind::ProjectionSubmitted {
                virtual_revision: projection_key.virtual_revision,
                surface_count,
                track_count,
                node_count,
            },
        );
        for source in &snapshot.sources {
            if source.live
                && matches!(
                    source.descriptor,
                    vivid_sdk::presenter::SourceDescriptor::Image(_)
                        | vivid_sdk::presenter::SourceDescriptor::Raster(_)
                )
            {
                continue;
            }
            let source_key = bridge_key(source.key);
            let forced_replay = self.retained_replay_requests.contains(&source_key);
            if !should_replay_retained(
                source.key,
                live_delivery_source,
                source.first_visible_presented,
                self.outer_attachment_generations.contains_key(&source_key),
                forced_replay,
            ) {
                // The MediaEvent that triggered this projection sync follows immediately. Do not
                // also send the same retained raster body as delivery 0: the outer source would
                // observe the same frame ID twice and reject the live update. Likewise, an
                // already-presented retained body needs no IPC replay while its outer attachment
                // remains resident.
                continue;
            }
            let sent = match source.descriptor {
                vivid_sdk::presenter::SourceDescriptor::Raster(_) => {
                    let Some(raster) = &source.retained_raster else {
                        continue;
                    };
                    let Ok(body) = retained_raster_body(raster) else {
                        continue;
                    };
                    send_media_body(
                        &writer,
                        0,
                        source_key,
                        vivid_protocol::messages::RASTER_FRAME,
                        &body,
                    )
                }
                vivid_sdk::presenter::SourceDescriptor::Image(_) => {
                    source.retained.as_ref().is_some_and(|body| {
                        send_media_body(
                            &writer,
                            0,
                            source_key,
                            vivid_protocol::messages::IMAGE_DATA,
                            body,
                        )
                    })
                }
                vivid_sdk::presenter::SourceDescriptor::VectorScene(_) => source
                    .retained_vector
                    .iter()
                    .all(|(kind, body)| send_media_body(&writer, 0, source_key, *kind, body)),
                _ => continue,
            };
            if !sent {
                return;
            }
            {
                if let Some(pending) = self.pending_media_projections.get_mut(&projection_revision)
                {
                    pending.retained_replays.insert(source_key);
                }
                if forced_replay {
                    self.retained_replay_requests.remove(&source_key);
                    self.retained_replay_inflight.insert(source_key);
                }
            }
        }
        self.sync_shared_visuals(
            force,
            projection_key,
            &snapshot,
            &surfaces,
            &sources,
            &nodes,
        );
        self.last_media_projection = Some(projection_key);
        self.media_projection_revision = projection_revision;
    }

    pub(super) fn schedule_render(&mut self) {
        self.pending_render = true;
        for client in self.clients.values_mut() {
            client.render_pending = true;
        }
    }

    /// Session-scoped relay counters for the media diagnostic surfaces.
    ///
    /// `delivery` is filled in by the virtual presenter, which owns those counters.
    pub(super) fn relay_metrics(&self) -> crate::metrics::RelayMetrics {
        crate::metrics::RelayMetrics {
            ipc: self
                .client_ipc
                .as_ref()
                .map(|counters| counters.snapshot())
                .unwrap_or_default(),
            delivery: crate::metrics::DeliveryMetrics::default(),
            bridge: self.bridge_metrics,
        }
    }

    pub(super) fn outer_media_projection(&self) -> vivid_sdk::presenter::OuterMediaProjection<'_> {
        if self.presenter_client().is_none_or(|client| !client.vivid)
            && let Some(client) = self
                .clients
                .values()
                .filter(|client| client.vivid && client.media_enabled)
                .max_by_key(|client| client.activity)
        {
            let state = &client.shared_visuals;
            return vivid_sdk::presenter::OuterMediaProjection {
                compatibility_revision: self.outer_projection_revision,
                apply_sequence: self.outer_apply_sequence,
                bridge_instance_id: state.bridge_instance,
                bridge_local_revision: state.outer_revision,
                attachment_generations: &state.attachment_generations,
            };
        }
        vivid_sdk::presenter::OuterMediaProjection {
            compatibility_revision: self.outer_projection_revision,
            apply_sequence: self.outer_apply_sequence,
            bridge_instance_id: self.bridge_instance_id,
            bridge_local_revision: self.bridge_local_revision,
            attachment_generations: &self.outer_attachment_generations,
        }
    }

    pub(super) fn record_media_trace(
        &mut self,
        source: Option<BridgeSourceKey>,
        bridge_instance_id: Option<u64>,
        origin_monotonic_us: Option<u64>,
        kind: MediaTraceKind,
    ) {
        let pane = source.and_then(|source| self.vivid.pane_for_source(source));
        self.media_trace.push(
            &self.session_instance,
            pane,
            source,
            bridge_instance_id,
            origin_monotonic_us,
            kind,
        );
    }

    pub(super) fn record_delivery_result(&mut self, delivery_id: u64, delivered: bool) {
        if let Some((source, bridge_instance_id, epoch, pts_us)) =
            self.traced_recovery_deliveries.remove(&delivery_id)
        {
            self.record_media_trace(
                Some(source),
                bridge_instance_id,
                None,
                MediaTraceKind::KeyframeDelivery {
                    delivery_id,
                    delivered,
                    epoch,
                    pts_us,
                },
            );
        } else if !delivered {
            self.record_media_trace(
                None,
                self.bridge_instance_id,
                None,
                MediaTraceKind::DeliveryFailed { delivery_id },
            );
        }
    }

    pub(super) fn record_projection_sources(
        &mut self,
        current: &HashSet<BridgeSourceKey>,
        virtual_revision: u64,
    ) {
        let hidden = self
            .traced_projected_sources
            .difference(current)
            .copied()
            .collect::<Vec<_>>();
        let visible = current
            .difference(&self.traced_projected_sources)
            .copied()
            .collect::<Vec<_>>();
        for source in hidden {
            self.record_media_trace(
                Some(source),
                self.bridge_instance_id,
                None,
                MediaTraceKind::TrackVisibility {
                    visible: false,
                    virtual_revision,
                },
            );
        }
        for source in visible {
            self.record_media_trace(
                Some(source),
                self.bridge_instance_id,
                None,
                MediaTraceKind::TrackVisibility {
                    visible: true,
                    virtual_revision,
                },
            );
        }
        self.traced_projected_sources.clone_from(current);
    }
}

fn bridge_play_request(request: vivid_sdk::presenter::PlayRequest) -> BridgePlayRequest {
    BridgePlayRequest {
        start_pts_us: request.start_pts_us,
        minimum_buffer_us: request.minimum_buffer_us,
        maximum_latency_us: request.maximum_latency_us,
        rate_32_32: request.rate_32_32,
        late_policy: request.late_policy,
        loop_count: request.loop_count,
        start_policy: request.start_policy,
        hold_serial: request.hold_serial,
    }
}

fn bridge_source_kind(
    key: vivid_sdk::presenter::SourceKey,
    descriptor: &vivid_sdk::presenter::SourceDescriptor,
    raster_delta_operation_limit: Option<u32>,
) -> BridgeSourceKind {
    match descriptor {
        vivid_sdk::presenter::SourceDescriptor::Raster(config) => BridgeSourceKind::Raster {
            width: config.width,
            height: config.height,
            alpha_mode: config.alpha_mode,
            compression_mode: u64::from(config.zstd_enabled),
            delta_operation_limit: raster_delta_operation_limit,
        },
        vivid_sdk::presenter::SourceDescriptor::Image(config) => BridgeSourceKind::Image {
            encoding: config.encoding,
            width: config.width,
            height: config.height,
            encoded_length: config.encoded_length,
            sha256: config.sha256,
        },
        vivid_sdk::presenter::SourceDescriptor::Video(config) => BridgeSourceKind::Video {
            codec: config.codec.clone(),
            packetization: config.packetization.clone(),
            extradata: config.extradata.clone(),
            width: config.coded_width,
            height: config.coded_height,
            profile: config.profile,
            level: config.level,
            bitrate: u64::from(config.maximum_access_unit_bytes)
                .saturating_mul(8)
                .saturating_mul(240),
            color_primaries: config.color_primaries,
            transfer: config.transfer,
            matrix: config.matrix,
            range: config.signal_range,
            sar_num: u32::try_from(config.aspect_numerator).unwrap_or(u32::MAX),
            sar_den: u32::try_from(config.aspect_denominator).unwrap_or(u32::MAX),
            max_access_unit_bytes: config.maximum_access_unit_bytes,
            codec_string: config.codec_string.clone(),
            decoder_config: config.decoder_configuration.clone(),
        },
        vivid_sdk::presenter::SourceDescriptor::Audio(config) => BridgeSourceKind::Audio {
            linked_video: config.linked_video_source_id.map(|source| BridgeSourceKey {
                producer: key.producer,
                context: key.context,
                surface: key.surface,
                track: source,
            }),
            codec: config.codec.clone(),
            packetization: config.packetization.clone(),
            extradata: config.extradata.clone(),
            sample_rate: config.sample_rate,
            channels: config.channels,
            channel_mask: config.channel_mask,
            bitrate: config.bitrate,
            max_access_unit_bytes: config.max_access_unit_bytes,
            codec_string: config.codec_string.clone(),
        },
        vivid_sdk::presenter::SourceDescriptor::VectorScene(config) => {
            BridgeSourceKind::VectorScene {
                width: config.width,
                height: config.height,
                maximum_scene_bytes: config.maximum_scene_bytes,
            }
        }
    }
}

/// The last `width` characters of `text`.
fn tail_chars(text: &str, width: usize) -> String {
    let skip = text.chars().count().saturating_sub(width);
    text.chars().skip(skip).collect()
}
