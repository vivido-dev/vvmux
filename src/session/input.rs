//! Keyboard, mouse, selection, link, and paste input routing to panes.

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
    pub(super) fn input(&mut self, bytes: Vec<u8>) {
        if let Some(pane_id) = self.direct_pane() {
            let failure = self
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| queue_key_input(pane, &bytes));
            self.report_input_failure(pane_id, failure);
            return;
        }
        // Keys from a client that does not own the open menu or prompt skip it and reach the
        // focused pane, and leave the owner's pointer state alone.
        let owns_ui = self.ui_takes_current_input();
        if owns_ui && self.agent_navigator.is_some() {
            self.agent_navigator_input(&key_presses(&bytes));
            return;
        }
        if owns_ui && self.tab_navigator.is_some() {
            self.tab_navigator_input(&key_presses(&bytes));
            return;
        }
        if owns_ui && self.pane_menu.is_some() {
            self.pane_menu_input(&key_presses(&bytes));
            return;
        }
        if owns_ui && self.tab_rename.is_some() {
            self.tab_rename_input(&key_presses(&bytes));
            return;
        }
        if owns_ui && self.close_pane_confirmation.is_some() {
            self.close_pane_confirmation_input(&key_presses(&bytes));
            return;
        }
        if owns_ui && self.save_layout_prompt.is_some() {
            self.save_layout_prompt_input(&key_presses(&bytes));
            return;
        }
        if owns_ui && self.invalidate_mouse_selection_state() {
            self.schedule_render();
        }
        let Some(pane_id) = self.active_tab().map(|tab| tab.focused) else {
            return;
        };
        if self
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.copy.is_some())
        {
            self.copy_input(pane_id, &key_presses(&bytes));
        } else if self.active_tab().is_some_and(|tab| tab.sync_input) {
            self.broadcast_input(&bytes);
        } else if let Some(pane) = self.panes.get_mut(&pane_id) {
            let failure = queue_key_input(pane, &bytes);
            self.report_input_failure(pane_id, failure);
        }
    }

    pub(super) fn broadcast_input(&mut self, bytes: &[u8]) {
        let targets = self.active_tab().map_or_else(Vec::new, |tab| {
            sync_targets(tab, &|pane_id| {
                self.panes
                    .get(&pane_id)
                    .is_some_and(|pane| pane.copy.is_some() || !pane_role_accepts_sync(&pane.role))
            })
        });
        let failures = queue_input_targets(&mut self.panes, &targets, bytes);
        for (pane_id, failure) in failures {
            self.report_input_failure(pane_id, Some(failure));
        }
    }

    pub(super) fn clear_retained_mouse_selections(&mut self) -> bool {
        let mut changed = false;
        for pane in self.panes.values_mut() {
            changed |= pane.mouse_selection.take().is_some();
        }
        changed
    }

    pub(super) fn invalidate_mouse_selection_state(&mut self) -> bool {
        self.mouse_selection_drag = None;
        self.mouse_click_tracker = None;
        // Everything the pointer was resolved against is now stale — the client detached, the
        // display resized, or the pane scrolled under a stationary pointer. A wheel scroll is the
        // case that matters most: no motion event follows it, so a hover kept here would stay
        // painted on whichever link happened to scroll into that cell.
        //
        // Deliberately not tied to pane output, which no longer invalidates selections wholesale:
        // clearing on every PTY chunk would make a link in any actively-printing pane unhoverable
        // (and erased selections during any continuous redraw — see
        // `adjust_mouse_selection_after_pane_output` for the replacement).
        self.hovered_link = None;
        self.clear_retained_mouse_selections()
    }

    /// Fold one batch of pane output into the pane's mouse-selection state.
    ///
    /// The selection used to be invalidated on every PTY output chunk, which erased it during any
    /// continuous redraw. It now survives redraws and rotates with content that scrolls into
    /// scrollback; the same transform keeps a drag anchor and a multi-click cell on their text, so
    /// a selection in progress in a busy pane finishes on what was selected. Rendering is left to
    /// the caller — pane output already schedules one.
    pub(super) fn adjust_mouse_selection_after_pane_output(
        &mut self,
        pane_id: PaneId,
        events: &[TerminalEvent],
        screen_switched: bool,
        history_len: usize,
    ) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            let adjusted = pane_mouse_selection_after_output(
                pane.mouse_selection,
                events,
                screen_switched,
                history_len,
            );
            pane.mouse_selection = adjusted;
        }
        if let Some(drag) = self
            .mouse_selection_drag
            .take()
            .filter(|drag| drag.pane == pane_id)
        {
            let anchor = MouseSelection {
                start: drag.start,
                end: drag.start,
                mode: drag.mode,
            };
            match pane_mouse_selection_after_output(
                Some(anchor),
                events,
                screen_switched,
                history_len,
            ) {
                Some(anchor) => {
                    self.mouse_selection_drag = Some(MouseSelectionDrag {
                        start: anchor.start,
                        ..drag
                    });
                }
                None => self.mouse_click_tracker = None,
            }
        }
        if let Some(click) = self
            .mouse_click_tracker
            .take()
            .filter(|click| click.pane == pane_id)
        {
            let cell = MouseSelection {
                start: click.cell,
                end: click.cell,
                mode: MouseSelectionMode::Character,
            };
            if let Some(cell) =
                pane_mouse_selection_after_output(Some(cell), events, screen_switched, history_len)
            {
                self.mouse_click_tracker = Some(MouseClickTracker {
                    cell: cell.start,
                    ..click
                });
            }
        }
    }

    pub(super) fn invalidate_mouse_selection_for_pane(&mut self, pane_id: PaneId) -> bool {
        if self
            .mouse_selection_drag
            .is_some_and(|drag| drag.pane == pane_id)
        {
            self.mouse_selection_drag = None;
        }
        if self
            .mouse_click_tracker
            .is_some_and(|click| click.pane == pane_id)
        {
            self.mouse_click_tracker = None;
        }
        self.panes
            .get_mut(&pane_id)
            .and_then(|pane| pane.mouse_selection.take())
            .is_some()
    }

    pub(super) fn begin_mouse_selection(
        &mut self,
        pane_id: PaneId,
        content: Rect,
        mouse: MouseEvent,
    ) {
        if !self.panes.contains_key(&pane_id) {
            return;
        }
        // Motion during a drag is consumed before it reaches hover tracking, so a hover left
        // standing here would stay painted for the whole drag. Activation does not depend on it:
        // `finish_mouse_selection` re-reads the link from the grid cell the press landed on.
        self.set_hovered_link(None);
        let Some(pane) = self.panes.get(&pane_id) else {
            return;
        };
        let display_offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
        let Some(cell) = mouse_selection_cell(content, mouse.x, mouse.y, display_offset) else {
            return;
        };
        let cell = normalize_mouse_selection_cell(&pane.terminal, cell);
        let click =
            MouseClickTracker::next(self.mouse_click_tracker, pane_id, cell, Instant::now());
        self.mouse_click_tracker = Some(click);
        let mode = match click.count {
            2 => MouseSelectionMode::Word,
            3 => MouseSelectionMode::Line,
            _ => MouseSelectionMode::Character,
        };
        self.mouse_selection_drag = Some(MouseSelectionDrag {
            pane: pane_id,
            content,
            display_offset,
            start: cell,
            mode,
            moved: false,
        });
        if mode != MouseSelectionMode::Character {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.mouse_selection = Some(MouseSelection {
                    start: cell,
                    end: cell,
                    mode,
                });
            }
            self.schedule_render();
        }
    }

    pub(super) fn update_mouse_selection(&mut self, mouse: MouseEvent) {
        let Some(mut drag) = self.mouse_selection_drag.take() else {
            return;
        };
        // A double click selects only its word; holding the second click does not extend it.
        if drag.mode == MouseSelectionMode::Word {
            self.mouse_selection_drag = Some(drag);
            return;
        }
        let Some(pane) = self.panes.get(&drag.pane) else {
            return;
        };
        let Some(end) = mouse_selection_cell(drag.content, mouse.x, mouse.y, drag.display_offset)
        else {
            return;
        };
        let end = normalize_mouse_selection_cell(&pane.terminal, end);
        drag.moved = true;
        if let Some(pane) = self.panes.get_mut(&drag.pane) {
            pane.mouse_selection = Some(MouseSelection {
                start: drag.start,
                end,
                mode: drag.mode,
            });
        }
        self.mouse_selection_drag = Some(drag);
        self.schedule_render();
    }

    pub(super) fn finish_mouse_selection(&mut self, mouse: MouseEvent) {
        let Some(drag) = self.mouse_selection_drag.take() else {
            return;
        };
        let Some(pane) = self.panes.get(&drag.pane) else {
            return;
        };
        let Some(end) = mouse_selection_cell(drag.content, mouse.x, mouse.y, drag.display_offset)
        else {
            return;
        };
        let end = if drag.mode == MouseSelectionMode::Word {
            drag.start
        } else {
            normalize_mouse_selection_cell(&pane.terminal, end)
        };
        let selected =
            drag.mode != MouseSelectionMode::Character || drag.moved || end != drag.start;
        if !selected {
            if let Some(pane) = self.panes.get_mut(&drag.pane) {
                pane.mouse_selection = None;
            }
            // A press and release on one cell with no motion between them is a click, not a
            // selection — the same test Vivido uses to decide a drag should not launch a link.
            if self.config.hyperlinks.enabled
                && self.config.hyperlinks.open == OpenMode::Local
                && let Some(link) = self.link_at_cell(drag.pane, drag.start)
            {
                self.open_link_locally(&link.uri);
            }
            self.schedule_render();
            return;
        }
        let selection = MouseSelection {
            start: drag.start,
            end,
            mode: drag.mode,
        };
        let bytes = extract_mouse_selection(&pane.terminal, selection);
        if let Some(pane) = self.panes.get_mut(&drag.pane) {
            pane.mouse_selection = Some(selection);
        }
        self.set_copy_buffer(bytes);
        self.schedule_render();
    }

    /// Adopt `bytes` as the copy buffer and mirror it to the clipboard of whoever copied it.
    pub(super) fn set_copy_buffer(&mut self, bytes: Vec<u8>) {
        self.copy_buffer = bytes;
        self.copy_buffer.truncate(COPY_BUFFER_LIMIT);
        self.mirror_copy_buffer();
    }

    /// Send the copy buffer to host clipboards.
    ///
    /// A copy a user made goes to that user's terminal. A program's OSC 52 store has no such
    /// user, so it goes to every client that can type — a read-only watcher's clipboard is not
    /// the session's to write.
    pub(super) fn mirror_copy_buffer(&self) {
        let message = ServerMessage::Clipboard(String::from_utf8_lossy(&self.copy_buffer).into());
        match self.current_client.and_then(|id| self.clients.get(&id)) {
            Some(client) => {
                let _ = crate::ipc::send(&client.writer, &message);
            }
            None => {
                for client in self.clients.values().filter(|client| !client.read_only) {
                    let _ = crate::ipc::send(&client.writer, &message);
                }
            }
        }
    }

    /// Honor an OSC 52 store from a pane.
    ///
    /// Restricted to the focused pane of an attached session. The copy buffer belongs to the user,
    /// so a background pane silently overwriting it — or a detached session accepting a write
    /// nobody can see — is not something the user asked for. Between a focused pane and a mouse
    /// selection the later write wins, and an in-progress selection is left untouched.
    pub(super) fn handle_clipboard_store(&mut self, focused: bool, selection: u8, text: Vec<u8>) {
        if !clipboard_store_allowed(
            self.config.clipboard.osc52,
            focused,
            !self.clients.is_empty(),
            selection,
        ) {
            return;
        }
        self.set_copy_buffer(text);
    }

    /// Answer an OSC 52 query on the requesting pane's own PTY.
    pub(super) fn handle_clipboard_load(
        &mut self,
        pane_id: PaneId,
        focused: bool,
        selection: u8,
        terminator: &str,
    ) {
        if !self.config.clipboard.osc52.allows_load()
            || !focused
            || self.clients.is_empty()
            || !is_supported_clipboard_selection(selection)
        {
            return;
        }
        let reply = osc52_reply(selection, &self.copy_buffer, terminator);
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            let _ = queue_pane_input(pane, &reply);
        }
    }

    pub(super) fn mouse(&mut self, mut mouse: MouseEvent, pixel_coordinates: bool) {
        // Panes are laid out for the layout display, anchored at the origin, so a cell means the
        // same pane on every client. The tab list, though, sits at the edge of the terminal the
        // event came from, and pixels are that terminal's pixels. Automation uses the witness.
        let (host, drawn_at_this_size, status_targets, sidebar_targets) = match self
            .current_client
            .and_then(|id| self.clients.get(&id))
            .or_else(|| self.witness_client())
        {
            Some(client) => (
                client.display,
                client.last_screen.as_ref().is_some_and(|screen| {
                    screen.rows == client.display.rows && screen.columns == client.display.columns
                }),
                client.status_tab_targets.clone(),
                client.sidebar_targets.clone(),
            ),
            None => (self.layout_display(), false, Vec::new(), Vec::new()),
        };
        self.status_tab_targets = status_targets;
        self.sidebar_targets = sidebar_targets;
        let display = DisplayMetrics {
            cell_width: host.cell_width,
            cell_height: host.cell_height,
            ..self.layout_display()
        };
        let pixels = pixel_coordinates.then_some((mouse.x, mouse.y));
        if pixel_coordinates {
            mouse = pixel_mouse_to_cells(mouse, host);
        }
        // Another client's menu, prompt, or drag is not this pointer's to drive or cancel.
        if self.owned_ui_active() && !self.ui_takes_current_input() {
            return;
        }
        // Shift-left gestures belong to the outer terminal. If it forwards a report anyway,
        // do not turn it into pane selection, focus changes, or a continuation of a live drag.
        if mouse.shift && mouse.button == 0 && mouse.kind != MouseKind::Wheel {
            self.mouse_selection_drag = None;
            self.mouse_click_tracker = None;
            self.plugin_link_press = None;
            self.cancel_pointer_drag(true);
            return;
        }
        if self.agent_navigator.is_some() {
            self.agent_navigator_mouse(mouse);
            return;
        }
        if self.tab_navigator.is_some() {
            self.tab_navigator_mouse(mouse);
            return;
        }
        if self.pane_menu.is_some() {
            self.pane_menu_mouse(mouse);
            return;
        }
        if self.tab_rename.is_some() || self.close_pane_confirmation.is_some() {
            return;
        }
        if self.direct_pane().is_none()
            && drawn_at_this_size
            && let Some(sidebar) = self
                .tab_view
                .sidebar_rect(host.columns, host.rows)
                .filter(|rect| rect.contains(mouse.x, mouse.y))
        {
            // The sidebar is outside every pane, so nothing else can want this pointer event.
            if matches!(mouse.kind, MouseKind::Press | MouseKind::Wheel) {
                self.sidebar_mouse(mouse, sidebar);
            }
            return;
        }
        if self.direct_pane().is_none()
            && mouse.kind == MouseKind::Press
            && mouse.button == 0
            && drawn_at_this_size
            && self.tab_view.bar_row(host.rows) == Some(mouse.y)
            && let Some(index) = self.status_tab_targets.iter().find_map(|(range, id)| {
                range
                    .contains(&usize::from(mouse.x))
                    .then(|| self.tabs.iter().position(|tab| tab.id == *id))
                    .flatten()
            })
        {
            self.action(Action::SelectTab(index));
            return;
        }
        if self.handle_plugin_link_mouse(mouse) {
            return;
        }
        if self.mouse_selection_drag.is_some() {
            match mouse.kind {
                MouseKind::Move if mouse.button == 0 => {
                    self.update_mouse_selection(mouse);
                    return;
                }
                MouseKind::Release if mouse.button == 0 => {
                    self.finish_mouse_selection(mouse);
                    return;
                }
                _ => {
                    self.mouse_selection_drag = None;
                    self.mouse_click_tracker = None;
                }
            }
        }
        if let Some(pane_id) = self.direct_pane() {
            self.direct_mouse(pane_id, mouse, pixels, display);
            return;
        }
        if self.pointer_drag.is_some() {
            match mouse.kind {
                MouseKind::Release if !mouse.shift && mouse.button == 0 => {
                    // A valid left-button release commits the live rectangle/tree.
                    self.pointer_drag = None;
                    return;
                }
                MouseKind::Move if !mouse.shift && mouse.button == 0 => {
                    self.update_pointer_drag(mouse);
                    return;
                }
                // A new press, Shift-modified event, wheel, wrong button, or other malformed
                // sequence cancels and restores the press-time state before normal handling.
                _ => self.cancel_pointer_drag(true),
            }
        }
        if matches!(mouse.kind, MouseKind::Move | MouseKind::Release) {
            self.forward_application_mouse(mouse, pixels, display);
            return;
        }
        if mouse.kind == MouseKind::Wheel && self.invalidate_mouse_selection_state() {
            self.schedule_render();
        }
        let cleared_selection =
            mouse.kind == MouseKind::Press && self.clear_retained_mouse_selections();
        if cleared_selection {
            self.schedule_render();
        }
        if mouse.kind == MouseKind::Press && mouse.button != 0 {
            self.mouse_click_tracker = None;
        }
        let area = self.content_area();
        let Some((tab_id, focused, original_tree, projection)) =
            self.active_tab().and_then(|tab| {
                // Top-down hit testing over the same ordered projections that composition paints,
                // so clicking always addresses the visually topmost pane.
                let hit = visible_projections(tab, area)
                    .into_iter()
                    .rev()
                    .find(|projection| projection.outer.contains(mouse.x, mouse.y))?;
                Some((tab.id, tab.focused, tab.tree.clone(), hit))
            })
        else {
            if mouse.kind == MouseKind::Press && mouse.button == 0 {
                self.mouse_click_tracker = None;
            }
            return;
        };
        let (pane_id, rect) = (projection.pane_id, projection.outer);
        let focus_changed = focused != pane_id;
        let raised = projection.layer != PaneLayer::Tiled && focus_changed;
        if focus_changed {
            self.end_float_mode(true);
        }
        if let Some(tab) = self.active_tab_mut() {
            tab.set_focus(pane_id);
        }
        if focus_changed {
            if raised {
                self.force_full = true;
            }
            self.projection_changed();
        }
        let on_vertical = mouse.x == rect.x || mouse.x + 1 == rect.x + rect.width;
        let on_horizontal = mouse.y == rect.y || mouse.y + 1 == rect.y + rect.height;
        if projection.layer != PaneLayer::Tiled {
            if mouse.kind == MouseKind::Press
                && mouse.button == 0
                && !mouse.shift
                && let Some(target) = float_pointer_target(
                    rect,
                    mouse.x,
                    mouse.y,
                    self.config.floating.border_drag_margin,
                )
            {
                let origin = self
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .and_then(|tab| tab.floating.get(pane_id))
                    .and_then(|float| float.origin);
                self.pointer_drag = Some(match target {
                    FloatPointerTarget::Move => PointerDrag::Move {
                        tab_id,
                        pane: pane_id,
                        start: (mouse.x, mouse.y),
                        original: rect,
                        origin,
                    },
                    FloatPointerTarget::Resize(edges) => PointerDrag::Resize {
                        tab_id,
                        pane: pane_id,
                        edges,
                        start: (mouse.x, mouse.y),
                        original: rect,
                        origin,
                    },
                });
                self.schedule_render();
                self.mouse_click_tracker = None;
                return;
            }
        } else if mouse.kind == MouseKind::Press
            && mouse.button == 0
            && !mouse.shift
            && (on_vertical || on_horizontal)
        {
            let axis = if on_vertical {
                Axis::Horizontal
            } else {
                Axis::Vertical
            };
            let boundary = if axis == Axis::Horizontal {
                mouse.x
            } else {
                mouse.y
            };
            let Some(original) = original_tree else {
                return;
            };
            self.pointer_drag = Some(PointerDrag::TiledBoundary {
                tab_id,
                axis,
                boundary,
                last: boundary,
                original,
            });
            self.schedule_render();
            self.mouse_click_tracker = None;
            return;
        }

        let right_press = mouse.kind == MouseKind::Press && mouse.button == 2;
        let content = rect.content();
        if mouse.x < content.x
            || mouse.x >= content.x + content.width
            || mouse.y < content.y
            || mouse.y >= content.y + content.height
        {
            if right_press {
                self.open_pane_menu(tab_id, pane_id, (mouse.x, mouse.y));
                return;
            }
            self.schedule_render();
            if mouse.kind == MouseKind::Press && mouse.button == 0 {
                self.mouse_click_tracker = None;
            }
            return;
        }
        if self.overlay_mouse(pane_id, mouse, pixels, content, display) {
            return;
        }
        // As in tmux, a right click belongs to an application that asked for mouse clicks, and
        // Shift takes it back for the menu.
        if right_press
            && (mouse.shift
                || self
                    .panes
                    .get(&pane_id)
                    .is_some_and(|pane| !pane.terminal.modes().mouse_clicks))
        {
            self.open_pane_menu(tab_id, pane_id, (mouse.x, mouse.y));
            return;
        }
        let selection_gesture = self.panes.get(&pane_id).is_some_and(|pane| {
            starts_mouse_selection(mouse, pane.copy.is_some(), pane.terminal.modes())
        });
        if selection_gesture {
            self.begin_mouse_selection(pane_id, content, mouse);
            return;
        }
        if mouse.kind == MouseKind::Press && mouse.button == 0 {
            self.mouse_click_tracker = None;
        }
        let mut translated = None;
        let mut copy_view_render = false;
        let mut media_view_changed = false;
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            let modes = pane.terminal.modes();
            let application_mouse = !mouse.shift
                && (modes.mouse_clicks || (mouse.kind == MouseKind::Move && modes.mouse_motion));
            if application_mouse {
                let mut button = u16::from(mouse.button);
                if mouse.kind == MouseKind::Wheel {
                    button |= 64;
                }
                if mouse.kind == MouseKind::Move {
                    button |= 32;
                }
                if mouse.alt {
                    button |= 8;
                }
                if mouse.ctrl {
                    button |= 16;
                }
                let (x, y) = application_mouse_coordinates(
                    mouse,
                    pixels,
                    content,
                    display,
                    modes.sgr_pixels,
                );
                translated = Some(crate::agent_drive::encode_sgr_mouse(
                    button,
                    x,
                    y,
                    mouse.kind != MouseKind::Release,
                ));
            } else if mouse.kind == MouseKind::Wheel {
                copy_view_render = true;
                let previous_offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
                let copy = pane.copy.get_or_insert(CopyState {
                    offset: 0,
                    row: 0,
                    column: 0,
                    selection_start: None,
                    search: None,
                    matches: Vec::new(),
                    current: None,
                });
                if mouse.button == 0 {
                    copy.offset = (copy.offset + 3).min(pane.terminal.history_len());
                } else {
                    copy.offset = copy.offset.saturating_sub(3);
                    if copy.offset == 0 {
                        pane.copy = None;
                    }
                }
                media_view_changed =
                    previous_offset != pane.copy.as_ref().map_or(0, |copy| copy.offset);
            }
        }
        if media_view_changed {
            self.projection_changed();
        } else if copy_view_render {
            self.schedule_render();
        }
        if let Some(translated) = translated {
            self.send_pane_input(pane_id, translated.as_bytes());
        }
    }

    /// The OSC 8 link on a pane's grid cell, if any.
    pub(super) fn link_at_cell(
        &self,
        pane_id: PaneId,
        cell: (isize, usize),
    ) -> Option<TerminalHyperlink> {
        let pane = self.panes.get(&pane_id)?;
        let line = pane.terminal.viewport_line(cell.0)?;
        line.get(cell.1)?.hyperlink.clone()
    }

    /// The OSC 8 link at a pane-content coordinate, if any.
    pub(super) fn link_at(
        &self,
        pane_id: PaneId,
        content: Rect,
        x: u16,
        y: u16,
    ) -> Option<TerminalHyperlink> {
        let pane = self.panes.get(&pane_id)?;
        let display_offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
        let cell = mouse_selection_cell(content, x, y, display_offset)?;
        self.link_at_cell(pane_id, cell)
    }

    /// Capture a Ctrl-left click on an OSC 8 URL for a declarative plugin handler. Capture wins
    /// over application mouse mode deliberately: Ctrl is the explicit user opt-in to reroute the
    /// click, while an ordinary click remains byte-for-byte application input or local selection.
    pub(super) fn handle_plugin_link_mouse(&mut self, mouse: MouseEvent) -> bool {
        if let Some(press) = self.plugin_link_press.take() {
            match mouse.kind {
                MouseKind::Move if mouse.button == 0 => {
                    if mouse_selection_cell(press.content, mouse.x, mouse.y, 0) == Some(press.cell)
                    {
                        self.plugin_link_press = Some(press);
                    }
                    return true;
                }
                MouseKind::Release if mouse.button == 0 => {
                    let unchanged = mouse_selection_cell(press.content, mouse.x, mouse.y, 0)
                        == Some(press.cell)
                        && self
                            .link_at_cell(press.pane, press.cell)
                            .is_some_and(|link| link.uri == press.uri);
                    if unchanged {
                        self.invoke_plugin_action(
                            press.action,
                            serde_json::json!({"uri": press.uri}),
                        );
                    }
                    return true;
                }
                _ => {}
            }
        }
        if mouse.kind != MouseKind::Press
            || mouse.button != 0
            || !mouse.ctrl
            || self.plugin_link_handlers.is_empty()
        {
            return false;
        }
        let area = self.content_area();
        let Some(projection) = self
            .attached_projections(area)
            .into_iter()
            .rev()
            .find(|projection| projection.content.contains(mouse.x, mouse.y))
        else {
            return false;
        };
        let Some(cell) = mouse_selection_cell(projection.content, mouse.x, mouse.y, 0) else {
            return false;
        };
        let Some(link) = self.link_at_cell(projection.pane_id, cell) else {
            return false;
        };
        let actions = self
            .plugin_link_handlers
            .iter()
            .filter(|handler| handler.regex.is_match(&link.uri))
            .take(2)
            .map(|handler| handler.action.clone())
            .collect::<Vec<_>>();
        let [action] = actions.as_slice() else {
            if actions.len() > 1 {
                self.status("plugin link click is ambiguous; matching handlers were not invoked");
                return true;
            }
            return false;
        };
        self.plugin_link_press = Some(PluginLinkPress {
            pane: projection.pane_id,
            content: projection.content,
            cell,
            uri: link.uri,
            action: action.clone(),
        });
        true
    }

    /// Open a clicked link on the host vvmux itself runs on.
    ///
    /// Only reached in `open = "local"`. The default delegates instead, because vvmux is often the
    /// remote end of an ssh session where the browser would come up on the wrong machine.
    pub(super) fn open_link_locally(&mut self, uri: &str) {
        // A double click is two press/release pairs on one cell, so without a cooldown it would
        // launch the handler twice.
        let now = Instant::now();
        if self
            .last_link_open
            .is_some_and(|last| now.duration_since(last) < LINK_OPEN_COOLDOWN)
        {
            return;
        }
        // The URI comes from whatever wrote to the pane, so it is untrusted input. Passing it as a
        // single argv element keeps it out of any shell, and only a vetted scheme is handed over at
        // all — an OSC 8 link is not constrained by the URL regex that guards text matches, so
        // `file:` and friends would otherwise be one click from launching a local handler.
        if !is_openable_uri(uri) {
            self.notice(format!("refused to open unsupported link: {uri}"));
            return;
        }
        self.last_link_open = Some(now);
        match crate::platform::open_external(uri) {
            Ok(()) => self.notice(format!("opening {uri}")),
            Err(error) => self.notice(format!("could not open link: {error}")),
        }
    }

    /// Record the link under the pointer, redrawing when it changes.
    pub(super) fn set_hovered_link(&mut self, hovered: Option<HoveredLink>) {
        if self.hovered_link == hovered {
            return;
        }
        self.hovered_link = hovered;
        // No `force_full`: the hover mark is applied to cells during composition, so the ordinary
        // cell diff already repaints exactly the run that changed. Forcing a full repaint here
        // would rewrite the whole screen on every pointer motion.
        self.schedule_render();
    }

    /// Drop hover state belonging to `pane_id`, leaving any other pane's hover alone.
    pub(super) fn clear_pane_hover(&mut self, pane_id: PaneId) {
        if self
            .hovered_link
            .as_ref()
            .is_some_and(|hovered| hovered.pane == pane_id)
        {
            self.set_hovered_link(None);
        }
    }

    /// Give a pane's overlay windows first refusal on a pointer event, in the pane-local logical
    /// pixels their producer laid them out against.
    ///
    /// This follows the host's own rule rather than inventing one: the overlay consumes an event
    /// that lands in one of its windows, and anything it does not take falls through to the pane
    /// unchanged — selection, hyperlinks, application mouse reporting and copy-mode scrolling all
    /// behave exactly as they do in a pane with no overlay.
    pub(super) fn overlay_mouse(
        &mut self,
        pane_id: PaneId,
        mouse: MouseEvent,
        pixels: Option<(u16, u16)>,
        content: Rect,
        display: DisplayMetrics,
    ) -> bool {
        let (x, y) = overlay_pointer_position(mouse, pixels, content, display);
        let modifiers = overlay_modifiers(mouse);
        let button = u16::from(mouse.button);
        let consumed = match mouse.kind {
            MouseKind::Move => self.vivid.overlay_pointer(pane_id, x, y, None, modifiers),
            MouseKind::Press => {
                self.vivid
                    .overlay_pointer(pane_id, x, y, Some((button, true)), modifiers)
            }
            MouseKind::Release => {
                self.vivid
                    .overlay_pointer(pane_id, x, y, Some((button, false)), modifiers)
            }
            MouseKind::Wheel => {
                // Report the same distance a wheel moves a pane locally. Button zero is the
                // upward detent, and a positive delta scrolls content down.
                let lines = 3. * f64::from(display.cell_height.max(1));
                let dy = if mouse.button == 0 { -lines } else { lines };
                self.vivid.overlay_wheel(pane_id, x, y, 0., dy, modifiers)
            }
        };
        consumed.unwrap_or(false)
    }

    /// Forward motion/release reports without changing pane focus. These used to return before
    /// application mouse handling, so even a pane in DEC 1003 mode could never hover, drag, or
    /// release a button.
    pub(super) fn forward_application_mouse(
        &mut self,
        mouse: MouseEvent,
        pixels: Option<(u16, u16)>,
        display: DisplayMetrics,
    ) {
        let area = self.content_area();
        let Some(projection) = self.active_tab().and_then(|tab| {
            visible_projections(tab, area)
                .into_iter()
                .rev()
                .find(|projection| projection.outer.contains(mouse.x, mouse.y))
        }) else {
            self.set_hovered_link(None);
            return;
        };
        let content = projection.outer.content();
        if !content.contains(mouse.x, mouse.y) {
            self.set_hovered_link(None);
            return;
        }
        let pane_id = projection.pane_id;
        if self.overlay_mouse(pane_id, mouse, pixels, content, display) {
            self.set_hovered_link(None);
            return;
        }
        let Some(modes) = self.panes.get(&pane_id).map(|pane| pane.terminal.modes()) else {
            self.set_hovered_link(None);
            return;
        };
        let application_mouse = !mouse.shift
            && match mouse.kind {
                MouseKind::Move => modes.mouse_motion,
                MouseKind::Release => modes.mouse_clicks,
                _ => false,
            };
        // Hover only where vvmux is the one reading the mouse. A pane running a full-screen
        // application that asked for motion reports owns those events, exactly as vvmux owns them
        // from the outer terminal; tracking hover anyway would mark links the pane cannot activate.
        if mouse.kind == MouseKind::Move {
            let hovered = (!application_mouse && self.config.hyperlinks.enabled)
                .then(|| self.link_at(pane_id, content, mouse.x, mouse.y))
                .flatten()
                .map(|link| HoveredLink {
                    pane: pane_id,
                    link,
                });
            self.set_hovered_link(hovered);
        }
        if !application_mouse {
            return;
        }
        let mut button = u16::from(mouse.button);
        if mouse.kind == MouseKind::Move {
            button |= 32;
        }
        if mouse.alt {
            button |= 8;
        }
        if mouse.ctrl {
            button |= 16;
        }
        let terminator = if mouse.kind == MouseKind::Release {
            'm'
        } else {
            'M'
        };
        let (x, y) =
            application_mouse_coordinates(mouse, pixels, content, display, modes.sgr_pixels);
        self.send_pane_input(
            pane_id,
            format!("\x1b[<{button};{x};{y}{terminator}").as_bytes(),
        );
    }

    pub(super) fn direct_mouse(
        &mut self,
        pane_id: PaneId,
        mouse: MouseEvent,
        pixels: Option<(u16, u16)>,
        display: DisplayMetrics,
    ) {
        let content = self.content_area();
        if !content.contains(mouse.x, mouse.y) {
            self.set_hovered_link(None);
            return;
        }
        if self.overlay_mouse(pane_id, mouse, pixels, content, display) {
            self.set_hovered_link(None);
            return;
        }
        let Some(modes) = self.panes.get(&pane_id).map(|pane| pane.terminal.modes()) else {
            return;
        };
        let application_mouse = !mouse.shift
            && (modes.mouse_clicks || (mouse.kind == MouseKind::Move && modes.mouse_motion));
        if mouse.kind == MouseKind::Move {
            let hovered = (!application_mouse && self.config.hyperlinks.enabled)
                .then(|| self.link_at(pane_id, content, mouse.x, mouse.y))
                .flatten()
                .map(|link| HoveredLink {
                    pane: pane_id,
                    link,
                });
            self.set_hovered_link(hovered);
        }
        if starts_mouse_selection(mouse, false, modes) {
            self.begin_mouse_selection(pane_id, content, mouse);
            return;
        }
        if application_mouse {
            let mut button = u16::from(mouse.button);
            if mouse.kind == MouseKind::Wheel {
                button |= 64;
            }
            if mouse.kind == MouseKind::Move {
                button |= 32;
            }
            if mouse.alt {
                button |= 8;
            }
            if mouse.ctrl {
                button |= 16;
            }
            let (x, y) =
                application_mouse_coordinates(mouse, pixels, content, display, modes.sgr_pixels);
            self.send_pane_input(
                pane_id,
                crate::agent_drive::encode_sgr_mouse(
                    button,
                    x,
                    y,
                    mouse.kind != MouseKind::Release,
                )
                .as_bytes(),
            );
        } else if mouse.kind == MouseKind::Wheel {
            let previous_offset = self
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.copy.as_ref())
                .map_or(0, |copy| copy.offset);
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                let copy = pane.copy.get_or_insert(CopyState {
                    offset: 0,
                    row: 0,
                    column: 0,
                    selection_start: None,
                    search: None,
                    matches: Vec::new(),
                    current: None,
                });
                if mouse.button == 0 {
                    copy.offset = (copy.offset + 3).min(pane.terminal.history_len());
                } else {
                    copy.offset = copy.offset.saturating_sub(3);
                    if copy.offset == 0 {
                        pane.copy = None;
                    }
                }
            }
            let current_offset = self
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.copy.as_ref())
                .map_or(0, |copy| copy.offset);
            if previous_offset == current_offset {
                self.schedule_render();
            } else {
                self.projection_changed();
            }
        }
    }

    pub(super) fn update_pointer_drag(&mut self, mouse: MouseEvent) {
        let Some(mut drag) = self.pointer_drag.take() else {
            return;
        };
        let active_tab_id = self.active_tab().map(|tab| tab.id);
        let drag_tab_id = match &drag {
            PointerDrag::TiledBoundary { tab_id, .. }
            | PointerDrag::Move { tab_id, .. }
            | PointerDrag::Resize { tab_id, .. } => *tab_id,
        };
        if active_tab_id != Some(drag_tab_id) {
            self.pointer_drag = Some(drag);
            self.cancel_pointer_drag(true);
            return;
        }

        let area = self.content_area();
        let (changed, floating) = match &mut drag {
            PointerDrag::TiledBoundary {
                axis,
                boundary,
                last,
                ..
            } => {
                let current = match axis {
                    Axis::Horizontal => mouse.x,
                    Axis::Vertical => mouse.y,
                };
                let mut changed = false;
                while *last < current {
                    if !self.resize_boundary(*axis, *boundary, true) {
                        break;
                    }
                    *last += 1;
                    *boundary += 1;
                    changed = true;
                }
                while *last > current {
                    if !self.resize_boundary(*axis, *boundary, false) {
                        break;
                    }
                    *last -= 1;
                    *boundary = boundary.saturating_sub(1);
                    changed = true;
                }
                (changed, false)
            }
            PointerDrag::Move {
                pane,
                start,
                original,
                ..
            } => {
                let dx = i32::from(mouse.x) - i32::from(start.0);
                let dy = i32::from(mouse.y) - i32::from(start.1);
                let changed = self
                    .active_tab_mut()
                    .is_some_and(|tab| tab.floating.move_from(*pane, *original, dx, dy, area));
                // A dragged float is where the user put it: stop re-proportioning it.
                if changed && let Some(tab) = self.active_tab_mut() {
                    tab.floating.clear_origin(*pane);
                }
                (changed, true)
            }
            PointerDrag::Resize {
                pane,
                edges,
                start,
                original,
                ..
            } => {
                let dx = i32::from(mouse.x) - i32::from(start.0);
                let dy = i32::from(mouse.y) - i32::from(start.1);
                let changed = self.active_tab_mut().is_some_and(|tab| {
                    tab.floating
                        .resize_from(*pane, *original, *edges, dx, dy, area)
                });
                if changed && let Some(tab) = self.active_tab_mut() {
                    tab.floating.clear_origin(*pane);
                }
                (changed, true)
            }
        };
        self.pointer_drag = Some(drag);
        if changed {
            if floating {
                self.force_full = true;
            }
            self.relayout();
        }
    }

    pub(super) fn cancel_pointer_drag(&mut self, restore: bool) {
        let Some(drag) = self.pointer_drag.take() else {
            return;
        };
        if !restore {
            return;
        }
        let area = self.content_area();
        let changed = match drag {
            PointerDrag::TiledBoundary {
                tab_id, original, ..
            } => self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == tab_id)
                .is_some_and(|tab| {
                    if tab.tree.as_ref() == Some(&original) {
                        false
                    } else {
                        tab.tree = Some(original);
                        true
                    }
                }),
            PointerDrag::Move {
                tab_id,
                pane,
                original,
                origin,
                ..
            }
            | PointerDrag::Resize {
                tab_id,
                pane,
                original,
                origin,
                ..
            } => self
                .tabs
                .iter_mut()
                .find(|tab| tab.id == tab_id)
                .is_some_and(|tab| {
                    let rect_changed = tab.floating.set_rect(pane, original, area);
                    // A cancelled edit is as if it never happened: the birth geometry the
                    // drag cleared comes back with the rectangle.
                    let origin_changed = tab
                        .floating
                        .get(pane)
                        .is_some_and(|float| float.origin != origin);
                    tab.floating.restore_origin(pane, origin);
                    rect_changed || origin_changed
                }),
        };
        if changed {
            self.force_full = true;
            self.relayout();
        }
    }

    pub(super) fn resize_boundary(&mut self, axis: Axis, boundary: u16, positive: bool) -> bool {
        let area = self.content_area();
        let Some(tab) = self.active_tab_mut() else {
            return false;
        };
        let Some(tree) = tab.tree.as_mut() else {
            return false;
        };
        let geometry = tree.geometry(area);
        let candidate = geometry
            .iter()
            .find_map(|(pane, rect)| match (axis, positive) {
                (Axis::Horizontal, true) if rect.x + rect.width == boundary => {
                    Some((*pane, Direction::Right))
                }
                (Axis::Horizontal, false) if rect.x == boundary => Some((*pane, Direction::Left)),
                (Axis::Vertical, true) if rect.y + rect.height == boundary => {
                    Some((*pane, Direction::Down))
                }
                (Axis::Vertical, false) if rect.y == boundary => Some((*pane, Direction::Up)),
                _ => None,
            });
        candidate.is_some_and(|(pane, direction)| tree.resize(pane, direction, area))
    }

    /// Queue `bytes` to reach a pane after `delay`.
    ///
    /// The caller has already replied or parked; nothing here reports success, because a delayed
    /// write has no request left to fail. Delivery is best effort by design: the pane can close,
    /// and a queue full of a slow reader's input must not become actor backpressure.
    pub(super) fn queue_delayed_input(
        &mut self,
        pane_id: PaneId,
        bytes: Vec<u8>,
        delay: Duration,
    ) -> bool {
        if bytes.is_empty() || self.delayed_inputs.len() >= MAX_DELAYED_INPUTS {
            return false;
        }
        let sequence = self.next_delayed_input;
        self.next_delayed_input = self.next_delayed_input.saturating_add(1);
        self.delayed_inputs.push(Reverse(DelayedInput {
            due: Instant::now() + delay,
            sequence,
            pane_id,
            bytes,
        }));
        true
    }

    /// Deliver every delayed input whose moment has arrived.
    pub(super) fn flush_delayed_inputs(&mut self) {
        let now = Instant::now();
        while self
            .delayed_inputs
            .peek()
            .is_some_and(|Reverse(input)| input.due <= now)
        {
            let Some(Reverse(input)) = self.delayed_inputs.pop() else {
                break;
            };
            // A pane that closed between queueing and now simply drops its input; the request that
            // scheduled this has already been answered.
            if let Some(pane) = self.panes.get(&input.pane_id) {
                let _ = pane.input.send(&input.bytes);
            }
        }
    }

    pub(super) fn next_delayed_input_deadline(&self) -> Duration {
        self.delayed_inputs
            .peek()
            .map_or(Duration::MAX, |Reverse(input)| {
                input.due.saturating_duration_since(Instant::now())
            })
    }

    pub(super) fn send_pane_input(&mut self, pane_id: PaneId, bytes: &[u8]) {
        let failure = self
            .panes
            .get_mut(&pane_id)
            .and_then(|pane| queue_pane_input(pane, bytes));
        self.report_input_failure(pane_id, failure);
    }

    /// Tell each pane whose program enabled focus reporting whether it now holds focus.
    ///
    /// The client asks its own terminal for focus reports so the session can answer this, which
    /// means the reports arrive whether or not any pane wants them. Relaying them as ordinary
    /// input put `ESC[I`/`ESC[O` in front of every focused pane's program: a shell prompt showed
    /// it as a stray newline, and a program that never asked for the mode echoed `^[[O`. A pane
    /// hears about focus only when it asked to and only when its own state changed, so an
    /// unrelated pane's program sees nothing at all.
    pub(super) fn sync_pane_focus(&mut self) {
        // A pane holds focus while any client's host terminal does: the view, and so the focused
        // pane, is shared.
        let client_focused = self.any_client_focused();
        let focused = self.attached_focus_pane().filter(|_| client_focused);
        let mut failures = Vec::new();
        let mut moved = Vec::new();
        for (pane_id, pane) in &mut self.panes {
            let holds_focus = focused == Some(*pane_id);
            if pane.focus_reported == holds_focus {
                continue;
            }
            pane.focus_reported = holds_focus;
            moved.push((*pane_id, holds_focus));
            if !pane.terminal.modes().focus_reporting {
                continue;
            }
            let report: &[u8] = if holds_focus { b"\x1b[I" } else { b"\x1b[O" };
            if let Some(failure) = queue_pane_input(pane, report) {
                failures.push((*pane_id, failure));
            }
        }
        for (pane_id, failure) in failures {
            self.report_input_failure(pane_id, Some(failure));
        }
        // An overlay window draws itself differently when its pane is the one being typed into, so
        // it hears about the same change the focus report carries — unconditionally, because an
        // overlay producer never enables focus reporting on the PTY it is not reading. The host
        // ignores a pane that does not own the focused window, so telling it about both sides of a
        // move is what lets the losing pane's window go inactive.
        for (pane_id, holds_focus) in moved {
            self.vivid.set_overlay_pane_focus(pane_id, holds_focus);
        }
        let focus_state = (
            client_focused,
            focused.and_then(|pane_id| {
                self.tabs
                    .iter()
                    .find(|tab| tab.contains(pane_id))
                    .map(|tab| tab.id)
            }),
            focused,
        );
        if self.last_plugin_focus != Some(focus_state) {
            self.last_plugin_focus = Some(focus_state);
            self.queue_plugin_state_event(
                "focus.changed",
                "focus".into(),
                serde_json::json!({
                    "client_focused": focus_state.0,
                    "tab_id": focus_state.1,
                    "pane_id": focus_state.2,
                }),
                focus_state.2,
            );
            self.schedule_render();
        }
    }

    /// Mirror the focused pane's Kitty keyboard and SGR-Pixels modes into the attached host
    /// terminal. Without this hop, a nested application can request enhanced key events and pixel
    /// coordinates while the physical presenter continues sending legacy keys and cell positions.
    pub(super) fn sync_client_input_mode(&mut self) {
        let (mut keyboard_flags, mut sgr_pixels) = self
            .attached_focus_pane()
            .and_then(|pane_id| self.panes.get(&pane_id))
            .map_or((0, false), |pane| {
                let modes = pane.terminal.modes();
                (modes.keyboard_flags, modes.sgr_pixels)
            });
        if self
            .attached_focus_pane()
            .is_some_and(|pane| self.vivid.overlay_has_focus(pane))
        {
            keyboard_flags |= 1 | 2 | 8 | 16;
            sgr_pixels = true;
        }
        let input_mode = (keyboard_flags, sgr_pixels);
        // Each host terminal is told once; a newly attached one has been told nothing yet.
        for client in self.clients.values_mut() {
            if client.reported_input_mode == Some(input_mode) {
                continue;
            }
            client.reported_input_mode = Some(input_mode);
            let _ = crate::ipc::send(
                &client.writer,
                &ServerMessage::InputMode {
                    keyboard_flags,
                    sgr_pixels,
                },
            );
        }
    }

    pub(super) fn report_input_failure(&mut self, pane_id: PaneId, failure: Option<InputFailure>) {
        let Some(failure) = failure else {
            return;
        };
        if failure.warn {
            self.status(&format!("pane {pane_id} input queue is unavailable"));
        }
        if failure.close {
            self.close_pane(pane_id);
        }
    }

    pub(super) fn copy_input(&mut self, pane_id: PaneId, bytes: &[u8]) {
        let prompt_active = self.panes.get(&pane_id).is_some_and(|pane| {
            pane.copy
                .as_ref()
                .and_then(|copy| copy.search.as_ref())
                .is_some_and(|search| search.prompt.is_some())
        });
        if !prompt_active && bytes.len() > 1 && matches!(bytes.first(), Some(b'/' | b'?')) {
            self.copy_input(pane_id, &bytes[..1]);
            self.copy_input(pane_id, &bytes[1..]);
            return;
        }
        let remapped = (!prompt_active)
            .then(|| copy_chord_name(bytes))
            .flatten()
            .and_then(|chord| self.config.keys.copy.get(chord))
            .and_then(|action| copy_action_bytes(action));
        let bytes = remapped.as_deref().unwrap_or(bytes);
        let Some(previous) = self.panes.get(&pane_id).and_then(|pane| pane.copy.clone()) else {
            return;
        };

        if prompt_active {
            let (action, direction) = {
                let pane = self.panes.get_mut(&pane_id).unwrap();
                let copy = pane.copy.as_mut().unwrap();
                let search = copy.search.as_mut().unwrap();
                let action = apply_prompt_key(search.prompt.as_mut().unwrap(), bytes);
                (action, search.direction)
            };
            match action {
                PromptAction::Editing => {}
                PromptAction::Cancel => {
                    if let Some(search) = self
                        .panes
                        .get_mut(&pane_id)
                        .unwrap()
                        .copy
                        .as_mut()
                        .and_then(|copy| copy.search.as_mut())
                    {
                        search.prompt = None;
                    }
                }
                PromptAction::Submit(query) => match crate::search::compile(&query, true, true) {
                    Ok(pattern) => {
                        self.search_pattern = Some((query.clone(), pattern));
                        let from = {
                            let copy = self.panes[&pane_id].copy.as_ref().unwrap();
                            (copy.row as isize - copy.offset as isize, copy.column)
                        };
                        let found = {
                            let pane = &self.panes[&pane_id];
                            find_next(
                                &pane.terminal,
                                &self.search_pattern.as_ref().unwrap().1,
                                from,
                                direction,
                                true,
                            )
                        };
                        let pane = &mut *self.panes.get_mut(&pane_id).unwrap();
                        let copy = pane.copy.as_mut().unwrap();
                        let search = copy.search.as_mut().unwrap();
                        search.prompt = None;
                        search.query = query;
                        if let Some(found) = found {
                            copy_jump_to(pane, &self.search_pattern.as_ref().unwrap().1, found);
                        } else {
                            copy.current = None;
                            copy.matches.clear();
                            self.status("search pattern not found");
                        }
                    }
                    Err(error) => {
                        if let Some(search) = self
                            .panes
                            .get_mut(&pane_id)
                            .unwrap()
                            .copy
                            .as_mut()
                            .and_then(|copy| copy.search.as_mut())
                        {
                            search.prompt = None;
                        }
                        self.status(&format!("invalid search pattern: {error}"));
                    }
                },
            }
            self.finish_copy_input(pane_id, previous);
            return;
        }

        let mut search_not_found = false;
        let mut copied = false;
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        let Some(copy) = &mut pane.copy else {
            return;
        };
        let rows = pane.terminal.rows();
        let columns = pane.terminal.cols();
        match bytes {
            b"q" | b"\x1b" => pane.copy = None,
            b"\x1b[A" => {
                if copy.row == 0 {
                    copy.offset = (copy.offset + 1).min(pane.terminal.history_len());
                } else {
                    copy.row -= 1;
                }
            }
            b"\x1b[B" => {
                if copy.row + 1 >= rows && copy.offset > 0 {
                    copy.offset -= 1;
                } else {
                    copy.row = (copy.row + 1).min(rows.saturating_sub(1));
                }
            }
            b"\x1b[C" => copy.column = (copy.column + 1).min(columns.saturating_sub(1)),
            b"\x1b[D" => copy.column = copy.column.saturating_sub(1),
            b"\x1b[5~" => {
                copy.offset = (copy.offset + rows).min(pane.terminal.history_len());
            }
            b"\x1b[6~" => copy.offset = copy.offset.saturating_sub(rows),
            b"/" | b"?" => {
                copy.search = Some(CopySearch {
                    prompt: Some(String::new()),
                    direction: if bytes == b"/" {
                        SearchDirection::Forward
                    } else {
                        SearchDirection::Backward
                    },
                    query: copy
                        .search
                        .as_ref()
                        .map_or_else(String::new, |search| search.query.clone()),
                });
            }
            b"n" | b"N" => {
                let Some(search) = copy.search.as_ref() else {
                    self.status("no search query");
                    self.finish_copy_input(pane_id, previous);
                    return;
                };
                if search.query.is_empty() {
                    self.status("no search query");
                    self.finish_copy_input(pane_id, previous);
                    return;
                }
                let direction = if bytes == b"n" {
                    search.direction
                } else {
                    search.direction.opposite()
                };
                let query = search.query.clone();
                let current = copy.current;
                let from = current.map_or(
                    (copy.row as isize - copy.offset as isize, copy.column),
                    |found| match direction {
                        SearchDirection::Forward => (found.line, found.end_column),
                        SearchDirection::Backward => {
                            (found.line, found.start_column.saturating_sub(1))
                        }
                    },
                );
                let needs_compile = self
                    .search_pattern
                    .as_ref()
                    .is_none_or(|(compiled, _)| compiled != &query);
                if needs_compile {
                    match crate::search::compile(&query, true, true) {
                        Ok(pattern) => self.search_pattern = Some((query, pattern)),
                        Err(error) => {
                            self.status(&format!("invalid search pattern: {error}"));
                            self.finish_copy_input(pane_id, previous);
                            return;
                        }
                    }
                }
                let found = find_next(
                    &pane.terminal,
                    &self.search_pattern.as_ref().unwrap().1,
                    from,
                    direction,
                    true,
                );
                if let Some(found) = found {
                    copy_jump_to(pane, &self.search_pattern.as_ref().unwrap().1, found);
                } else {
                    search_not_found = true;
                }
            }
            b" " => {
                copy.selection_start =
                    Some((copy.row as isize - copy.offset as isize, copy.column));
            }
            b"\r" | b"\n" => {
                let end = (copy.row as isize - copy.offset as isize, copy.column);
                let start = copy.selection_start.unwrap_or((-(copy.offset as isize), 0));
                self.copy_buffer = extract_selection(&pane.terminal, start, end);
                self.copy_buffer.truncate(COPY_BUFFER_LIMIT);
                pane.copy = None;
                copied = true;
            }
            _ => {}
        }
        if let Some((query, pattern)) = &self.search_pattern
            && pane
                .copy
                .as_ref()
                .and_then(|copy| copy.search.as_ref())
                .is_some_and(|search| search.query == *query)
        {
            refresh_copy_matches(pane, pattern);
        }
        let _ = pane;
        if copied {
            self.mirror_copy_buffer();
        }
        if search_not_found {
            self.status("search pattern not found");
        }
        self.finish_copy_input(pane_id, previous);
    }

    pub(super) fn finish_copy_input(&mut self, pane_id: PaneId, previous: CopyState) {
        let Some(pane) = self.panes.get(&pane_id) else {
            return;
        };
        let previous = Some(previous);
        let changed = previous != pane.copy;
        let media_view_changed = previous.as_ref().map_or(0, |copy| copy.offset)
            != pane.copy.as_ref().map_or(0, |copy| copy.offset);
        if changed {
            self.mark_pane_screen_change(pane_id, None);
        }
        if media_view_changed {
            self.projection_changed();
        } else {
            self.schedule_render();
        }
    }

    pub(super) fn paste(&mut self) {
        if let Some(pane_id) = self.direct_pane() {
            let sanitized = sanitize_bracketed_paste(&self.copy_buffer);
            let bytes = self.paste_payload_for(pane_id, &sanitized);
            let failure = self
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| queue_pane_input(pane, &bytes));
            self.report_input_failure(pane_id, failure);
            return;
        }
        let Some(tab) = self.active_tab() else {
            return;
        };
        let focused = tab.focused;
        let targets = if tab.sync_input {
            sync_targets(tab, &|pane_id| {
                self.panes
                    .get(&pane_id)
                    .is_some_and(|pane| pane.copy.is_some() || !pane_role_accepts_sync(&pane.role))
            })
        } else {
            vec![focused]
        };
        let sanitized = sanitize_bracketed_paste(&self.copy_buffer);
        let mut failures = Vec::new();
        for pane_id in targets {
            let bytes = self.paste_payload_for(pane_id, &sanitized);
            if let Some(failure) = self
                .panes
                .get_mut(&pane_id)
                .and_then(|pane| queue_pane_input(pane, &bytes))
            {
                failures.push((pane_id, failure));
            }
        }
        for (pane_id, failure) in failures {
            self.report_input_failure(pane_id, Some(failure));
        }
    }

    pub(super) fn paste_payload_for(&self, pane_id: PaneId, sanitized: &[u8]) -> Vec<u8> {
        if self
            .panes
            .get(&pane_id)
            .is_some_and(|pane| pane.terminal.modes().bracketed_paste)
        {
            let mut bytes = Vec::with_capacity(sanitized.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(sanitized);
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            self.copy_buffer.clone()
        }
    }
}

fn copy_chord_name(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        b"\x1b[A" => Some("Up"),
        b"\x1b[B" => Some("Down"),
        b"\x1b[C" => Some("Right"),
        b"\x1b[D" => Some("Left"),
        b"\x1b[5~" => Some("PageUp"),
        b"\x1b[6~" => Some("PageDown"),
        b" " => Some("Space"),
        b"\r" | b"\n" => Some("Enter"),
        b"q" => Some("q"),
        b"/" => Some("/"),
        b"?" => Some("?"),
        b"n" => Some("n"),
        b"N" => Some("N"),
        b"\x1b" => Some("Escape"),
        _ => None,
    }
}

pub(crate) fn copy_action_bytes(action: &str) -> Option<Vec<u8>> {
    Some(
        match action {
            "up" => b"\x1b[A".as_slice(),
            "down" => b"\x1b[B".as_slice(),
            "left" => b"\x1b[D".as_slice(),
            "right" => b"\x1b[C".as_slice(),
            "page-up" => b"\x1b[5~".as_slice(),
            "page-down" => b"\x1b[6~".as_slice(),
            "start-selection" => b" ".as_slice(),
            "copy" => b"\r".as_slice(),
            "cancel" => b"q".as_slice(),
            "search-forward" => b"/".as_slice(),
            "search-backward" => b"?".as_slice(),
            "search-next" => b"n".as_slice(),
            "search-previous" => b"N".as_slice(),
            _ => return None,
        }
        .to_vec(),
    )
}

fn copy_jump_to(pane: &mut Pane, pattern: &SearchPattern, found: SearchMatch) {
    let rows = pane.terminal.rows();
    let centered = rows as isize / 2 - found.line;
    let offset = centered.clamp(0, pane.terminal.history_len() as isize) as usize;
    let copy = pane.copy.as_mut().unwrap();
    copy.offset = offset;
    copy.row = (found.line + offset as isize).clamp(0, rows.saturating_sub(1) as isize) as usize;
    copy.column = found
        .start_column
        .min(pane.terminal.cols().saturating_sub(1));
    copy.current = Some(found);
    refresh_copy_matches(pane, pattern);
}

fn overlay_modifiers(mouse: MouseEvent) -> u32 {
    use vivid_protocol::overlay::modifiers;
    let mut mask = 0;
    if mouse.shift {
        mask |= modifiers::SHIFT;
    }
    if mouse.ctrl {
        mask |= modifiers::CONTROL;
    }
    if mouse.alt {
        mask |= modifiers::ALT;
    }
    mask
}
