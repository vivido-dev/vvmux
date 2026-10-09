//! Pane and tab lifecycle, floating panes, layout plans, and configuration reload.

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
    pub(super) fn enter_float_mode(&mut self, kind: FloatingEditKind) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.zoomed.is_some() {
            self.status("unzoom before editing a floating pane");
            return;
        }
        let Some(float) = tab.floating.get(tab.focused) else {
            self.status("only floating panes can be moved or resized");
            return;
        };
        let (pane, original, origin) = (float.pane_id, float.rect, float.origin);
        let Some(next_mode) = self.next_float_mode.checked_add(1) else {
            self.status("floating edit mode ID space exhausted");
            return;
        };
        self.next_float_mode = next_mode;
        let mode_id = self.next_float_mode;
        self.float_modal = Some(FloatModal {
            mode_id,
            client: self.current_client,
            pane,
            kind,
            original,
            origin,
        });
        self.send_float_mode(
            self.current_client,
            ServerMessage::FloatingEditMode {
                mode_id,
                pane: Some(pane),
                kind: Some(kind),
            },
        );
    }

    /// End the active float-edit mode. `restore` puts the captured entry rectangle and birth
    /// geometry back (re-clamped against the current area); commit and destroyed-pane paths pass
    /// `false`.
    pub(super) fn end_float_mode(&mut self, restore: bool) {
        let Some(modal) = self.float_modal.take() else {
            return;
        };
        if restore {
            let area = self.content_area();
            let changed = self
                .tabs
                .iter_mut()
                .find(|tab| tab.floating.contains(modal.pane))
                .is_some_and(|tab| {
                    let rect_changed = tab.floating.set_rect(modal.pane, modal.original, area);
                    // A cancelled edit is as if it never happened: the birth geometry the
                    // steps cleared comes back with the rectangle.
                    let origin_changed = tab
                        .floating
                        .get(modal.pane)
                        .is_some_and(|float| float.origin != modal.origin);
                    tab.floating.restore_origin(modal.pane, modal.origin);
                    rect_changed || origin_changed
                });
            if changed {
                self.force_full = true;
                self.relayout();
            }
        }
        self.send_float_mode(
            modal.client,
            ServerMessage::FloatingEditMode {
                mode_id: modal.mode_id,
                pane: None,
                kind: None,
            },
        );
    }

    pub(super) fn send_float_mode(&self, client: Option<u64>, message: ServerMessage) {
        match client {
            Some(id) => {
                if let Some(client) = self.clients.get(&id) {
                    let _ = crate::ipc::send(&client.writer, &message);
                }
            }
            None => self.send_to_clients(&message),
        }
    }

    pub(super) fn float_edit(&mut self, mode_id: u64, command: FloatingEditCommand) {
        let Some(modal) = &self.float_modal else {
            return;
        };
        if modal.mode_id != mode_id {
            // A command from an already-ended mode raced its cancellation; ignore it.
            return;
        }
        let (pane, kind) = (modal.pane, modal.kind);
        match command {
            FloatingEditCommand::Commit => self.end_float_mode(false),
            FloatingEditCommand::Cancel => self.end_float_mode(true),
            FloatingEditCommand::Step { direction, cells } => {
                if !matches!(cells, 1 | 5) {
                    self.end_float_mode(true);
                    return;
                }
                let step = i32::from(cells);
                let (dx, dy) = match direction {
                    Direction::Left => (-step, 0),
                    Direction::Right => (step, 0),
                    Direction::Up => (0, -step),
                    Direction::Down => (0, step),
                };
                let area = self.content_area();
                let changed = self.active_tab_mut().is_some_and(|tab| match kind {
                    FloatingEditKind::Move => tab.floating.move_by(pane, dx, dy, area),
                    // Keyboard resize anchors the top-left corner: Left/Up shrink the
                    // bottom-right edges, Right/Down grow them.
                    FloatingEditKind::Resize => tab.floating.resize_by(
                        pane,
                        EdgeMask {
                            right: true,
                            bottom: true,
                            ..EdgeMask::default()
                        },
                        dx,
                        dy,
                        area,
                    ),
                });
                if changed {
                    // Stepping fixes the float: it stops re-proportioning on host changes.
                    if let Some(tab) = self.active_tab_mut() {
                        tab.floating.clear_origin(pane);
                    }
                    self.force_full = true;
                    self.relayout();
                }
            }
        }
    }

    pub(super) fn split(&mut self, axis: Axis) {
        let Some((focused, tree, tab_id)) = self
            .active_tab()
            .map(|tab| (tab.focused, tab.tree.clone(), tab.id))
        else {
            return;
        };
        let Some(tree) = tree else {
            self.status("no tiled pane to split");
            return;
        };
        if self
            .active_tab()
            .is_some_and(|tab| tab.floating.contains(focused))
        {
            self.status("cannot split a floating pane");
            return;
        }
        let pane_id = self.next_pane_id;
        let mut candidate = tree;
        if candidate
            .split(focused, pane_id, axis, self.content_area())
            .is_err()
        {
            self.status("pane is too small to split");
            return;
        }
        if self
            .spawn_pane(pane_id, tab_id, &PaneSpawn::default())
            .is_err()
        {
            self.status("could not spawn shell");
            return;
        }
        self.next_pane_id += 1;
        if let Some(tab) = self.active_tab_mut() {
            tab.tree = Some(candidate);
            tab.set_focus(pane_id);
        }
        self.relayout();
    }

    pub(super) fn focus(&mut self, direction: Direction) {
        let area = self.content_area();
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        if tab.zoomed.is_some() {
            return;
        }
        let projections = visible_projections(tab, area);
        if let Some(next) = directional_focus(&projections, tab.focused, direction) {
            tab.set_focus(next);
            self.force_full = true;
            self.projection_changed();
        }
    }

    pub(super) fn resize(&mut self, direction: Direction) {
        let area = self.content_area();
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.zoomed.is_some() {
            return;
        }
        if tab.floating.contains(tab.focused) {
            self.status("floating panes resize with the resize mode or the mouse");
            return;
        }
        let changed = self.active_tab_mut().is_some_and(|tab| {
            let focused = tab.focused;
            tab.tree
                .as_mut()
                .is_some_and(|tree| tree.resize(focused, direction, area))
        });
        if changed {
            self.relayout();
        }
    }

    pub(super) fn new_float(&mut self) {
        if self.active_tab().is_some_and(|tab| tab.zoomed.is_some()) {
            self.status("unzoom before creating a floating pane");
            return;
        }
        let pane_id = self.next_pane_id;
        let tab_id = self.active_tab().map_or(self.next_tab_id, |tab| tab.id);
        if self
            .spawn_pane(pane_id, tab_id, &PaneSpawn::default())
            .is_err()
        {
            self.status("could not spawn shell");
            return;
        }
        self.next_pane_id += 1;
        let area = self.content_area();
        let origin = self.default_float_origin();
        if let Some(tab) = self.active_tab_mut() {
            tab.floating.insert(pane_id, area, origin);
            tab.set_focus(pane_id);
        }
        self.force_full = true;
        self.relayout();
    }

    pub(super) fn toggle_floats(&mut self) {
        if self.active_tab().is_some_and(|tab| tab.zoomed.is_some()) {
            self.status("unzoom before changing floating pane visibility");
            return;
        }
        if self.active_tab().is_none_or(|tab| tab.floating.is_empty()) {
            self.status("no floating panes in this tab");
            return;
        }
        if let Some(tab) = self.active_tab_mut() {
            tab.floating.ordinary_visible = !tab.floating.ordinary_visible;
            let focused_hidden = !tab.floating.ordinary_visible
                && tab
                    .floating
                    .get(tab.focused)
                    .is_some_and(|float| !float.pinned);
            if focused_hidden && let Some(next) = tab.fallback_focus() {
                tab.set_focus(next);
            }
        }
        self.force_full = true;
        self.relayout();
    }

    pub(super) fn toggle_pin(&mut self) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.zoomed.is_some() {
            self.status("unzoom before pinning a pane");
            return;
        }
        let Some(pinned) = tab.floating.get(tab.focused).map(|float| float.pinned) else {
            self.status("only floating panes can be pinned");
            return;
        };
        if let Some(tab) = self.active_tab_mut() {
            let focused = tab.focused;
            tab.floating.set_pinned(focused, !pinned);
        }
        self.force_full = true;
        self.projection_changed();
    }

    /// Flip whether the focused pane paints its own background.
    ///
    /// No full repaint is forced: the substitution changes the composited cells themselves, so the
    /// ordinary per-cell diff already carries it. The status message is worth the line because the
    /// effect is invisible unless the outer terminal is running translucent — without it, a user
    /// whose window is opaque cannot tell the action fired at all.
    pub(super) fn toggle_transparency(&mut self) {
        let Some(pane_id) = self.active_tab().map(|tab| tab.focused) else {
            return;
        };
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        pane.transparent = !pane.transparent;
        let transparent = pane.transparent;
        self.schedule_render();
        self.status(if transparent {
            "pane background: transparent"
        } else {
            "pane background: opaque"
        });
    }

    pub(super) fn new_tab(&mut self) -> io::Result<()> {
        let pane_id = self.next_pane_id;
        let tab_id = self.next_tab_id;
        self.spawn_pane(pane_id, tab_id, &PaneSpawn::default())?;
        self.next_pane_id += 1;
        self.tabs.push(Tab {
            id: tab_id,
            name: None,
            tree: Some(TiledNode::leaf(pane_id)),
            floating: FloatingLayer::default(),
            focused: pane_id,
            last_focused_tiled: Some(pane_id),
            zoomed: None,
            sync_input: false,
        });
        self.next_tab_id += 1;
        self.schedule_render();
        Ok(())
    }

    pub(super) fn apply_layout_plan(
        &mut self,
        plan: LayoutPlan,
        extras: Option<&SnapshotExtras>,
        history: Option<&SessionHistory>,
    ) -> io::Result<()> {
        let area = self.content_area();
        let mut restored_active = None;
        // One conversation, one pane. Reserved across the whole restore rather than per tab,
        // because a duplicate reference can appear in any two panes of the session.
        let mut resumed_sessions: HashSet<String> = HashSet::new();
        for (plan_index, planned) in plan.tabs.into_iter().enumerate() {
            // Keyed by the plan's tab index, which is the index the capture walk emitted, not the
            // index this tab ends up at — a tab whose panes all failed to spawn is skipped below.
            let tab_extras = extras.and_then(|extras| extras.tabs.get(plan_index));
            let tab_history = history.and_then(|history| history.tabs.get(plan_index));
            let tab_id = self.next_tab_id;
            self.next_tab_id = self.next_tab_id.wrapping_add(1);

            // Allocate every slot before spawning so the planned tree is independent of spawn
            // order. Failed slots consume their scoped IDs and are then closed out of the tree.
            let slot_ids = (0..planned.spawns.len())
                .map(|_| {
                    let pane_id = self.next_pane_id;
                    self.next_pane_id = self.next_pane_id.wrapping_add(1);
                    pane_id
                })
                .collect::<Vec<_>>();
            let mut failed = HashSet::new();
            for (slot, spec) in planned.spawns.iter().enumerate() {
                if self.spawn_pane(slot_ids[slot], tab_id, spec).is_err() {
                    failed.insert(slot_ids[slot]);
                }
            }

            let mut tree = planned.tiled.as_ref().map(|node| node.to_tiled(&slot_ids));
            for pane_id in slot_ids.iter().filter(|pane_id| failed.contains(pane_id)) {
                tree = tree.and_then(|tree| tree.close(*pane_id));
            }

            let mut floating = FloatingLayer::default();
            for planned_float in &planned.floating {
                let pane_id = slot_ids[planned_float.slot];
                if failed.contains(&pane_id) {
                    continue;
                }
                // A snapshot knows where the float actually sat; a hand-written layout knows its
                // optional percent edge; neither means centered with the layer's cascade.
                let restored = tab_extras.and_then(|tab| {
                    tab.floats
                        .iter()
                        .find(|float| float.slot == planned_float.slot)
                });
                floating.insert(
                    pane_id,
                    area,
                    FloatOrigin {
                        width_percent: planned_float.width_percent,
                        height_percent: planned_float.height_percent,
                        x_percent: restored
                            .map(|float| float.x_percent)
                            .or(planned_float.x_percent),
                        y_percent: restored
                            .map(|float| float.y_percent)
                            .or(planned_float.y_percent),
                    },
                );
                floating.set_pinned(pane_id, planned_float.pinned);
            }

            if tree.is_none() && floating.is_empty() {
                continue;
            }
            let first_tiled = tree
                .as_ref()
                .and_then(|tree| tree.pane_ids().into_iter().next());
            let requested_focus = planned
                .focus_slot
                .map(|slot| slot_ids[slot])
                .filter(|pane_id| !failed.contains(pane_id));
            let last_focused_tiled = requested_focus
                .filter(|pane_id| tree.as_ref().is_some_and(|tree| tree.contains(*pane_id)))
                .or(first_tiled);
            let focused = requested_focus
                .or_else(|| floating.focus_candidate())
                .or(last_focused_tiled)
                .expect("a surviving layout tab has a focusable pane");
            // A zoomed slot whose pane failed to spawn restores unzoomed rather than zooming
            // something else: zoom names one leaf, and the wrong leaf is worse than none.
            let zoomed = tab_extras
                .and_then(|tab| tab.zoomed)
                .and_then(|slot| slot_ids.get(slot).copied())
                .filter(|pane_id| !failed.contains(pane_id))
                .filter(|pane_id| tree.as_ref().is_some_and(|tree| tree.contains(*pane_id)));
            if extras.is_some_and(|extras| extras.active_tab == plan_index) {
                restored_active = Some(self.tabs.len());
            }
            if let Some(tab_extras) = tab_extras {
                self.restore_pane_names(tab_extras, &slot_ids, &failed);
                self.arm_pane_resumes(tab_extras, &slot_ids, &failed, &mut resumed_sessions);
            }
            if let Some(tab_history) = tab_history {
                for pane_history in &tab_history.panes {
                    let Some(pane_id) = slot_ids.get(pane_history.slot).copied() else {
                        continue;
                    };
                    if failed.contains(&pane_id) {
                        continue;
                    }
                    // An agent about to reopen its own conversation repaints its own transcript.
                    // Replaying the screen underneath it would show that transcript twice, so a
                    // pane with a resume armed gets none.
                    if self
                        .panes
                        .get(&pane_id)
                        .is_some_and(|pane| pane.pending_resume.is_some())
                    {
                        continue;
                    }
                    let rows = pane_history
                        .rows
                        .iter()
                        .map(|row| (history_cells(row, &pane_history.styles), row.wrapped))
                        .collect::<Vec<_>>();
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        // Into scrollback, never onto the screen: the viewport belongs to the shell
                        // that just started, and painting over it would leave a dead screen with a
                        // live prompt on top of it.
                        pane.terminal.restore_history(rows);
                    }
                }
            }
            self.tabs.push(Tab {
                id: tab_id,
                name: planned.name,
                tree,
                floating,
                focused,
                last_focused_tiled,
                zoomed,
                sync_input: tab_extras.is_some_and(|tab| tab.sync_input),
            });
        }
        if self.tabs.is_empty() {
            return self.new_tab();
        }
        // Clamped by construction: `restored_active` is only ever an index this loop pushed.
        self.active_tab = restored_active.unwrap_or(0);
        self.schedule_render();
        Ok(())
    }

    /// Capture the session and replace `path` atomically, reporting the saved tab and pane counts.
    ///
    /// The rendered file is small and bounded by the layout caps, and the writer stays on the
    /// actor for the same reason config reload does: one parse-render-write path, no shared state.
    pub(super) fn save_layout(&self, path: &Path) -> io::Result<(usize, usize)> {
        let layout = self.capture_layout()?;
        let rendered = layout.render()?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        fs::write(&temporary, rendered.as_bytes())?;
        if let Err(error) = crate::runtime::atomic_replace(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(layout.counts())
    }

    pub(super) fn spawn_pane(
        &mut self,
        pane_id: PaneId,
        tab_id: u64,
        spec: &PaneSpawn,
    ) -> io::Result<()> {
        let shell = self
            .config
            .general
            .shell
            .as_ref()
            .map(|path| OsString::from(path.as_os_str()))
            .or_else(default_shell)
            .unwrap_or_else(fallback_shell);
        #[cfg(windows)]
        let shell = crate::platform::resolve_windows_executable(&shell).unwrap_or(shell);
        let cwd = spec
            .cwd
            .clone()
            .or_else(|| self.config.general.default_cwd.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(fallback_cwd);
        let term = if terminfo_installed() {
            "vvmux"
        } else {
            "xterm-256color"
        };
        let vvmux_bin = std::env::current_exe()?.to_string_lossy().into_owned();
        // The caller's extras go first so the fixed pane identity below always wins: nothing a
        // layout file or `run` supplies may shadow VIVID_ROOT_SECRET or VVMUX_PANE_ID.
        let mut environment: Vec<(String, String)> = spec.extra_env.clone();
        environment.extend([
            ("TERM".into(), term.into()),
            ("TERM_PROGRAM".into(), "vvmux".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("VVMUX_SESSION".into(), self.name.clone()),
            ("VVMUX_TAB_ID".into(), tab_id.to_string()),
            ("VVMUX_PANE_ID".into(), pane_id.to_string()),
            ("VVMUX_BIN".into(), vvmux_bin),
            // Ambient agent-mesh coordinates. Three strings, no dependency: `vvagent` in this
            // pane derives its runtime instance and position from them instead of being told, so
            // an agent started here can address agents in other sessions without vvmux linking the
            // mesh at all. Everything else about the mesh — the store, activation, policy — lives
            // outside.
            //
            // A session is its own runtime instance even when it runs inside a Vivida pane. Its
            // panes must not carry the host's position: the session outlives the window that
            // started it and can be reattached from another one, which would leave every address
            // pointing at a window that is no longer presenting it.
            ("AGENT_MESH_RUNTIME".into(), "vvmux".into()),
            ("AGENT_MESH_INSTANCE".into(), self.name.clone()),
        ]);
        if let Some(address) = mesh_address(tab_id, pane_id) {
            environment.push(("AGENT_MESH_ADDRESS".into(), address));
        }
        let birth_metrics = if spec.vivid_capability {
            let display = self.layout_display();
            let cells = vivid_cell_size(display.cell_width, display.cell_height);
            environment.extend([
                ("VIVID_ENDPOINT_CONTROL".into(), self.vivid.endpoint()),
                // The daemon can be reattached remotely; pane audio belongs to the presenter.
                ("VIVID_AUDIO_FALLBACK".into(), "deny".into()),
                (
                    "VIVID_ROOT_SECRET".into(),
                    self.vivid.issue_pane_capability(pane_id)?,
                ),
            ]);
            #[cfg(windows)]
            environment.push(("VIVID_ANCHOR_TRANSPORT".into(), "conpty".into()));
            // Publish the birth geometry before the child runs. The next relayout replaces it,
            // but a producer that connects first — or in a session no client has attached to,
            // where no relayout comes — must find a presentation target rather than be refused.
            self.vivid.update_metrics(pane_id, 80, 22, cells);
            Some((80, 22, cells.0, cells.1))
        } else {
            None
        };
        // Every failure past `issue_pane_capability` must revoke it: the capability is already
        // minted, and leaving it live would let a dead pane's secret authenticate.
        let spawned = match spec.argv.as_deref() {
            Some([program, arguments @ ..]) => {
                PtyProcess::spawn_argv(program, arguments, &cwd, 80, 22, &environment)
            }
            Some([]) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pane argv must contain a program",
            )),
            None if self.config.general.microphone && birth_metrics.is_some() && cfg!(unix) => {
                environment.push((
                    "VVMIC_LABEL".into(),
                    format!("{} pane {}", self.name, pane_id),
                ));
                let mut arguments = vec![
                    OsString::from("run"),
                    OsString::from("--"),
                    shell.as_os_str().to_owned(),
                ];
                if let Some(command) = &spec.command {
                    arguments.extend(["-c".into(), command.clone()]);
                } else {
                    arguments.push("-l".into());
                }
                PtyProcess::spawn_argv(
                    std::ffi::OsStr::new("vvmic"),
                    &arguments,
                    &cwd,
                    80,
                    22,
                    &environment,
                )
            }
            None => PtyProcess::spawn(&shell, spec.command.as_deref(), &cwd, 80, 22, &environment),
        };
        let parts = match spawned {
            Ok(parts) => parts,
            Err(error) => {
                if birth_metrics.is_some() {
                    self.vivid.revoke_pane(pane_id);
                }
                return Err(error);
            }
        };
        let child_pid = parts.child_pid;
        let reader_sender = self.sender.clone();
        let mut reader = parts.reader;
        std::thread::Builder::new()
            .name(format!("vvmux-pty-{pane_id}"))
            .spawn(move || {
                let mut buffer = vec![0_u8; PTY_READ_CHUNK];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            if reader_sender
                                .send(ActorEvent::PtyOutput(pane_id, buffer[..read].to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })?;
        let exit_sender = self.sender.clone();
        let waiter = parts.waiter;
        std::thread::Builder::new()
            .name(format!("vvmux-wait-{pane_id}"))
            .spawn(move || {
                let status = waiter.wait().ok();
                let _ = exit_sender.send(ActorEvent::PtyExit(pane_id, status));
            })?;
        self.panes.insert(
            pane_id,
            Pane {
                id: pane_id,
                terminal: Terminal::new(22, 80, self.config.general.scrollback_lines),
                input: parts.input,
                control: parts.control,
                child_pid,
                spawn_cwd: cwd.clone(),
                agent: AgentRuntime::new(),
                agent_published: None,
                agent_change_seq: 0,
                pending_resume: None,
                pending_alias: None,
                transcript: PaneTranscript::default(),
                name: None,
                copy: None,
                mouse_selection: None,
                vivid_metrics: birth_metrics,
                capture_scale: None,
                transparent: spec.transparent.unwrap_or(self.config.panes.transparent),
                hold_on_exit: spec.hold_on_exit,
                exit_status: None,
                focus_reported: false,
                key_paste: false,
                last_input_warning: None,
                screen_sequence: 1,
                last_screen_change: Instant::now(),
                screen_changes: VecDeque::new(),
                role: spec.role.clone(),
                spawn: PaneSpawn {
                    cwd: Some(cwd),
                    ..spec.clone()
                },
            },
        );
        self.refresh_agent_detector_targets();
        self.publish_plugin_event(
            "pane.opened",
            serde_json::json!({"pane_id": pane_id, "tab_id": tab_id}),
            Some(pane_id),
            None,
        );
        Ok(())
    }

    pub(super) fn close_pane(&mut self, pane_id: PaneId) {
        let tab_id = self
            .tabs
            .iter()
            .find(|tab| tab.contains(pane_id))
            .map(|tab| tab.id);
        self.release_pane(pane_id);
        let Some(tab_index) = self.tabs.iter().position(|tab| tab.contains(pane_id)) else {
            return;
        };
        let tab = &mut self.tabs[tab_index];
        if !tab.floating.remove(pane_id)
            && let Some(tree) = tab.tree.take()
        {
            tab.tree = tree.close(pane_id);
        }
        tab.zoomed = tab.zoomed.filter(|pane| *pane != pane_id);
        tab.last_focused_tiled = tab.last_focused_tiled.filter(|pane| *pane != pane_id);
        if tab.is_empty() {
            // A tab lives while either class has panes; it closes with its last pane.
            self.tabs.remove(tab_index);
        } else if tab.focused == pane_id
            && let Some(next) = tab.fallback_focus()
        {
            tab.set_focus(next);
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
        self.tab_rename = self
            .tab_rename
            .take()
            .filter(|rename| self.tabs.iter().any(|tab| tab.id == rename.tab_id));
        self.close_pane_confirmation = self.close_pane_confirmation.take().filter(|confirmation| {
            self.tabs
                .iter()
                .any(|tab| tab.id == confirmation.tab_id && tab.contains(confirmation.pane_id))
        });
        self.force_full = true;
        self.relayout();
        self.publish_plugin_event(
            "pane.closed",
            serde_json::json!({"pane_id": pane_id, "tab_id": tab_id}),
            Some(pane_id),
            None,
        );
    }

    /// End a pane's process and everything scoped to it, leaving its tab slot to the caller:
    /// `close_pane` collapses the slot, `respawn_pane` has already handed it to a new pane.
    pub(super) fn release_pane(&mut self, pane_id: PaneId) {
        if self.direct_pane() == Some(pane_id) {
            self.detach_all_clients(&format!("directly attached pane {pane_id} closed"));
        }
        // A lease on a pane that no longer exists would keep refusing requests for a pane nobody
        // can reach, until it expired.
        self.leases.forget_pane(pane_id);
        if let Some(pending) = self.alt_reads.remove(&pane_id) {
            self.reply_automation_error(
                pending.reply,
                AutomationError::new("pane_not_found", "pane closed during alternate-screen read"),
            );
        }
        self.invalidate_mouse_selection_for_pane(pane_id);
        self.clear_pane_hover(pane_id);
        // Pane IDs are reused, so a stale floor would silence the first notification of whatever
        // owns this ID next.
        self.last_notified.remove(&pane_id);
        if let Some(drag) = &self.pointer_drag {
            self.cancel_pointer_drag(drag.pane() != Some(pane_id));
        }
        if let Some(modal) = self.float_modal {
            // A closing edited pane discards the mode; any other close still invalidates it
            // but restores the entry rectangle.
            self.end_float_mode(modal.pane != pane_id);
        }
        if let Some(pane) = self.panes.remove(&pane_id) {
            pane.control.terminate();
        }
        self.refresh_agent_detector_targets();
        self.vivid.revoke_pane(pane_id);
    }

    /// Kill a pane's process and run its original command again in the same slot.
    ///
    /// The fresh process is a new pane with a new ID, as it has to be: the old process's reader
    /// and exit waiter are still addressing the old ID, and its Vivid capability and media belong
    /// to that ID alone. Only the slot, focus, zoom, transparency, and name carry over.
    pub(super) fn respawn_pane(&mut self, pane_id: PaneId) {
        let Some(pane) = self.panes.get(&pane_id) else {
            return;
        };
        if !matches!(pane.role, PaneRole::Core) {
            self.status("plugin panes cannot be respawned");
            return;
        }
        let spec = PaneSpawn {
            transparent: Some(pane.transparent),
            ..pane.spawn.clone()
        };
        let name = pane.name.clone();
        let Some(tab_id) = self
            .tabs
            .iter()
            .find(|tab| tab.contains(pane_id))
            .map(|tab| tab.id)
        else {
            return;
        };
        let new_id = self.next_pane_id;
        if self.spawn_pane(new_id, tab_id, &spec).is_err() {
            self.status("could not respawn pane");
            return;
        }
        self.next_pane_id += 1;
        self.release_pane(pane_id);
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            if !tab
                .tree
                .as_mut()
                .is_some_and(|tree| tree.rename_pane(pane_id, new_id))
            {
                tab.floating.replace(pane_id, new_id);
            }
            if tab.focused == pane_id {
                tab.focused = new_id;
            }
            if tab.zoomed == Some(pane_id) {
                tab.zoomed = Some(new_id);
            }
            if tab.last_focused_tiled == Some(pane_id) {
                tab.last_focused_tiled = Some(new_id);
            }
        }
        if let Some(pane) = self.panes.get_mut(&new_id) {
            pane.name = name;
        }
        self.session_sequence = self.session_sequence.wrapping_add(1);
        self.force_full = true;
        self.relayout();
        self.publish_plugin_event(
            "pane.closed",
            serde_json::json!({"pane_id": pane_id, "tab_id": tab_id}),
            Some(pane_id),
            None,
        );
    }

    pub(super) fn close_plugin_panes(&mut self, plugin_id: &str, package_digest: &str) {
        let panes = self
            .panes
            .iter()
            .filter_map(|(pane_id, pane)| {
                plugin_pane_matches_generation(
                    &pane.role,
                    &self.session_instance,
                    plugin_id,
                    package_digest,
                )
                .then_some(*pane_id)
            })
            .collect::<Vec<_>>();
        for pane_id in panes {
            self.close_pane(pane_id);
        }
    }

    pub(super) fn close_all_plugin_panes(&mut self) {
        let panes = self
            .panes
            .iter()
            .filter_map(|(pane_id, pane)| {
                matches!(pane.role, PaneRole::Plugin(_)).then_some(*pane_id)
            })
            .collect::<Vec<_>>();
        for pane_id in panes {
            self.close_pane(pane_id);
        }
    }

    /// Start the session-scoped plugin machinery after a live false-to-true config transition.
    pub(super) fn enable_plugin_runtime(&mut self) -> Result<(), String> {
        if self.plugin_supervisor.is_some() {
            return Ok(());
        }
        let (supervisor, agent_catalog_generation, agent_catalog) =
            crate::plugin_supervisor::PluginSupervisor::start(
                self.name.clone(),
                self.session_instance.clone(),
                self.sender.clone(),
            )
            .map_err(|error| format!("could not start plugin supervisor: {error}"))?;
        let watcher_shutdown = Arc::new(AtomicBool::new(false));
        if let Err(error) = crate::plugin::registry_path().and_then(|path| {
            crate::config_watch::spawn_plugin_registry(
                path,
                self.sender.clone(),
                Arc::clone(&watcher_shutdown),
                Arc::clone(&self.plugin_reload_pending),
            )
        }) {
            watcher_shutdown.store(true, Ordering::Release);
            supervisor.shutdown();
            return Err(format!("could not start plugin registry watcher: {error}"));
        }
        self.plugin_supervisor = Some(supervisor);
        self.plugin_watch_shutdown = Some(watcher_shutdown);
        // Same reasoning as the initial startup scan: apply it now rather than waiting for the
        // async `AgentCatalogApplied` event, so a command issued right after this reload sees the
        // agents this scan found.
        self.agent_catalog_generation = agent_catalog_generation;
        self.agent_catalog = agent_catalog;
        self.agent_detector
            .replace_catalog(Arc::clone(&self.agent_catalog));
        for pane in self.panes.values_mut() {
            if pane.agent.reconcile_catalog(&self.agent_catalog) {
                pane.terminal.clear_agent_osc();
            }
        }
        Ok(())
    }

    pub(super) fn disable_plugin_runtime(&mut self) {
        if let Some(stop) = self.plugin_watch_shutdown.take() {
            stop.store(true, Ordering::Release);
        }
        self.plugin_reload_pending.store(false, Ordering::Release);
        self.close_all_plugin_panes();
        if let Some(supervisor) = self.plugin_supervisor.take() {
            supervisor.shutdown();
        }
        self.agent_catalog = Arc::new(crate::agent::AgentCatalog::default());
        self.agent_detector
            .replace_catalog(Arc::clone(&self.agent_catalog));
        for pane in self.panes.values_mut() {
            if pane.agent.reconcile_catalog(&self.agent_catalog) {
                pane.terminal.clear_agent_osc();
            }
        }
        for (_, subscription) in self.plugin_event_subscriptions.drain() {
            subscription.cancel.cancel();
        }
        self.plugin_registration_generation = 0;
        self.plugin_keybindings.clear();
        self.plugin_link_handlers.clear();
        self.plugin_link_press = None;
        self.send_plugin_keymap();
    }

    /// Re-read the config file and adopt what can be adopted without disturbing live state.
    ///
    /// Three settings resist reloading, and each is handled rather than ignored:
    ///
    /// - `[media]` was moved into the running `VirtualVivid` at startup. Swapping it would strand
    ///   live retained media and in-flight tracks, so the running values are carried forward.
    /// - `general.prefix` and `[keys.prefix]` are interpreted by the *client's* prefix parser, and
    ///   `[server]` only by `vvmux serve`. Both are stored, but neither reaches this process's
    ///   behavior until the peer restarts or reattaches.
    /// - `shell`, `default_cwd`, and `scrollback_lines` are read when a pane spawns, so they apply
    ///   to the next pane rather than existing ones. `default_layout` is a next-session setting.
    ///
    /// A parse or validation failure leaves the running config completely untouched: a config
    /// saved mid-edit must never degrade a live session.
    pub(super) fn reload_config(&mut self) -> Result<ReloadReport, String> {
        let path = self
            .config_path
            .clone()
            .or_else(crate::config::default_path)
            .ok_or_else(|| "no config file could be resolved for this session".to_owned())?;
        let source = std::fs::read_to_string(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                format!("{} does not exist", path.display())
            } else {
                format!("could not read {}: {error}", path.display())
            }
        })?;
        let mut next = crate::config::Config::parse(&source, &path).map_err(|error| {
            // `parse` already names the file and the offending key.
            error.to_string()
        })?;

        let mut report = ReloadReport {
            path: path.display().to_string(),
            applied: Vec::new(),
            ignored: Vec::new(),
            deferred: Vec::new(),
            failed: BTreeMap::new(),
        };

        // MediaConfig comes from vivid_sdk::presenter and does not derive PartialEq; comparing the
        // serialized form avoids depending on that.
        let media_changed =
            serde_json::to_value(&next.media).ok() != serde_json::to_value(&self.config.media).ok();
        if media_changed {
            next.media = self.config.media.clone();
            report.ignored.push("media".to_owned());
        }
        if next.general.prefix != self.config.general.prefix {
            report.deferred.push("general.prefix".to_owned());
        }
        if next.keys.prefix != self.config.keys.prefix {
            report.deferred.push("keys.prefix".to_owned());
        }
        // The client reads its own config at attach and owns the sound command, exactly as it owns
        // prefix parsing. The rest of `[notifications]` is session policy and applies at once.
        if next.notifications.sound_command != self.config.notifications.sound_command {
            report
                .deferred
                .push("notifications.sound_command".to_owned());
        }
        if next.general.shell != self.config.general.shell
            || next.general.microphone != self.config.general.microphone
            || next.general.default_cwd != self.config.general.default_cwd
            || next.general.scrollback_lines != self.config.general.scrollback_lines
        {
            report.deferred.push("general.pane_defaults".to_owned());
        }
        if next.general.default_layout != self.config.general.default_layout {
            report.deferred.push("general.default_layout".to_owned());
        }
        // Only the seed for a newly spawned pane. Panes already open keep whatever they were last
        // toggled to, which a reload has no business overriding.
        if next.panes.transparent != self.config.panes.transparent {
            report.deferred.push("panes.transparent".to_owned());
        }
        let server_changed = serde_json::to_value(&next.server).ok()
            != serde_json::to_value(&self.config.server).ok();
        if server_changed {
            next.server = self.config.server.clone();
            report.ignored.push("server".to_owned());
        }

        if next.plugins.enabled != self.config.plugins.enabled {
            if next.plugins.enabled {
                if let Err(error) = self.enable_plugin_runtime() {
                    next.plugins.enabled = false;
                    report.failed.insert("plugins.enabled".into(), error);
                } else {
                    report.applied.push("plugins.enabled".into());
                }
            } else {
                self.disable_plugin_runtime();
                report.applied.push("plugins.enabled".into());
            }
        }

        let tab_view_changed = next.general.tab_view != self.config.general.tab_view;
        let window_size_changed = next.general.window_size != self.config.general.window_size;
        let snapshot_changed = next.session.auto_snapshot != self.config.session.auto_snapshot
            || next.session.pane_history != self.config.session.pane_history;
        self.config = next;
        self.config_path = Some(path);

        if snapshot_changed {
            self.apply_snapshot_setting();
            report.applied.push("session.auto_snapshot".into());
        }
        if window_size_changed {
            self.refresh_layout_display();
            report.applied.push("general.window_size".into());
        }

        if tab_view_changed && self.tab_view != self.config.general.tab_view {
            // An edited `tab_view` is a request to switch now, whatever the session cycled to.
            self.set_tab_view(self.config.general.tab_view);
        } else {
            // Colors can change without any geometry moving, and a cell whose only difference is
            // its color still diffs correctly; force_full is simply the cheapest way to be sure.
            self.force_full = true;
            self.schedule_render();
        }
        self.queue_plugin_state_event(
            "config.changed",
            "config".into(),
            serde_json::json!({"path": report.path}),
            None,
        );
        Ok(report)
    }

    pub(super) fn relayout(&mut self) {
        self.invalidate_mouse_selection_state();
        self.mark_snapshot_dirty();
        self.layout_revision = self.layout_revision.wrapping_add(1);
        self.session_sequence = self.session_sequence.wrapping_add(1);
        self.resize_all();
        self.queue_plugin_state_event(
            "layout.changed",
            "layout".into(),
            serde_json::json!({"layout_revision": self.layout_revision}),
            None,
        );
        if self.recorder.is_some() {
            let layout_sequence = self.layout_revision;
            let layout = self.automation_layout(None);
            self.record(crate::record::RecordedEvent::Layout {
                layout_sequence,
                layout,
            });
        }
        self.schedule_render();
    }

    /// Visibility, z-order, pin, and focus changes must refresh the media projection even when
    /// no rectangle changed: occlusion and quota priority depend on them. No PTY resizing is
    /// needed on this path.
    pub(super) fn projection_changed(&mut self) {
        self.mark_snapshot_dirty();
        self.layout_revision = self.layout_revision.wrapping_add(1);
        self.session_sequence = self.session_sequence.wrapping_add(1);
        self.schedule_render();
    }

    /// Host metrics that back layout and the pane geometry published to producers.
    ///
    /// A detached session keeps the last attached host's metrics. `DisplayMetrics::default()` has a
    /// zero cell size, and a relayout while detached (a pane exiting, an automation split) would
    /// otherwise publish a zero-viewport `DISPLAY_CHANGED` — geometry no producer can honor — and
    /// then publish real metrics again on reattach, resizing every live source twice for a host
    /// that never changed.
    pub(super) fn layout_display(&self) -> DisplayMetrics {
        self.last_display
    }

    /// The tab view the attached client sees: a direct pane view has no tab list.
    pub(super) fn effective_tab_view(&self) -> TabView {
        if self.direct_pane().is_none() {
            self.tab_view
        } else {
            TabView::Hidden
        }
    }

    pub(super) fn content_area(&self) -> Rect {
        let display = self.layout_display();
        self.effective_tab_view()
            .content_area(display.columns, display.rows)
    }

    /// Switch the session's tab view. The tab list sits outside the pane area, so the usable
    /// geometry changes with it.
    pub(super) fn set_tab_view(&mut self, view: TabView) {
        if view == self.tab_view {
            return;
        }
        self.tab_view = view;
        self.sidebar_scroll = 0;
        // The stored displays were normalized against the old view and must be re-normalized
        // before anything derives geometry from them.
        let effective = self.effective_tab_view();
        self.last_display = normalized_display(self.last_display, effective);
        for client in self.clients.values_mut() {
            client.display = normalized_display(client.display, effective);
        }
        let area = self.content_area();
        for tab in &mut self.tabs {
            tab.floating.reproportion(area);
        }
        self.cancel_pointer_drag(true);
        self.force_full = true;
        // One relayout for the whole change: it resizes every PTY and schedules the render.
        self.relayout();
    }

    /// Birth geometry for a float created at runtime: the configured default size, which then
    /// re-proportions across host changes until the user moves or resizes the pane.
    pub(super) fn default_float_origin(&self) -> FloatOrigin {
        FloatOrigin::sized(
            self.config.floating.default_width_percent,
            self.config.floating.default_height_percent,
        )
    }

    pub(super) fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub(super) fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    pub(super) fn pane_is_visibly_present(&self, pane_id: PaneId) -> bool {
        if !self.any_client_focused() {
            return false;
        }
        let area = self.content_area();
        self.attached_projections(area)
            .iter()
            .any(|projection| projection.pane_id == pane_id)
    }

    pub(super) fn terminate_children(&mut self) {
        let controls = std::mem::take(&mut self.panes)
            .into_values()
            .map(|pane| pane.control)
            .collect::<Vec<_>>();
        let workers = controls
            .into_iter()
            .filter_map(|control| {
                std::thread::Builder::new()
                    .name("vvmux-pane-shutdown".into())
                    .spawn(move || control.terminate_blocking())
                    .ok()
            })
            .collect::<Vec<_>>();
        for worker in workers {
            let _ = worker.join();
        }
    }
}

/// Rebuild the cells of one restored line.
fn history_cells(row: &HistoryRow, styles: &[HistoryStyle]) -> Vec<Cell> {
    let mut cells = Vec::new();
    for run in &row.runs {
        let style = styles.get(run.style);
        for ch in run.text.chars() {
            let mut cell = Cell {
                ch,
                ..Cell::default()
            };
            if let Some(style) = style {
                cell.foreground = restored_color(style.fg);
                cell.background = restored_color(style.bg);
                cell.bold = style.bold;
                cell.dim = style.dim;
                cell.italic = style.italic;
                cell.underline_style = match style.underline {
                    1 => UnderlineStyle::Single,
                    2 => UnderlineStyle::Double,
                    3 => UnderlineStyle::Curl,
                    4 => UnderlineStyle::Dotted,
                    5 => UnderlineStyle::Dashed,
                    _ => UnderlineStyle::None,
                };
                cell.underline = cell.underline_style != UnderlineStyle::None;
                cell.blink = style.blink;
                cell.inverse = style.inverse;
                cell.hidden = style.hidden;
                cell.strikeout = style.strikeout;
            }
            cells.push(cell);
        }
    }
    cells
}

fn terminfo_installed() -> bool {
    let candidates = [
        std::env::var_os("TERMINFO").map(PathBuf::from),
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".terminfo")),
        Some(PathBuf::from("/usr/share/terminfo")),
        Some(PathBuf::from("/usr/local/share/terminfo")),
    ];
    candidates
        .into_iter()
        .flatten()
        .any(|root| root.join("v/vvmux").exists())
}
