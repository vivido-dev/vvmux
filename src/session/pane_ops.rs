//! Automation operations on panes, tabs, layout, capture, and session inspection.

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
    pub(super) fn automation_split(
        &mut self,
        pane_id: PaneId,
        axis: Axis,
    ) -> Result<serde_json::Value, AutomationError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(pane_id))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        self.check_session_pane_cap()?;
        let tab_id = self.tabs[tab_index].id;
        let mut candidate = self.tabs[tab_index]
            .tree
            .clone()
            .ok_or_else(|| AutomationError::new("unsupported", "tab has no tiled layout"))?;
        if !candidate.contains(pane_id) {
            return Err(AutomationError::new(
                "unsupported",
                "floating panes cannot be split",
            ));
        }
        let new_pane_id = self.next_pane_id;
        candidate
            .split(pane_id, new_pane_id, axis, self.content_area())
            .map_err(|_out_of_range| {
                AutomationError::new("invalid_state", "pane is too small to split")
            })?;
        self.spawn_pane(new_pane_id, tab_id, &PaneSpawn::default())
            .map_err(|error| AutomationError::new("pty_spawn_failed", error.to_string()))?;
        self.next_pane_id = self.next_pane_id.wrapping_add(1);
        self.tabs[tab_index].tree = Some(candidate);
        self.tabs[tab_index].set_focus(new_pane_id);
        self.force_full = true;
        self.relayout();
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "new_pane_id": new_pane_id,
            "tab_id": tab_id,
            "session_sequence": self.session_sequence,
        }))
    }

    /// Write the current layout to a startup layout file.
    ///
    /// Unlike the interactive prompt this never asks before replacing an existing file: an
    /// automation caller named the path itself.
    pub(super) fn automation_save_layout(
        &mut self,
        path: Option<String>,
    ) -> Result<serde_json::Value, AutomationError> {
        let path = crate::layout_file::resolve_save_path(
            path.as_deref().unwrap_or(crate::layout_file::STARTUP_FILE),
        )
        .map_err(|error| AutomationError::new("invalid_argument", error.to_string()))?;
        let (tabs, panes) = self
            .save_layout(&path)
            .map_err(|error| AutomationError::new("save_failed", error.to_string()))?;
        Ok(serde_json::json!({
            "path": path.display().to_string(),
            "tabs": tabs,
            "panes": panes,
            "session_sequence": self.session_sequence,
        }))
    }

    /// Open a pane running one command.
    ///
    /// Ordering mirrors `automation_split`: the tiled tree is cloned and validated *before* any
    /// process is created, so a placement that cannot fit fails without leaving an orphan shell,
    /// and the tree is committed only once the spawn has succeeded.
    pub(super) fn automation_run(
        &mut self,
        anchor: PaneId,
        command: String,
        placement: crate::ipc::RunPlacement,
        cwd: Option<String>,
        hold: bool,
        focus: bool,
    ) -> Result<serde_json::Value, AutomationError> {
        let spec = PaneSpawn {
            command: Some(OsString::from(command)),
            argv: None,
            cwd: cwd.map(PathBuf::from),
            transparent: None,
            hold_on_exit: hold,
            extra_env: Vec::new(),
            role: PaneRole::Core,
            vivid_capability: true,
        };
        self.place_pane(anchor, spec, placement, focus, None)
    }

    /// Validate placement before process creation, then commit one actor-owned pane mutation.
    pub(super) fn place_pane(
        &mut self,
        anchor: PaneId,
        spec: PaneSpawn,
        placement: crate::ipc::RunPlacement,
        focus: bool,
        tab_name: Option<String>,
    ) -> Result<serde_json::Value, AutomationError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(anchor))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        self.check_session_pane_cap()?;
        let new_pane_id = self.next_pane_id;

        let tab_id = match placement {
            crate::ipc::RunPlacement::Split { axis } => {
                let tab_id = self.tabs[tab_index].id;
                let mut candidate = self.tabs[tab_index].tree.clone().ok_or_else(|| {
                    AutomationError::new("unsupported", "tab has no tiled layout")
                })?;
                if !candidate.contains(anchor) {
                    return Err(AutomationError::new(
                        "invalid_state",
                        "cannot split a floating pane",
                    ));
                }
                candidate
                    .split(anchor, new_pane_id, axis, self.content_area())
                    .map_err(|_out_of_range| {
                        AutomationError::new("invalid_state", "pane is too small to split")
                    })?;
                self.spawn_pane(new_pane_id, tab_id, &spec)
                    .map_err(|error| AutomationError::new("spawn_failed", error.to_string()))?;
                self.tabs[tab_index].tree = Some(candidate);
                tab_id
            }
            crate::ipc::RunPlacement::Float => {
                let tab_id = self.tabs[tab_index].id;
                self.spawn_pane(new_pane_id, tab_id, &spec)
                    .map_err(|error| AutomationError::new("spawn_failed", error.to_string()))?;
                let area = self.content_area();
                let origin = self.default_float_origin();
                self.tabs[tab_index]
                    .floating
                    .insert(new_pane_id, area, origin);
                tab_id
            }
            crate::ipc::RunPlacement::Tab => {
                let tab_id = self.next_tab_id;
                self.spawn_pane(new_pane_id, tab_id, &spec)
                    .map_err(|error| AutomationError::new("spawn_failed", error.to_string()))?;
                self.tabs.push(Tab {
                    id: tab_id,
                    name: tab_name,
                    tree: Some(TiledNode::leaf(new_pane_id)),
                    floating: FloatingLayer::default(),
                    focused: new_pane_id,
                    last_focused_tiled: Some(new_pane_id),
                    zoomed: None,
                    sync_input: false,
                });
                self.next_tab_id += 1;
                tab_id
            }
        };

        self.next_pane_id = self.next_pane_id.wrapping_add(1);
        if focus {
            // A new tab is already focused on its own pane; only move the active tab when the
            // caller asked for focus.
            if let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) {
                self.tabs[index].set_focus(new_pane_id);
                self.active_tab = index;
            }
        }
        self.force_full = true;
        self.relayout();
        Ok(serde_json::json!({
            "pane_id": new_pane_id,
            "tab_id": tab_id,
            "session_sequence": self.session_sequence,
        }))
    }

    pub(super) fn automation_focus(&mut self, pane_id: PaneId) -> Result<(), AutomationError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(pane_id))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        let tab = &mut self.tabs[tab_index];
        if tab
            .floating
            .get(pane_id)
            .is_some_and(|floating| !floating.pinned)
        {
            tab.floating.ordinary_visible = true;
        }
        if tab.zoomed.is_some_and(|zoomed| zoomed != pane_id) {
            tab.zoomed = None;
        }
        tab.set_focus(pane_id);
        self.active_tab = tab_index;
        self.force_full = true;
        self.relayout();
        Ok(())
    }

    pub(super) fn finish_selection(
        &mut self,
        target: AutomationReplyTarget,
        wait: Option<AutomationCompletion>,
        timeout_ms: u64,
        before_outer: u64,
        result: serde_json::Value,
    ) {
        let Some(level) = wait else {
            self.reply_automation(target, result);
            return;
        };
        let supported = match level {
            AutomationCompletion::Outer => {
                self.presenter_client().is_some_and(|client| client.vivid)
            }
            AutomationCompletion::Rendered => !self.clients.is_empty(),
        };
        if !supported {
            let message = match level {
                AutomationCompletion::Outer => {
                    "no attached Vivid-capable foreground client can acknowledge media projection"
                }
                AutomationCompletion::Rendered => {
                    "no attached client can acknowledge the terminal frame"
                }
            };
            self.reply_automation_error(
                target,
                AutomationError::new("missing_attachment", message),
            );
            return;
        }
        self.add_automation_waiter(AutomationWaiter {
            reply: target,
            pane_id: None,
            deadline: deadline(timeout_ms),
            kind: AutomationWaitKind::Completion {
                level,
                after_outer: before_outer,
                after_session: self.session_sequence,
                result,
            },
        });
    }

    /// Which tab a selector means, as an index into `self.tabs`.
    pub(super) fn resolve_tab(&self, selector: &TabSelector) -> Result<usize, AutomationError> {
        match selector {
            TabSelector::Id(tab_id) => self
                .tabs
                .iter()
                .position(|tab| tab.id == *tab_id)
                .ok_or_else(|| {
                    AutomationError::new("tab_not_found", format!("tab {tab_id} does not exist"))
                }),
            TabSelector::Name(name) => {
                // Case-insensitive, like Vivida's tab locators: a name is typed by a person, and
                // "Logs" and "logs" are the same tab to everyone except a byte comparison.
                let mut matches = self.tabs.iter().enumerate().filter(|(_, tab)| {
                    tab.name
                        .as_deref()
                        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
                });
                let first = matches.next().map(|(index, _)| index).ok_or_else(|| {
                    AutomationError::new("tab_not_found", format!("no tab is named {name}"))
                })?;
                // Ambiguity is refused rather than resolved by position: acting on the wrong tab is
                // worse than saying the name does not identify one.
                if matches.next().is_some() {
                    return Err(AutomationError::new(
                        "invalid_params",
                        format!("more than one tab is named {name}; use --tab-id"),
                    ));
                }
                Ok(first)
            }
            TabSelector::Active => (!self.tabs.is_empty())
                .then_some(self.active_tab)
                .ok_or_else(|| AutomationError::new("tab_not_found", "session has no tabs")),
        }
    }

    /// Every pane in a tab with the rectangle it would occupy unzoomed.
    ///
    /// Deliberately not [`visible_projections`], which collapses a zoomed tab to its single
    /// zoomed pane. Zoom is a projection of one leaf and never changes the tree, so a topology
    /// answer that disappeared while a pane was zoomed would be describing the screen rather than
    /// the layout. Zoom is reported per pane instead.
    pub(super) fn topology_projections(tab: &Tab, area: Rect) -> Vec<PaneProjection> {
        let mut projections = Vec::new();
        if let Some(tree) = &tab.tree {
            for (pane_id, outer) in tree.geometry(area) {
                projections.push(PaneProjection {
                    pane_id,
                    outer,
                    content: outer.content(),
                    layer: PaneLayer::Tiled,
                    focused: tab.focused == pane_id,
                });
            }
        }
        for float in tab.floating.panes() {
            projections.push(PaneProjection {
                pane_id: float.pane_id,
                outer: float.rect,
                content: float.rect.content(),
                layer: if float.pinned {
                    PaneLayer::Pinned
                } else {
                    PaneLayer::Floating
                },
                focused: tab.focused == float.pane_id,
            });
        }
        projections
    }

    /// The whole session's shape: tabs, panes, tree positions, rectangles, and neighbors.
    ///
    /// `caller` locates the pane the request came from, taken from the inherited `VVMUX_PANE_ID`,
    /// so an agent can ask "where am I" and "what is next to me" in the same call it uses to
    /// discover the session.
    ///
    /// `neighbors` is the *navigation* graph, not a strict geometric one: each entry is where one
    /// step in that direction lands, computed by the same [`directional_focus`] that
    /// `Action::Focus` uses. That is deliberate — `resolve_pane` and moving focus must agree, or an
    /// agent resolves one pane and focuses another. It also means a direction can point at a pane
    /// that is not strictly on that side: from a full-height left pane, "up" lands on the upper of
    /// the two panes to its right, because that is where focus would go. Read `geometry` when the
    /// question is really about position on screen.
    pub(super) fn automation_layout(&self, caller: Option<PaneId>) -> serde_json::Value {
        let area = self.content_area();
        let caller = caller.filter(|pane_id| self.panes.contains_key(pane_id));
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let projections = Self::topology_projections(tab, area);
                let panes = projections
                    .iter()
                    .map(|projection| {
                        let pane_id = projection.pane_id;
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
                                serde_json::to_value(directional_focus(
                                    &projections,
                                    pane_id,
                                    direction,
                                ))
                                .unwrap_or(serde_json::Value::Null),
                            )
                        })
                        .collect::<serde_json::Map<_, _>>();
                        let pane_name = self
                            .panes
                            .get(&pane_id)
                            .and_then(|pane| pane.name.as_ref())
                            .map(ToString::to_string);
                        serde_json::json!({
                            "pane_id": pane_id,
                            "pane_name": pane_name,
                            "locator": {
                                "tab_id": tab.id,
                                "tab_name": tab.name,
                                "pane_name": pane_name,
                                "pane_id": pane_id,
                            },
                            "split_path": tab
                                .tree
                                .as_ref()
                                .and_then(|tree| tree.split_path(pane_id)),
                            "layer": match projection.layer {
                                PaneLayer::Tiled => "tiled",
                                PaneLayer::Floating => "floating",
                                PaneLayer::Pinned => "pinned",
                            },
                            "geometry": rect_json(projection.outer),
                            "content_geometry": rect_json(projection.content),
                            "focused": projection.focused,
                            "visible": self.pane_is_visibly_present(pane_id),
                            "zoomed": tab.zoomed == Some(pane_id),
                            "is_caller": caller == Some(pane_id),
                            "title": self
                                .panes
                                .get(&pane_id)
                                .and_then(|pane| pane.terminal.title()),
                            "agent_alias": self
                                .panes
                                .get(&pane_id)
                                .and_then(|pane| pane.agent.alias())
                                .map(ToString::to_string),
                            "neighbors": neighbors,
                        })
                    })
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "tab_id": tab.id,
                    "tab_name": tab.name,
                    "display_index": index,
                    "active": index == self.active_tab,
                    "focused_pane_id": tab.focused,
                    "zoomed_pane_id": tab.zoomed,
                    "sync_input": tab.sync_input,
                    "panes": panes,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "schema_version": 1,
            "session": self.name,
            "session_instance": self.session_instance,
            "session_sequence": self.session_sequence,
            "layout_sequence": self.layout_revision,
            "active_tab_id": self.active_tab().map(|tab| tab.id),
            "area": rect_json(area),
            "caller": caller.map(|pane_id| {
                serde_json::json!({
                    "pane_id": pane_id,
                    "tab_id": self
                        .tabs
                        .iter()
                        .find(|tab| tab.contains(pane_id))
                        .map(|tab| tab.id),
                })
            }),
            "tabs": tabs,
        })
    }

    /// Walk a directional route, or resolve a name, without touching focus.
    pub(super) fn automation_resolve_pane(
        &self,
        caller: Option<PaneId>,
        selector: Option<&TabSelector>,
        path: &[Direction],
        pane_name: Option<&crate::layout::PaneName>,
    ) -> Result<serde_json::Value, AutomationError> {
        if let Some(name) = pane_name {
            if !path.is_empty() {
                return Err(AutomationError::new(
                    "invalid_params",
                    "--pane-name resolves a pane directly; it cannot also take a --path",
                ));
            }
            let pane_id = self.pane_with_name(name).ok_or_else(|| {
                AutomationError::new("pane_not_found", format!("no pane is named {name}"))
            })?;
            return Ok(serde_json::json!({
                "schema_version": 1,
                "selector": {"pane_name": name.as_str()},
                "steps": [],
                "target": self.resolved_pane_json(pane_id),
            }));
        }
        if path.len() > MAX_RESOLVE_PANE_STEPS {
            return Err(AutomationError::new(
                "limit_exceeded",
                format!("a route may take at most {MAX_RESOLVE_PANE_STEPS} steps"),
            ));
        }
        let caller = caller.filter(|pane_id| self.panes.contains_key(pane_id));
        // With no tab named, the route starts where the caller is, even when that tab is not the
        // active one: an agent asking for "the pane to my left" means its own tab.
        let tab_index = match selector {
            Some(selector) => self.resolve_tab(selector)?,
            None => caller
                .and_then(|pane_id| self.tabs.iter().position(|tab| tab.contains(pane_id)))
                .or((!self.tabs.is_empty()).then_some(self.active_tab))
                .ok_or_else(|| AutomationError::new("tab_not_found", "session has no tabs"))?,
        };
        let tab = &self.tabs[tab_index];
        // The caller's own pane when the route stays in its tab, otherwise that tab's focused pane.
        let start = caller
            .filter(|pane_id| tab.contains(*pane_id))
            .unwrap_or(tab.focused);
        if !self.panes.contains_key(&start) {
            return Err(AutomationError::new(
                "no_focused_pane",
                "the selected tab has no resolvable starting pane",
            ));
        }
        let projections = Self::topology_projections(tab, self.content_area());
        let mut current = start;
        let mut steps = Vec::with_capacity(path.len());
        for direction in path {
            let next = directional_focus(&projections, current, *direction).ok_or_else(|| {
                AutomationError::new(
                    "pane_not_found",
                    format!("no pane lies {} of pane {current}", direction.as_str()),
                )
            })?;
            steps.push(serde_json::json!({
                "direction": direction,
                "from_pane_id": current,
                "to_pane_id": next,
            }));
            current = next;
        }
        Ok(serde_json::json!({
            "schema_version": 1,
            "selector": {
                "tab_id": tab.id,
                "tab_name": tab.name,
                "path": path,
            },
            "source_pane_id": start,
            "steps": steps,
            "target": self.resolved_pane_json(current),
        }))
    }

    /// The identity block every resolution returns, so a caller can act on it without a second call.
    pub(super) fn resolved_pane_json(&self, pane_id: PaneId) -> serde_json::Value {
        let tab = self.tabs.iter().find(|tab| tab.contains(pane_id));
        serde_json::json!({
            "pane_id": pane_id,
            "pane_name": self
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.name.as_ref())
                .map(ToString::to_string),
            "tab_id": tab.map(|tab| tab.id),
            "tab_name": tab.and_then(|tab| tab.name.clone()),
            "split_path": tab
                .and_then(|tab| tab.tree.as_ref())
                .and_then(|tree| tree.split_path(pane_id)),
            "visible": self.pane_is_visibly_present(pane_id),
            "focused": tab.is_some_and(|tab| tab.focused == pane_id),
        })
    }

    /// Sanitized process detail for one pane.
    ///
    /// Names only, never argv: a foreground command line routinely carries an agent's session
    /// identity, an API key passed as a flag, or a path a caller has no business seeing, and this
    /// is a broad observation surface rather than a debugging one. The pane's own PID and the
    /// group holding the terminal are enough to tell "the shell is at its prompt" from "a job is
    /// running", which is what a caller is actually asking.
    pub(super) fn pane_process_json(&self, pane: &Pane) -> serde_json::Value {
        let group = pane.control.foreground_process_group_id();
        let foreground = group
            .map(|group| {
                crate::agent::foreground_job(&self.agent_catalog, pane.child_pid, Some(group)).1
            })
            .unwrap_or_default()
            .into_iter()
            .map(|process| serde_json::json!({"pid": process.pid, "name": process.name}))
            .collect::<Vec<_>>();
        serde_json::json!({
            "pid": pane.child_pid,
            "foreground_process_group": group,
            // A group that is not the pane's own child means something claimed the terminal, which
            // is the difference between a shell waiting and a job running under it.
            "foreground_job": group.is_some_and(|group| group != pane.child_pid),
            "foreground": foreground,
            "cwd": pane_cwd(pane.child_pid),
            "spawn_cwd": pane.spawn_cwd.display().to_string(),
            "exit": pane.exit_status.map(|status| {
                serde_json::json!({"code": status.code, "signal": status.signal})
            }),
        })
    }

    /// Resolve a pane-local position to a cell in the session's own coordinate space.
    ///
    /// Pane-local on the way in because a caller should not have to know where a pane sits, and
    /// session-absolute on the way out because that is what every existing mouse path speaks. The
    /// pixel form needs the attached client's cell metrics; without a client there is no scale to
    /// convert by, and guessing one would silently put the click somewhere else.
    pub(super) fn resolve_mouse_position(
        &self,
        content: Rect,
        position: crate::ipc::MousePosition,
    ) -> Result<ResolvedMousePoint, AutomationError> {
        use crate::ipc::MousePosition;
        let display = self.layout_display();
        let (column, row, pixels) = match position {
            MousePosition::Cell { column, row } => (column, row, None),
            MousePosition::Relative { x, y } => {
                if x > 1000 || y > 1000 {
                    return Err(AutomationError::new(
                        "invalid_params",
                        "relative coordinates are per-mille, 0 through 1000",
                    ));
                }
                // Multiply before dividing, and clamp: the last cell must be reachable at 1000
                // without rounding past the edge.
                let column = (u32::from(x) * u32::from(content.width) / 1000)
                    .min(u32::from(content.width.saturating_sub(1)))
                    as u16;
                let row = (u32::from(y) * u32::from(content.height) / 1000)
                    .min(u32::from(content.height.saturating_sub(1)))
                    as u16;
                (column, row, None)
            }
            MousePosition::Pixel { x, y } => {
                let (cell_width, cell_height) = (display.cell_width, display.cell_height);
                if cell_width == 0 || cell_height == 0 {
                    return Err(AutomationError::new(
                        "invalid_state",
                        "pixel coordinates need an attached client's cell metrics; use --cell-column and --cell-row",
                    ));
                }
                let column = (x / u32::from(cell_width)) as u16;
                let row = (y / u32::from(cell_height)) as u16;
                // Session-absolute pixels, which is what the SGR pixel encoder subtracts the
                // pane origin from.
                let absolute = (
                    (u32::from(content.x) * u32::from(cell_width)).saturating_add(x) as u16,
                    (u32::from(content.y) * u32::from(cell_height)).saturating_add(y) as u16,
                );
                (column, row, Some(absolute))
            }
        };
        if column >= content.width || row >= content.height {
            return Err(AutomationError::new(
                "invalid_params",
                format!(
                    "position is outside the pane's {}x{} content area",
                    content.width, content.height
                ),
            ));
        }
        Ok(ResolvedMousePoint {
            x: content.x.saturating_add(column),
            y: content.y.saturating_add(row),
            pixels,
        })
    }

    /// The content rectangle a pane's mouse coordinates are relative to.
    pub(super) fn pane_content_rect(&self, pane_id: PaneId) -> Result<Rect, AutomationError> {
        let area = self.content_area();
        if self.direct_pane() == Some(pane_id) {
            return Ok(area);
        }
        let tab = self
            .tabs
            .iter()
            .find(|tab| tab.contains(pane_id))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        Self::topology_projections(tab, area)
            .into_iter()
            .find(|projection| projection.pane_id == pane_id)
            .map(|projection| projection.content)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no geometry"))
    }

    /// Send one mouse action to a pane.
    pub(super) fn automation_mouse(
        &mut self,
        pane_id: PaneId,
        request: MouseRequest,
    ) -> Result<serde_json::Value, AutomationError> {
        use crate::ipc::{MouseAction, MouseRoute};
        let content = self.pane_content_rect(pane_id)?;
        let before_session = self.session_sequence;

        if request.action == MouseAction::Path {
            return self.automation_mouse_path(pane_id, content, request, before_session);
        }
        let position = request.position.ok_or_else(|| {
            AutomationError::new(
                "invalid_params",
                "this mouse action needs a position; pass --cell-column and --cell-row",
            )
        })?;
        let ResolvedMousePoint { x, y, pixels } = self.resolve_mouse_position(content, position)?;

        let mut sent = 0_usize;
        let mut send = |actor: &mut Self, kind: MouseKind, button: u8| {
            let event = MouseEvent {
                button,
                x,
                y,
                kind,
                shift: request.shift,
                alt: request.alt,
                ctrl: request.ctrl,
            };
            match request.route {
                MouseRoute::Application => actor.send_application_mouse(pane_id, event, pixels),
                // Through the session's own handler, so copy-mode selection, float drag, and
                // focus behave exactly as they do for a real pointer.
                MouseRoute::Mux => actor.mouse(event, pixels.is_some()),
            }
            sent += 1;
        };

        let button = request.button.code();
        match request.action {
            MouseAction::Move => send(self, MouseKind::Move, 0),
            MouseAction::Down => send(self, MouseKind::Press, button),
            MouseAction::Up => send(self, MouseKind::Release, button),
            MouseAction::Click | MouseAction::Drag => {
                send(self, MouseKind::Press, button);
                if request.action == MouseAction::Drag {
                    // A drag with one point is a press and a release at the same place; the motion
                    // report between them is what makes an application treat it as a drag.
                    send(self, MouseKind::Move, button);
                }
                send(self, MouseKind::Release, button);
            }
            MouseAction::DoubleClick => {
                for _ in 0..2 {
                    send(self, MouseKind::Press, button);
                    send(self, MouseKind::Release, button);
                }
            }
            MouseAction::Scroll => {
                if request.scroll == 0 {
                    return Err(AutomationError::new(
                        "invalid_params",
                        "scroll needs a nonzero notch count",
                    ));
                }
                let wheel = if request.scroll < 0 {
                    crate::agent_drive::WHEEL_UP
                } else {
                    crate::agent_drive::WHEEL_DOWN
                };
                for _ in 0..request.scroll.unsigned_abs().min(MAX_MOUSE_SCROLL_NOTCHES) {
                    send(self, MouseKind::Wheel, (wheel & 0xff) as u8);
                }
            }
            MouseAction::Path => unreachable!("handled above"),
        }
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "events": sent,
            "cell": {"column": x.saturating_sub(content.x), "row": y.saturating_sub(content.y)},
            "route": request.route,
            "session_sequence": self.session_sequence,
            "before_session_sequence": before_session,
        }))
    }

    /// One bounded press/move/release gesture over a list of points.
    ///
    /// Delivered in a single request rather than as one call per point: a gesture is a press, a
    /// path, and a release that belong together, and splitting it across processes leaves a button
    /// held down if any of them fails.
    pub(super) fn automation_mouse_path(
        &mut self,
        pane_id: PaneId,
        content: Rect,
        request: MouseRequest,
        before_session: u64,
    ) -> Result<serde_json::Value, AutomationError> {
        use crate::ipc::MouseRoute;
        if !(2..=MAX_MOUSE_PATH_POINTS).contains(&request.points.len()) {
            return Err(AutomationError::new(
                "invalid_params",
                format!("a mouse path takes 2 through {MAX_MOUSE_PATH_POINTS} points"),
            ));
        }
        let resolved = request
            .points
            .iter()
            .map(|point| self.resolve_mouse_position(content, *point))
            .collect::<Result<Vec<_>, _>>()?;
        let button = request.button.code();
        let last = resolved.len() - 1;
        for (index, ResolvedMousePoint { x, y, pixels }) in resolved.into_iter().enumerate() {
            let kind = match index {
                0 => MouseKind::Press,
                index if index == last => MouseKind::Release,
                _ => MouseKind::Move,
            };
            let event = MouseEvent {
                button,
                x,
                y,
                kind,
                shift: request.shift,
                alt: request.alt,
                ctrl: request.ctrl,
            };
            match request.route {
                MouseRoute::Application => self.send_application_mouse(pane_id, event, pixels),
                MouseRoute::Mux => self.mouse(event, pixels.is_some()),
            }
        }
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "events": request.points.len(),
            "route": request.route,
            "session_sequence": self.session_sequence,
            "before_session_sequence": before_session,
        }))
    }

    /// Encode one event through a pane's own terminal modes and write it to that pane's PTY.
    ///
    /// Addressed rather than hit-tested, which is what lets an explicitly targeted event reach a
    /// pane that is hidden, in another tab, or covered by a zoom. The interactive path has to hit
    /// test because it starts from a pointer position with no pane attached to it; this one was
    /// told which pane.
    pub(super) fn send_application_mouse(
        &mut self,
        pane_id: PaneId,
        mouse: MouseEvent,
        pixels: Option<(u16, u16)>,
    ) {
        let display = self.layout_display();
        let Ok(content) = self.pane_content_rect(pane_id) else {
            return;
        };
        let Some(modes) = self.panes.get(&pane_id).map(|pane| pane.terminal.modes()) else {
            return;
        };
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
        if mouse.shift {
            button |= 4;
        }
        let (x, y) =
            application_mouse_coordinates(mouse, pixels, content, display, modes.sgr_pixels);
        let encoded =
            crate::agent_drive::encode_sgr_mouse(button, x, y, mouse.kind != MouseKind::Release);
        self.send_pane_input(pane_id, encoded.as_bytes());
    }

    /// Submit a command line to a pane's shell, once that shell is in a state to accept one.
    pub(super) fn start_shell_command(
        &mut self,
        pane_id: PaneId,
        command: &str,
    ) -> Result<AutomationWaitKind, AutomationError> {
        if command.is_empty() || command.len() > MAX_RUN_COMMAND_BYTES {
            return Err(AutomationError::new(
                "invalid_params",
                format!("a shell command holds 1..={MAX_RUN_COMMAND_BYTES} bytes"),
            ));
        }
        // One line, one command. A newline in the middle would submit two, and the completion
        // marker this waits for would belong to whichever one finished first.
        if command.contains(['\n', '\r']) {
            return Err(AutomationError::new(
                "invalid_params",
                "a shell command is one line; it cannot contain a newline",
            ));
        }
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        if pane.terminal.alternate_screen() {
            return Err(AutomationError::new(
                "not_alternate_screen",
                "the pane is running a full-screen application, not sitting at a shell prompt",
            ));
        }
        let shell = pane.terminal.shell_integration();
        if !shell.is_active() {
            return Err(AutomationError::new(
                "unsupported",
                "this pane's shell does not emit OSC 133 command markers, so a command's exit \
                 status cannot be observed; use `submit` and wait on output instead",
            ));
        }
        if shell.phase != vvmux_terminal::ShellPhase::Prompt {
            return Err(AutomationError::new(
                "invalid_state",
                "the pane's shell is already running a command",
            ));
        }
        let started_screen = pane.screen_sequence;
        let after_command_id = shell.completed_command_id;
        // The same single write `submit_line` uses: the text and its Enter cannot be separated by
        // a failure that would leave half a command line at the prompt.
        self.send_pane_input(pane_id, format!("{command}\r").as_bytes());
        Ok(AutomationWaitKind::ShellCommand {
            after_command_id,
            started_screen,
        })
    }

    /// Start, stop, or report on a session recording.
    pub(super) fn automation_record(
        &mut self,
        operation: &crate::ipc::RecordOperation,
    ) -> Result<serde_json::Value, AutomationError> {
        match operation {
            crate::ipc::RecordOperation::Start { path } => {
                if self.recorder.is_some() {
                    return Err(AutomationError::new(
                        "invalid_state",
                        "this session is already recording",
                    ));
                }
                let path = std::path::PathBuf::from(path);
                if !path.is_absolute() {
                    return Err(AutomationError::new(
                        "invalid_params",
                        "a recording path must be absolute; the session's working directory is                          not the caller's",
                    ));
                }
                let mut recorder = crate::record::Recorder::new(path.clone());
                // The opening frame is the session's shape, so a replay has somewhere to start
                // rather than inferring a layout from whichever panes happen to produce output.
                recorder.push(
                    self.session_sequence,
                    crate::record::RecordedEvent::Opened {
                        session: self.name.clone(),
                        layout: self.automation_layout(None),
                    },
                );
                self.recorder = Some(recorder);
                Ok(serde_json::json!({
                    "recording": true,
                    "path": path.display().to_string(),
                }))
            }
            crate::ipc::RecordOperation::Stop => {
                let recorder = self.recorder.take().ok_or_else(|| {
                    AutomationError::new("invalid_state", "this session is not recording")
                })?;
                recorder
                    .finish()
                    .map_err(|error| AutomationError::new("save_failed", error.to_string()))
            }
            crate::ipc::RecordOperation::Status => Ok(serde_json::json!({
                "recording": self.recorder.is_some(),
                "path": self
                    .recorder
                    .as_ref()
                    .map(|recorder| recorder.path().display().to_string()),
                "events": self.recorder.as_ref().map(crate::record::Recorder::len),
            })),
        }
    }

    /// Note something in the recording, if one is running.
    pub(super) fn record(&mut self, event: crate::record::RecordedEvent) {
        let sequence = self.session_sequence;
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.push(sequence, event);
        }
    }

    /// The client whose Vivido window a pane agent should address: the presenter's, because that
    /// is where the pane's media is visible, else the window someone is using.
    pub(super) fn outer_identity_client(&self) -> Option<&AttachedClient> {
        self.presenter_client()
            .filter(|client| client.outer.is_some())
            .or_else(|| {
                self.clients
                    .values()
                    .filter(|client| client.outer.is_some())
                    .max_by_key(|client| client.activity)
            })
    }

    /// Who is presenting this session, and how to address them.
    ///
    /// Never carries the outer Vivid endpoint or root secret; those stay in the foreground client
    /// by design and this struct exists precisely so the daemon can answer "which window" without
    /// ever holding "how to reach it".
    pub(super) fn outer_identity_json(&self) -> serde_json::Value {
        let Some(outer) = self
            .outer_identity_client()
            .and_then(|client| client.outer.as_ref())
        else {
            return serde_json::Value::Null;
        };
        serde_json::json!({
            "vivido_window_id": outer.vivido_window_id,
            "vivido_session": outer.vivido_session,
            "has_outer_endpoint": outer.has_outer_endpoint,
            "remote": outer.remote,
            "cell_width": outer.cell_width,
            "cell_height": outer.cell_height,
            // Stated rather than left to be worked out: a `vivido msg` from a pane on another
            // machine cannot reach an owner-only local socket, so the route does not exist.
            "vivido_automation_reachable": !outer.remote && outer.vivido_window_id.is_some(),
        })
    }

    /// Where a pane sits inside the presenting Vivido window, in physical pixels.
    ///
    /// The crop a caller applies to `vivido msg screenshot` to get just this pane. Absent when no
    /// client is attached or its cell metrics are unknown, because a rectangle computed from a
    /// guessed cell size would be confidently wrong.
    pub(super) fn pane_outer_crop_json(&self, pane_id: PaneId) -> serde_json::Value {
        let Some(outer) = self
            .outer_identity_client()
            .and_then(|client| client.outer.as_ref())
        else {
            return serde_json::Value::Null;
        };
        let (Ok(content), true) = (
            self.pane_content_rect(pane_id),
            outer.cell_width > 0 && outer.cell_height > 0,
        ) else {
            return serde_json::Value::Null;
        };
        let (cell_width, cell_height) = (u32::from(outer.cell_width), u32::from(outer.cell_height));
        serde_json::json!({
            "x": u32::from(content.x) * cell_width,
            "y": u32::from(content.y) * cell_height,
            "width": u32::from(content.width) * cell_width,
            "height": u32::from(content.height) * cell_height,
            "vivido_window_id": outer.vivido_window_id,
        })
    }

    /// What a pane's layers look like right now, as the baseline a scaled capture waits past.
    ///
    /// Identity is the node plus its raster dimensions and frame id: a re-render at new metrics
    /// changes the dimensions, and a producer that re-sent the same size still advances the frame.
    pub(super) fn capture_baseline(&self, pane_id: PaneId) -> Vec<CaptureIdentity> {
        self.vivid
            .capture_pane(pane_id, 0)
            .layers
            .iter()
            .map(|layer| match &layer.content {
                vivid_sdk::presenter::CaptureContent::Raster(raster) => (
                    layer.node_id,
                    raster.width,
                    raster.height,
                    Some(raster.frame_id),
                ),
                vivid_sdk::presenter::CaptureContent::EncodedImage(_) => {
                    (layer.node_id, 0, 0, None)
                }
            })
            .collect()
    }

    /// Raise a pane's cell metrics so its producer re-renders at a higher density.
    ///
    /// Refused while a client is attached unless forced: the transition is an ordinary resize to
    /// everything downstream, so the human watching the pane sees it change shape and change back.
    pub(super) fn begin_scaled_capture(
        &mut self,
        pane_id: PaneId,
        scale: u32,
        force: bool,
    ) -> Result<Vec<CaptureIdentity>, AutomationError> {
        if !self.panes.contains_key(&pane_id) {
            return Err(AutomationError::new(
                "pane_not_found",
                "pane no longer exists",
            ));
        }
        if self.panes.values().any(|pane| pane.capture_scale.is_some()) {
            return Err(AutomationError::new(
                "capture_in_progress",
                "another scaled capture is still running in this session",
            ));
        }
        if !self.clients.is_empty() && !force {
            return Err(AutomationError::new(
                "capture_would_disturb_client",
                "a scaled capture resizes the pane in view of the attached client; pass --force to allow it",
            ));
        }
        let content = self.pane_content_rect(pane_id)?;
        let (cell_width, cell_height) =
            (self.last_display.cell_width, self.last_display.cell_height);
        if cell_width == 0 || cell_height == 0 {
            return Err(AutomationError::new(
                "cell_metrics_unknown",
                "no client has reported cell metrics, so a pane has no pixel size yet",
            ));
        }
        // Bound the request against the same budget the compositor enforces, before anything is
        // asked to allocate a framebuffer this large.
        let (scaled_width, scaled_height) = scaled_cells(cell_width, cell_height, Some(scale));
        let pixels = u64::from(content.width)
            .checked_mul(u64::from(scaled_width))
            .and_then(|width| {
                u64::from(content.height)
                    .checked_mul(u64::from(scaled_height))
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or_else(|| {
                AutomationError::new("capture_scale_rejected", "scaled pane size overflows")
            })?;
        if pixels > crate::capture_media::MAX_CAPTURE_PIXELS {
            return Err(AutomationError::new(
                "capture_scale_rejected",
                format!(
                    "scale {scale} would need {pixels} pixels, past the {} budget",
                    crate::capture_media::MAX_CAPTURE_PIXELS
                ),
            ));
        }
        let baseline = self.capture_baseline(pane_id);
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.capture_scale = Some(scale);
        }
        self.resize_all();
        Ok(baseline)
    }

    /// Has the producer finished re-rendering? If so, capture; if not, keep waiting.
    ///
    /// Two things make this a settle rather than a single edge. A producer answers the resize by
    /// replacing its raster track, and that new track's first frame is the blank buffer it was
    /// created with, so capturing on the first change captures an empty page. And a producer is
    /// entitled to clamp what it was asked for — `vvrd` at its 16,384px render ceiling, `vrowser`
    /// against its pixel contract — so a wait keyed to the arithmetic ideal would hang forever on
    /// exactly the panes that most need capturing.
    pub(super) fn poll_scaled_capture(
        &mut self,
        pane_id: PaneId,
        path: &str,
        baseline: &[CaptureIdentity],
        settling: Option<&CaptureSettling>,
        now: Instant,
    ) -> ScaledCapturePoll {
        if !self.panes.contains_key(&pane_id) {
            return ScaledCapturePoll::Done(Err(AutomationError::new(
                "pane_not_found",
                "pane closed while its capture was in flight",
            )));
        }
        let current = self.capture_baseline(pane_id);
        match capture_settle_step(baseline, current, settling, now) {
            CaptureSettleStep::Wait(next) => ScaledCapturePoll::Pending(next),
            CaptureSettleStep::Capture => {
                ScaledCapturePoll::Done(self.capture_media_payload(pane_id, path))
            }
        }
    }

    /// Drop a pane back to the client's real cell metrics.
    ///
    /// Called from every exit out of a scaled capture, the timeout and a vanished pane included:
    /// leaving `capture_scale` set would strand the pane at capture density.
    pub(super) fn finish_scaled_capture(&mut self, pane_id: PaneId) {
        let was_scaled = self
            .panes
            .get_mut(&pane_id)
            .is_some_and(|pane| pane.capture_scale.take().is_some());
        if was_scaled {
            self.resize_all();
        }
    }

    /// Compose a pane's retained media and write it out as a PNG.
    ///
    /// Reads the gateway's own retained framebuffers rather than the outer projection, so it is
    /// correct with nothing attached and with the pane hidden. Nothing here activates the pane or
    /// disturbs the projection: this is `observe`-class and stays that way.
    pub(super) fn capture_media_payload(
        &self,
        pane_id: PaneId,
        path: &str,
    ) -> Result<serde_json::Value, AutomationError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        let offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
        let content = self.pane_content_rect(pane_id)?;
        // The scaled cells, not the client's: while a capture is in flight the producer has been
        // asked to render at capture density, so composing against the client's cell size would
        // resample the extra detail straight back out and leave `--scale` doing nothing at all.
        let (cell_width, cell_height) = scaled_cells(
            self.last_display.cell_width,
            self.last_display.cell_height,
            pane.capture_scale,
        );
        if cell_width == 0 || cell_height == 0 {
            return Err(AutomationError::new(
                "cell_metrics_unknown",
                "no client has reported cell metrics, so a pane has no pixel size yet",
            ));
        }
        let target = crate::capture_media::CaptureTarget {
            columns: u32::from(content.width),
            rows: u32::from(content.height),
            cell_width: u32::from(cell_width),
            cell_height: u32::from(cell_height),
        };
        let captured_pane = self.vivid.capture_pane(pane_id, offset);
        let captured = crate::capture_media::compose(&captured_pane.layers, target)
            .map_err(|error| AutomationError::new("capture_failed", error.to_string()))?;
        std::fs::write(path, &captured.png).map_err(|error| {
            AutomationError::new("capture_write_failed", format!("{path}: {error}"))
        })?;
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "path": path,
            "width": captured.width,
            "height": captured.height,
            "bytes": captured.png.len(),
            "cell_width": cell_width,
            "cell_height": cell_height,
            "session_sequence": self.session_sequence,
            // Why a source contributed nothing. A blank capture that explains itself is a fact a
            // caller can act on; one that says nothing reads as a broken request, and the reasons
            // want different responses — encoded video will never appear here, while a track
            // awaiting recovery will as soon as its keyframe lands.
            "skipped": captured_pane
                .skipped
                .iter()
                .map(|entry| serde_json::json!({
                    "producer_id": entry.source.producer,
                    "context_id": entry.source.context,
                    "surface_id": entry.source.surface,
                    "track_id": entry.source.track,
                    "node_id": entry.node_id,
                    "reason": entry.reason.as_str(),
                }))
                .collect::<Vec<_>>(),
            "layers": captured
                .layers
                .iter()
                .map(|layer| serde_json::json!({
                    "producer_id": layer.source.producer,
                    "context_id": layer.source.context,
                    "surface_id": layer.source.surface,
                    "track_id": layer.source.track,
                    "node_id": layer.node_id,
                    "width": layer.source_width,
                    "height": layer.source_height,
                    "frame_id": layer.frame_id,
                    "epoch": layer.epoch,
                    "encoded_image": layer.encoded_image,
                }))
                .collect::<Vec<_>>(),
        }))
    }

    /// Everything `capture` returns once its pane has settled.
    pub(super) fn capture_payload(
        &self,
        pane_id: PaneId,
        grid: bool,
    ) -> Result<serde_json::Value, AutomationError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        let offset = pane.copy.as_ref().map_or(0, |copy| copy.offset);
        let mut result = serde_json::json!({
            "pane_id": pane_id,
            "text": pane.terminal.visible_text(offset),
            "columns": pane.terminal.cols(),
            "rows": pane.terminal.rows(),
            "screen_sequence": pane.screen_sequence,
            "session_sequence": self.session_sequence,
            "output_offset": pane.transcript.offset,
            "visible": self.pane_is_visibly_present(pane_id),
            "geometry": self
                .pane_content_rect(pane_id)
                .ok()
                .map(rect_json),
        });
        if grid {
            result["grid"] = self.grid_snapshot(pane_id, None, None, None)?;
        }
        Ok(result)
    }

    /// Read a pane's rolling output window.
    pub(super) fn automation_transcript(
        &self,
        pane_id: PaneId,
        after_offset: Option<u64>,
        base64: bool,
        max_bytes: Option<u32>,
    ) -> Result<serde_json::Value, AutomationError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        if after_offset.is_some_and(|offset| offset > pane.transcript.offset) {
            return Err(AutomationError::new(
                "sequence_gap",
                format!(
                    "output offset is {}, before requested {}",
                    pane.transcript.offset,
                    after_offset.unwrap_or_default()
                ),
            ));
        }
        let (mut bytes, gap) = pane.transcript.since(after_offset);
        let dropped_before = gap.then(|| pane.transcript.start());
        // Truncate from the front: a caller capping the reply wants the most recent output, and
        // the offsets reported below say exactly which window it got.
        let requested = max_bytes.map_or(bytes.len(), |max| max as usize);
        let truncated = bytes.len() > requested;
        if truncated {
            bytes.drain(..bytes.len() - requested);
        }
        let from_offset = pane.transcript.offset.saturating_sub(bytes.len() as u64);
        let mut result = serde_json::json!({
            "pane_id": pane_id,
            "from_offset": from_offset,
            "output_offset": pane.transcript.offset,
            "retained_from_offset": pane.transcript.start(),
            "bytes": bytes.len(),
            "truncated": truncated,
            // Present only when output the caller asked for is genuinely gone, so its absence is
            // a promise rather than an omission.
            "dropped_before_offset": dropped_before,
        });
        if base64 {
            use base64::Engine as _;
            result["base64"] = base64::engine::general_purpose::STANDARD
                .encode(&bytes)
                .into();
        } else {
            result["text"] = String::from_utf8_lossy(&bytes).into_owned().into();
        }
        Ok(result)
    }

    /// Give a pane an exact size along either axis.
    ///
    /// `action resize <direction>` nudges one step, which is right for a keybinding and useless
    /// for a caller that needs a deterministic grid — a TUI test wants 80 columns, not "a bit
    /// wider than before". The committed geometry is reported because the tree may not be able to
    /// grant the request exactly: minimum pane sizes and integer cell division both round it.
    pub(super) fn automation_resize_pane(
        &mut self,
        pane_id: PaneId,
        columns: Option<u16>,
        rows: Option<u16>,
    ) -> Result<serde_json::Value, AutomationError> {
        if columns.is_none() && rows.is_none() {
            return Err(AutomationError::new(
                "invalid_params",
                "resize-pane needs --columns, --rows, or both",
            ));
        }
        let area = self.content_area();
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(pane_id))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        if self.tabs[tab_index].zoomed.is_some() {
            return Err(AutomationError::new(
                "invalid_state",
                "unzoom the tab before resizing a pane",
            ));
        }
        let mut refused = Vec::new();
        if self.tabs[tab_index].floating.contains(pane_id) {
            let rect = self.tabs[tab_index]
                .floating
                .get(pane_id)
                .map(|float| float.rect)
                .ok_or_else(|| AutomationError::new("pane_not_found", "float no longer exists"))?;
            // A float's frame is part of its rectangle, so a caller asking for a content size is
            // asking for two more columns and two more rows of window than that.
            let next = Rect {
                width: columns.map_or(rect.width, |columns| columns.saturating_add(2)),
                height: rows.map_or(rect.height, |rows| rows.saturating_add(2)),
                ..rect
            };
            self.tabs[tab_index].floating.set_rect(pane_id, next, area);
            // An exactly sized float is a caller's decision: it stops re-proportioning.
            self.tabs[tab_index].floating.clear_origin(pane_id);
        } else {
            for (requested, axis, name) in [
                (columns, Axis::Horizontal, "columns"),
                (rows, Axis::Vertical, "rows"),
            ] {
                let Some(requested) = requested else { continue };
                // The frame again: the tree divides outer rectangles, the caller means content.
                let span = requested.saturating_add(2);
                let applied = self.tabs[tab_index]
                    .tree
                    .as_mut()
                    .is_some_and(|tree| tree.set_pane_span(pane_id, axis, span, area));
                if !applied {
                    refused.push(name);
                }
            }
            if refused.len() == 2 || (refused.len() == 1 && columns.is_none() != rows.is_none()) {
                return Err(AutomationError::new(
                    "invalid_state",
                    format!(
                        "pane {pane_id} cannot be resized in {}: it has no split on that axis, or \
                         the result would be below the minimum pane size",
                        refused.join(" or ")
                    ),
                ));
            }
        }
        self.relayout();
        let geometry = self
            .tabs
            .get(tab_index)
            .map(|tab| Self::topology_projections(tab, self.content_area()))
            .unwrap_or_default()
            .into_iter()
            .find(|projection| projection.pane_id == pane_id);
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "geometry": geometry.map(|projection| rect_json(projection.outer)),
            "content_geometry": geometry.map(|projection| rect_json(projection.content)),
            "columns": self.panes.get(&pane_id).map(|pane| pane.terminal.cols()),
            "rows": self.panes.get(&pane_id).map(|pane| pane.terminal.rows()),
            "layout_sequence": self.layout_revision,
        }))
    }

    /// Move a pane to another tab, swap it with a neighbour, or change its layer.
    ///
    /// The pane keeps its ID, its name, its agent, and its Vivid media ownership: only where it
    /// sits changes. That matters more here than anywhere else in the layout surface — media
    /// identity is the complete owner/context/surface/track tuple, and a move that renumbered or
    /// re-registered anything would look to the bridge like one producer's sources vanishing.
    pub(super) fn automation_move_pane(
        &mut self,
        pane_id: PaneId,
        to_tab: Option<&TabSelector>,
        swap: Option<Direction>,
        to_layer: Option<crate::ipc::PaneLayerRequest>,
    ) -> Result<serde_json::Value, AutomationError> {
        let requests = [to_tab.is_some(), swap.is_some(), to_layer.is_some()];
        if requests.iter().filter(|given| **given).count() != 1 {
            return Err(AutomationError::new(
                "invalid_params",
                "move-pane takes exactly one of --to-tab, --swap, or --to-layer",
            ));
        }
        let area = self.content_area();
        let from_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(pane_id))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        if self.tabs[from_index].zoomed == Some(pane_id) {
            self.tabs[from_index].zoomed = None;
        }

        if let Some(direction) = swap {
            let projections = Self::topology_projections(&self.tabs[from_index], area);
            let other = directional_focus(&projections, pane_id, direction).ok_or_else(|| {
                AutomationError::new(
                    "pane_not_found",
                    format!("no pane lies {} of pane {pane_id}", direction.as_str()),
                )
            })?;
            let tree = self.tabs[from_index].tree.as_mut().ok_or_else(|| {
                AutomationError::new("unsupported", "tab has no tiled layout to swap within")
            })?;
            if !tree.swap(pane_id, other) {
                return Err(AutomationError::new(
                    "unsupported",
                    "both panes must be tiled to swap places",
                ));
            }
            self.relayout();
            return Ok(self.moved_pane_json(pane_id, "swap"));
        }

        if let Some(layer) = to_layer {
            let floating = self.tabs[from_index].floating.contains(pane_id);
            match (layer, floating) {
                (crate::ipc::PaneLayerRequest::Floating, false) => {
                    let remaining = self.tabs[from_index]
                        .tree
                        .take()
                        .and_then(|tree| tree.close(pane_id));
                    if remaining.is_none() {
                        // Nothing would be left behind it, and a tab whose only pane floats over
                        // an empty tiled area is a shape the layout engine does not describe.
                        self.tabs[from_index].tree = Some(TiledNode::leaf(pane_id));
                        return Err(AutomationError::new(
                            "invalid_state",
                            "the tab's only tiled pane cannot be made floating",
                        ));
                    }
                    self.tabs[from_index].tree = remaining;
                    let origin = self.default_float_origin();
                    self.tabs[from_index].floating.insert(pane_id, area, origin);
                }
                (crate::ipc::PaneLayerRequest::Tiled, true) => {
                    self.tabs[from_index].floating.remove(pane_id);
                    let tree = match self.tabs[from_index].tree.take() {
                        Some(mut tree) => {
                            let anchor = self.tabs[from_index]
                                .last_focused_tiled
                                .filter(|anchor| tree.contains(*anchor))
                                .or_else(|| tree.pane_ids().into_iter().next());
                            match anchor {
                                Some(anchor) => {
                                    tree.split(anchor, pane_id, Axis::Horizontal, area)
                                        .map_err(|_out_of_range| {
                                            AutomationError::new(
                                                "invalid_state",
                                                "no room to tile this pane",
                                            )
                                        })?;
                                    tree
                                }
                                None => TiledNode::leaf(pane_id),
                            }
                        }
                        None => TiledNode::leaf(pane_id),
                    };
                    self.tabs[from_index].tree = Some(tree);
                }
                // Already where it was asked to be. Idempotent on purpose: a retried move is not
                // an error, and reporting one would make every retry loop handle a false failure.
                _ => {}
            }
            self.relayout();
            return Ok(self.moved_pane_json(pane_id, "layer"));
        }

        let to_index = self.resolve_tab(to_tab.expect("checked above"))?;
        if to_index == from_index {
            return Ok(self.moved_pane_json(pane_id, "tab"));
        }
        let floating = self.tabs[from_index].floating.contains(pane_id);
        if floating {
            self.tabs[from_index].floating.remove(pane_id);
        } else {
            let remaining = self.tabs[from_index]
                .tree
                .take()
                .and_then(|tree| tree.close(pane_id));
            self.tabs[from_index].tree = remaining;
        }
        if self.tabs[from_index].is_empty() {
            // The source tab is gone. Removing it shifts every later index, including the
            // destination, so both are re-resolved from the tab ID rather than reused.
            let to_id = self.tabs[to_index].id;
            self.tabs.remove(from_index);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len().saturating_sub(1);
            }
            let to_index = self
                .tabs
                .iter()
                .position(|tab| tab.id == to_id)
                .expect("the destination tab is not the one that was removed");
            self.attach_pane_to_tab(to_index, pane_id, floating, area)?;
        } else {
            if let Some(tab) = self.tabs.get_mut(from_index)
                && !tab.contains(tab.focused)
                && let Some(next) = tab.fallback_focus()
            {
                tab.set_focus(next);
            }
            self.attach_pane_to_tab(to_index, pane_id, floating, area)?;
        }
        self.relayout();
        Ok(self.moved_pane_json(pane_id, "tab"))
    }

    /// Put a pane into a tab it did not previously belong to, on the requested layer.
    pub(super) fn attach_pane_to_tab(
        &mut self,
        tab_index: usize,
        pane_id: PaneId,
        floating: bool,
        area: Rect,
    ) -> Result<(), AutomationError> {
        if floating {
            let origin = self.default_float_origin();
            self.tabs[tab_index].floating.insert(pane_id, area, origin);
            return Ok(());
        }
        let tree = match self.tabs[tab_index].tree.take() {
            Some(mut tree) => {
                let anchor = self.tabs[tab_index]
                    .last_focused_tiled
                    .filter(|anchor| tree.contains(*anchor))
                    .or_else(|| tree.pane_ids().into_iter().next());
                match anchor {
                    Some(anchor) => {
                        tree.split(anchor, pane_id, Axis::Horizontal, area)
                            .map_err(|_out_of_range| {
                                AutomationError::new(
                                    "invalid_state",
                                    "no room in the destination tab",
                                )
                            })?;
                        tree
                    }
                    None => TiledNode::leaf(pane_id),
                }
            }
            None => TiledNode::leaf(pane_id),
        };
        self.tabs[tab_index].tree = Some(tree);
        Ok(())
    }

    pub(super) fn moved_pane_json(&mut self, pane_id: PaneId, kind: &str) -> serde_json::Value {
        self.mark_snapshot_dirty();
        let tab = self.tabs.iter().find(|tab| tab.contains(pane_id));
        serde_json::json!({
            "pane_id": pane_id,
            "moved": kind,
            "tab_id": tab.map(|tab| tab.id),
            "split_path": tab
                .and_then(|tab| tab.tree.as_ref())
                .and_then(|tree| tree.split_path(pane_id)),
            "layer": match tab {
                Some(tab) if tab.floating.get(pane_id).is_some_and(|float| float.pinned) => "pinned",
                Some(tab) if tab.floating.contains(pane_id) => "floating",
                _ => "tiled",
            },
            "layout_sequence": self.layout_revision,
        })
    }

    /// Set one pane or tab flag outright.
    pub(super) fn automation_set_flag(
        &mut self,
        pane_id: PaneId,
        flag: crate::ipc::PaneFlag,
        enabled: bool,
        offset: Option<usize>,
    ) -> Result<serde_json::Value, AutomationError> {
        use crate::ipc::PaneFlag;
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(pane_id))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        let previous = self.pane_flag(tab_index, pane_id, flag);
        match flag {
            PaneFlag::Zoom => {
                // Tab-scoped: zooming this pane unzooms whatever was zoomed, and turning zoom off
                // only clears it when it is this pane that is zoomed.
                let zoomed = self.tabs[tab_index].zoomed;
                if enabled {
                    if self.tabs[tab_index].floating.contains(pane_id) {
                        return Err(AutomationError::new(
                            "unsupported",
                            "floating panes cannot be zoomed",
                        ));
                    }
                    self.tabs[tab_index].zoomed = Some(pane_id);
                } else if zoomed == Some(pane_id) {
                    self.tabs[tab_index].zoomed = None;
                }
                self.force_full = true;
                self.relayout();
            }
            PaneFlag::Pinned => {
                if !self.tabs[tab_index].floating.contains(pane_id) {
                    return Err(AutomationError::new(
                        "unsupported",
                        "only floating panes can be pinned",
                    ));
                }
                self.tabs[tab_index].floating.set_pinned(pane_id, enabled);
                self.force_full = true;
                self.projection_changed();
            }
            PaneFlag::Transparent => {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.transparent = enabled;
                }
                self.mark_snapshot_dirty();
                self.schedule_render();
            }
            PaneFlag::CopyMode => {
                self.set_copy_mode(pane_id, enabled, offset)?;
            }
            PaneFlag::FloatsVisible => {
                if self.tabs[tab_index].zoomed.is_some() {
                    return Err(AutomationError::new(
                        "invalid_state",
                        "unzoom before changing floating pane visibility",
                    ));
                }
                self.tabs[tab_index].floating.ordinary_visible = enabled;
                if let Some(tab) = self.tabs.get_mut(tab_index) {
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
            PaneFlag::SyncInput => {
                self.tabs[tab_index].sync_input = enabled;
                self.mark_snapshot_dirty();
                self.schedule_render();
            }
        }
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "tab_id": self.tabs[tab_index].id,
            "flag": flag,
            "enabled": self.pane_flag(tab_index, pane_id, flag),
            // A setter is idempotent, so "did anything change" is the useful answer rather than
            // "did it succeed" — which it always did.
            "changed": previous != self.pane_flag(tab_index, pane_id, flag),
            "layout_sequence": self.layout_revision,
        }))
    }

    pub(super) fn pane_flag(
        &self,
        tab_index: usize,
        pane_id: PaneId,
        flag: crate::ipc::PaneFlag,
    ) -> bool {
        use crate::ipc::PaneFlag;
        let Some(tab) = self.tabs.get(tab_index) else {
            return false;
        };
        match flag {
            PaneFlag::Zoom => tab.zoomed == Some(pane_id),
            PaneFlag::Pinned => tab.floating.get(pane_id).is_some_and(|float| float.pinned),
            PaneFlag::Transparent => self
                .panes
                .get(&pane_id)
                .is_some_and(|pane| pane.transparent),
            PaneFlag::CopyMode => self
                .panes
                .get(&pane_id)
                .is_some_and(|pane| pane.copy.is_some()),
            PaneFlag::FloatsVisible => tab.floating.ordinary_visible,
            PaneFlag::SyncInput => tab.sync_input,
        }
    }

    /// Enter or leave copy mode on one pane, optionally at a scrollback offset.
    ///
    /// The same state the interactive `enter-copy-mode` builds, so a pane entered through
    /// automation behaves identically to one a keybinding opened — including the search and
    /// selection a caller may drive afterwards.
    pub(super) fn set_copy_mode(
        &mut self,
        pane_id: PaneId,
        enabled: bool,
        offset: Option<usize>,
    ) -> Result<(), AutomationError> {
        let rows = self.content_area().height.saturating_sub(2) as usize;
        let pane = self
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        if !enabled {
            if offset.is_some() {
                return Err(AutomationError::new(
                    "invalid_params",
                    "--offset only applies when entering copy mode",
                ));
            }
            pane.copy = None;
            self.mark_pane_screen_change(pane_id, None);
            self.schedule_render();
            return Ok(());
        }
        // Clamped rather than refused: scrollback shrinks as a pane keeps writing, so an offset
        // that was in range when a caller read it can be past the end by the time it is used.
        let offset = offset.unwrap_or(0).min(pane.terminal.history_len());
        pane.copy = Some(CopyState {
            offset,
            row: rows.saturating_sub(1),
            column: 0,
            selection_start: None,
            search: None,
            matches: Vec::new(),
            current: None,
        });
        self.mark_pane_screen_change(pane_id, None);
        self.schedule_render();
        Ok(())
    }

    /// Refuse a request whose view of the session has already moved on.
    ///
    /// Checked before the handler rather than inside it, so a stale action never reaches a PTY or
    /// a layout mutation. Only the fields a caller supplied are compared: pinning a screen
    /// sequence should not also require the layout to have stood still.
    pub(super) fn check_expected_state(
        &self,
        pane_id: Option<PaneId>,
        expect: crate::ipc::ExpectedState,
    ) -> Result<(), AutomationError> {
        let stale = |what: &str, expected: u64, actual: u64| {
            AutomationError::new(
                "invalid_state",
                format!("{what} is {actual}, not the expected {expected}"),
            )
        };
        if let Some(expected) = expect.screen_sequence {
            let pane_id = pane_id.ok_or_else(|| {
                AutomationError::new(
                    "invalid_params",
                    "an expected screen sequence needs a pane to compare it against",
                )
            })?;
            let actual = self
                .panes
                .get(&pane_id)
                .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?
                .screen_sequence;
            if actual != expected {
                return Err(stale("screen sequence", expected, actual));
            }
        }
        if let Some(expected) = expect.session_sequence
            && self.session_sequence != expected
        {
            return Err(stale("session sequence", expected, self.session_sequence));
        }
        if let Some(expected) = expect.layout_sequence
            && self.layout_revision != expected
        {
            return Err(stale("layout sequence", expected, self.layout_revision));
        }
        Ok(())
    }

    /// Record an idempotency key, or return the reply the first request with it produced.
    ///
    /// Only mutating methods are deduplicated. An observation is safe to repeat and its answer
    /// goes stale immediately, so replaying a cached `get-text` would be a lie about the present
    /// rather than a protection against a double action.
    pub(super) fn claim_idempotency_key(
        &mut self,
        method: &AutomationMethod,
        key: String,
    ) -> Result<Option<serde_json::Value>, AutomationError> {
        if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
            return Err(AutomationError::new(
                "invalid_params",
                format!("an idempotency key holds 1..={MAX_IDEMPOTENCY_KEY_BYTES} bytes"),
            ));
        }
        if !crate::ipc::METHOD_CAPABILITIES
            .iter()
            .find(|capability| capability.name == method.name())
            .is_some_and(|capability| capability.mutating)
        {
            return Err(AutomationError::new(
                "invalid_params",
                format!(
                    "{} does not change anything, so an idempotency key would only hide fresh state",
                    method.name()
                ),
            ));
        }
        if let Some(previous) = self.idempotency_keys.get(&key) {
            return Ok(Some(previous.clone()));
        }
        // Reserved with a null result before the handler runs. A second request arriving while the
        // first is still in flight is a retry too, and it must not be applied a second time
        // because the first has not answered yet.
        while self.idempotency_keys.len() >= MAX_IDEMPOTENCY_KEYS {
            let Some(oldest) = self.idempotency_order.pop_front() else {
                break;
            };
            self.idempotency_keys.remove(&oldest);
        }
        self.idempotency_order.push_back(key.clone());
        self.idempotency_keys.insert(key, serde_json::Value::Null);
        Ok(None)
    }

    /// Deliver one signal to a pane's foreground process group.
    ///
    /// The pane is not closed and its process is not reaped here: a signalled job may exit, stop,
    /// or ignore it entirely, and the ordinary exit path already owns whichever of those happens.
    /// The reply reports the group that was signalled so a caller can tell "delivered to the job"
    /// from "delivered to the shell that was waiting at its prompt".
    pub(super) fn automation_signal(
        &mut self,
        pane_id: PaneId,
        signal: crate::ipc::SignalName,
    ) -> Result<serde_json::Value, AutomationError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        if pane.exit_status.is_some() {
            return Err(AutomationError::new(
                "pty_closed",
                "pane process has already exited",
            ));
        }
        let child_pid = pane.child_pid;
        let group = pane.control.signal(signal.number()).map_err(|error| {
            let code = match error.kind() {
                io::ErrorKind::Unsupported => "unsupported",
                io::ErrorKind::BrokenPipe | io::ErrorKind::NotFound => "pty_closed",
                _ => "invalid_state",
            };
            AutomationError::new(code, error.to_string())
        })?;
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "signal": signal.as_str(),
            "process_group": group,
            // A group that is not the pane's own child means a job had claimed the terminal, which
            // is the difference between interrupting `cargo test` and interrupting its shell.
            "foreground_job": group != child_pid,
        }))
    }

    /// Give a pane a name, or clear it.
    ///
    /// Uniqueness is per session and checked here: a name is a target, and two panes answering to
    /// one would make every command that uses it ambiguous. Renaming a pane to the name it already
    /// has succeeds rather than colliding with itself.
    pub(super) fn automation_pane_rename(
        &mut self,
        pane_id: PaneId,
        name: Option<crate::layout::PaneName>,
    ) -> Result<serde_json::Value, AutomationError> {
        if let Some(name) = &name
            && self
                .pane_with_name(name)
                .is_some_and(|held| held != pane_id)
        {
            return Err(AutomationError::new(
                "pane_name_taken",
                format!("another pane is already named {name}"),
            ));
        }
        let pane = self
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        let previous = std::mem::replace(&mut pane.name, name.clone());
        // A name lives in the snapshot, so the session that comes back after a restart is only
        // correct if the rename is written down.
        self.mark_snapshot_dirty();
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "pane_name": name.as_ref().map(ToString::to_string),
            "previous_pane_name": previous.as_ref().map(ToString::to_string),
        }))
    }

    /// Make a pane visible without moving focus.
    ///
    /// Selects the owning tab and lifts a zoom that is hiding the target. It deliberately does not
    /// focus the pane or disturb the attachment: visibility is what drives media projection, so
    /// "let me see this" and "type here now" are different requests.
    pub(super) fn automation_activate_pane(
        &mut self,
        pane_id: PaneId,
    ) -> Result<serde_json::Value, AutomationError> {
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.contains(pane_id))
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane has no owning tab"))?;
        let tab_selected = self.active_tab != tab_index;
        // Zoom projects one leaf over the whole tab, so any other pane in that tab is hidden by it.
        let unzoomed = self.tabs[tab_index]
            .zoomed
            .is_some_and(|zoomed| zoomed != pane_id);
        // An ordinary float that the layer has hidden cannot be revealed pane by pane: the toggle
        // is per tab, so the whole ordinary block comes back.
        let revealed_floats = self.tabs[tab_index].floating.contains(pane_id)
            && !self.tabs[tab_index]
                .floating
                .visible()
                .any(|float| float.pane_id == pane_id);
        if tab_selected {
            self.active_tab = tab_index;
            self.force_full = true;
        }
        if unzoomed {
            self.tabs[tab_index].zoomed = None;
        }
        if revealed_floats {
            self.tabs[tab_index].floating.ordinary_visible = true;
        }
        if tab_selected || unzoomed || revealed_floats {
            self.relayout();
        }
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "tab_id": self.tabs[tab_index].id,
            "tab_selected": tab_selected,
            "unzoomed": unzoomed,
            "revealed_floats": revealed_floats,
            "visible": self.pane_is_visibly_present(pane_id),
            "focused": self.tabs[tab_index].focused == pane_id,
            // Stated rather than implied: the whole point of this method is that it does not.
            "focus_changed": false,
            "layout_sequence": self.layout_revision,
        }))
    }

    /// Open a tab and report the IDs it was given.
    ///
    /// `action new-tab` does the same thing but answers nothing, so a caller that needs to act on
    /// the new tab has to guess or re-list. This returns the identities it just created.
    pub(super) fn automation_new_tab(
        &mut self,
        name: Option<String>,
    ) -> Result<serde_json::Value, AutomationError> {
        let name = name.map(validated_tab_name).transpose()?;
        if self.tabs.len() >= MAX_LAYOUT_TABS {
            return Err(AutomationError::new(
                "limit_exceeded",
                format!("a session holds at most {MAX_LAYOUT_TABS} tabs"),
            ));
        }
        self.check_session_pane_cap()?;
        self.new_tab()
            .map_err(|error| AutomationError::new("pty_spawn_failed", error.to_string()))?;
        let tab = self
            .tabs
            .last_mut()
            .expect("new_tab pushed the tab it created");
        tab.name.clone_from(&name);
        let result = serde_json::json!({
            "tab_id": tab.id,
            "tab_name": name,
            "pane_id": tab.focused,
            "display_index": self.tabs.len() - 1,
        });
        self.mark_snapshot_dirty();
        self.relayout();
        Ok(result)
    }

    /// Set or clear a tab's name.
    pub(super) fn automation_rename_tab(
        &mut self,
        selector: &TabSelector,
        name: Option<String>,
    ) -> Result<serde_json::Value, AutomationError> {
        let name = name.map(validated_tab_name).transpose()?;
        let index = self.resolve_tab(selector)?;
        let previous = std::mem::replace(&mut self.tabs[index].name, name.clone());
        self.mark_snapshot_dirty();
        self.schedule_render();
        Ok(serde_json::json!({
            "tab_id": self.tabs[index].id,
            "tab_name": name,
            "previous_tab_name": previous,
        }))
    }

    /// Close a tab and everything in it.
    pub(super) fn automation_close_tab(
        &mut self,
        selector: &TabSelector,
    ) -> Result<serde_json::Value, AutomationError> {
        let index = self.resolve_tab(selector)?;
        let tab_id = self.tabs[index].id;
        let mut pane_ids = self.tabs[index]
            .tree
            .as_ref()
            .map_or_else(Vec::new, TiledNode::pane_ids);
        pane_ids.extend(self.tabs[index].floating.pane_ids());
        pane_ids.sort_unstable();
        pane_ids.dedup();
        // Closing every pane is what removes the tab: `close_pane` already owns tab collapse,
        // focus fallback, media teardown, and the `pane.closed` event, and reimplementing any of
        // that here would be a second path that has to stay in agreement with it.
        for pane_id in &pane_ids {
            self.close_pane(*pane_id);
        }
        Ok(serde_json::json!({
            "tab_id": tab_id,
            "closed_pane_ids": pane_ids,
            "accepted": true,
            "layout_sequence": self.layout_revision,
        }))
    }

    pub(super) fn automation_tabs(&self) -> serde_json::Value {
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let mut pane_ids = tab.tree.as_ref().map_or_else(Vec::new, TiledNode::pane_ids);
                pane_ids.extend(tab.floating.pane_ids());
                pane_ids.sort_unstable();
                pane_ids.dedup();
                serde_json::json!({
                    "tab_id": tab.id,
                    "display_index": index,
                    "name": tab.name,
                    "active": index == self.active_tab,
                    "focused_pane_id": tab.focused,
                    "pane_ids": pane_ids,
                    "sync_input": tab.sync_input,
                    "zoomed_pane_id": tab.zoomed,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "session": self.name,
            "session_instance": self.session_instance,
            "active_tab_id": self.active_tab().map(|tab| tab.id),
            "tabs": tabs,
        })
    }

    /// Every attached client in connection order. Carries nothing that could reach a client's
    /// host: no outer endpoint, no token.
    pub(super) fn clients_json(&self) -> Vec<serde_json::Value> {
        self.clients
            .values()
            .map(|client| {
                serde_json::json!({
                    "client_id": client.id,
                    "presenter": self.presenter == Some(client.id),
                    "read_only": client.read_only,
                    "vivid_capable": client.vivid,
                    "media_enabled": client.media_enabled,
                    "retained_inflight": client.shared_visuals.inflight.len(),
                    "retained_pending_revision": client.shared_visuals.pending_revision,
                    "kitty_graphics": client.kitty_graphics,
                    "focused": client.focused,
                    "remote": client.outer.as_ref().is_some_and(|outer| outer.remote),
                    "columns": client.display.columns,
                    "rows": client.display.rows,
                    "acknowledged_frame": client.acknowledged_frame,
                    "rendered_session_sequence": client.rendered_session_sequence,
                    "pending_frame_acknowledgements": client.frame_sequences.len(),
                })
            })
            .collect()
    }

    pub(super) fn automation_list_clients(&self) -> serde_json::Value {
        let layout = self.layout_display();
        serde_json::json!({
            "session": self.name,
            "clients": self.clients_json(),
            "presenter_client_id": self.presenter,
            "window_size": self.config.general.window_size,
            "view": match self.direct_pane() {
                Some(pane_id) => serde_json::json!({"kind": "pane", "pane_id": pane_id}),
                None => serde_json::json!({"kind": "session"}),
            },
            "layout": {"columns": layout.columns, "rows": layout.rows},
        })
    }

    pub(super) fn automation_session_inspect(&self) -> serde_json::Value {
        let queue = self.relay_metrics();
        serde_json::json!({
            "schema_version": 1,
            "session": self.name,
            "session_instance": self.session_instance,
            "clients": self.clients_json(),
            "presenter_client_id": self.presenter,
            // Null with nothing attached, which is the honest answer: with no client there is no
            // window presenting this session and nothing for a pane agent to address.
            "outer": self.outer_identity_json(),
            "active_tab_id": self.active_tab().map(|tab| tab.id),
            "active_pane_id": self.active_tab().map(|tab| tab.focused),
            "session_sequence": self.session_sequence,
            "layout_revision": self.layout_revision,
            "virtual_projection_revision": self.vivid.revision(),
            "submitted_projection_revision": self.media_projection_revision,
            "outer_projection_revision": self.outer_projection_revision,
            "outer_apply_sequence": self.outer_apply_sequence,
            "bridge_instance_id": self.bridge_instance_id,
            "bridge_local_revision": self.bridge_local_revision,
            "pending": {
                "actor_work": self.pending_actor_work.len(),
                "automation_waiters": self.automation_waiters.len(),
                "media_projections": self.pending_media_projections.len(),
                "render_scheduled": self.pending_render,
            },
            "queue_health": queue,
            "tabs": self.automation_tabs()["tabs"],
        })
    }

    pub(super) fn automation_diagnose(
        &self,
        requested_pane: Option<PaneId>,
        all_panes: bool,
        trace_limit: u16,
    ) -> Result<serde_json::Value, AutomationError> {
        let pane_ids = if all_panes {
            self.panes.keys().copied().collect::<Vec<_>>()
        } else {
            vec![
                requested_pane
                    .or_else(|| self.active_tab().map(|tab| tab.focused))
                    .ok_or_else(|| {
                        AutomationError::new("no_focused_pane", "session has no pane")
                    })?,
            ]
        };
        let mut panes = Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            // Diagnostics are collected into debug bundles and pasted into issues.
            let pane = self
                .pane_description(pane_id, AgentDisclosure::Presence)
                .ok_or_else(|| {
                    AutomationError::new("pane_not_found", format!("pane {pane_id} does not exist"))
                })?;
            let media = self.vivid.pane_status(
                pane_id,
                self.outer_media_projection(),
                self.relay_metrics(),
            );
            let trace = self.media_trace.query(
                None,
                trace_limit,
                Some(pane_id),
                MediaTraceFilter::default(),
            );
            panes.push(serde_json::json!({"pane": pane, "media": media, "trace": trace}));
        }
        Ok(serde_json::json!({
            "schema_version": 1,
            "capture": {"atomic_actor_turn": true, "asynchronous_metric_age_ms": 0},
            "session": self.automation_session_inspect(),
            "panes": panes,
        }))
    }
}

/// A tab name a caller supplied, held to what the interactive rename accepts.
///
/// Same ceiling and same control-character handling as the modal path, so a name typed at the
/// prompt and a name set through automation cannot end up being different kinds of string. An
/// empty or all-blank name clears the tab's name rather than setting a blank one.
fn validated_tab_name(name: String) -> Result<String, AutomationError> {
    if name.len() > MAX_TAB_NAME_BYTES {
        return Err(AutomationError::new(
            "limit_exceeded",
            format!("a tab name holds at most {MAX_TAB_NAME_BYTES} bytes"),
        ));
    }
    let name = single_line(&name).trim().to_owned();
    if name.is_empty() {
        return Err(AutomationError::new(
            "invalid_params",
            "a tab name cannot be blank; use reset-tab-title to clear one",
        ));
    }
    Ok(name)
}
