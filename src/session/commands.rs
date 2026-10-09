//! Key-bound actions, plugin actions, and plugin host calls.

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
    pub(super) fn action(&mut self, action: Action) {
        self.cancel_pointer_drag(true);
        if self.invalidate_mouse_selection_state() {
            self.schedule_render();
        }
        // Any prefix action during a float-edit mode invalidates it (focus, tab, zoom, and
        // layout changes are all cancellation triggers); restore the entry rectangle first.
        self.end_float_mode(true);
        match action {
            Action::ToggleAgentNavigator => {
                self.toggle_agent_navigator();
                return;
            }
            Action::ToggleTabNavigator => {
                self.toggle_tab_navigator();
                return;
            }
            Action::BeginRenameTab => {
                self.begin_tab_rename();
                return;
            }
            Action::BeginClosePaneConfirmation => {
                self.begin_close_pane_confirmation();
                return;
            }
            Action::BeginSaveLayout => {
                self.begin_save_layout();
                return;
            }
            Action::ResolveClosePaneConfirmation(confirmed) => {
                self.resolve_close_pane_confirmation(confirmed);
                return;
            }
            _ => {}
        }
        if self.transient_ui_active() {
            return;
        }
        match action {
            Action::Split(axis) => self.split(axis),
            Action::Focus(direction) => self.focus(direction),
            Action::Resize(direction) => self.resize(direction),
            Action::NewTab if self.new_tab().is_ok() => {
                self.active_tab = self.tabs.len() - 1;
                self.relayout();
            }
            Action::NextTab if !self.tabs.is_empty() => {
                self.active_tab = (self.active_tab + 1) % self.tabs.len();
                self.force_full = true;
                self.relayout();
            }
            Action::PreviousTab if !self.tabs.is_empty() => {
                self.active_tab = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
                self.force_full = true;
                self.relayout();
            }
            Action::SelectTab(index) if index < self.tabs.len() => {
                self.active_tab = index;
                self.force_full = true;
                self.relayout();
            }
            Action::ClosePane => {
                if let Some(pane) = self.active_tab().map(|tab| tab.focused) {
                    self.close_pane(pane);
                }
            }
            Action::ToggleZoom => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.zoomed = if tab.zoomed.is_some() {
                        None
                    } else {
                        Some(tab.focused)
                    };
                    self.force_full = true;
                    self.relayout();
                }
            }
            Action::ToggleSyncInput => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.sync_input = !tab.sync_input;
                    self.session_sequence = self.session_sequence.wrapping_add(1);
                    self.force_full = true;
                    self.schedule_render();
                }
            }
            Action::EnterCopyMode => {
                let rows = self.content_area().height.saturating_sub(2) as usize;
                if let Some(pane_id) = self.active_tab().map(|tab| tab.focused)
                    && let Some(pane) = self.panes.get_mut(&pane_id)
                {
                    pane.copy = Some(CopyState {
                        offset: 0,
                        row: rows.saturating_sub(1),
                        column: 0,
                        selection_start: None,
                        search: None,
                        matches: Vec::new(),
                        current: None,
                    });
                    self.mark_pane_screen_change(pane_id, None);
                    self.schedule_render();
                }
            }
            Action::CopyInput(bytes) => {
                if let Some(pane) = self.active_tab().map(|tab| tab.focused) {
                    self.copy_input(pane, &bytes);
                }
            }
            Action::Paste => self.paste(),
            Action::NewFloatingPane => self.new_float(),
            Action::ToggleFloatingPanes => self.toggle_floats(),
            Action::TogglePanePinned => self.toggle_pin(),
            Action::TogglePaneTransparency => self.toggle_transparency(),
            Action::EnterFloatingMoveMode => self.enter_float_mode(FloatingEditKind::Move),
            Action::EnterFloatingResizeMode => self.enter_float_mode(FloatingEditKind::Resize),
            Action::Plugin(reference) => self.start_plugin_action(reference),
            Action::CycleTabView => self.set_tab_view(self.tab_view.next()),
            _ => {}
        }
    }

    pub(super) fn start_plugin_action(&mut self, reference: String) {
        let Some(invocation) = reference.strip_prefix("plugin:").map(ToOwned::to_owned) else {
            self.status("invalid plugin action reference");
            return;
        };
        if !valid_invocation_reference(&invocation) {
            self.status("invalid plugin action reference");
            return;
        }
        self.invoke_plugin_action(invocation, serde_json::json!({}));
    }

    pub(super) fn invoke_plugin_action(&mut self, invocation: String, input: serde_json::Value) {
        let Some(supervisor) = self.plugin_supervisor.clone() else {
            self.status("plugin action rejected: plugins are disabled in this session");
            return;
        };
        let notice = format!("plugin:{invocation}");
        if let Err(error) = supervisor.invoke_notice(invocation, input, notice) {
            self.status(&format!("plugin action rejected: {}", error.message));
        }
    }

    pub(super) fn execute_session_command(
        &mut self,
        caller: &CallerContext,
        command: SessionCommand,
    ) -> Result<serde_json::Value, AutomationError> {
        authorize_session_scope(caller, &self.session_instance)?;
        match command {
            SessionCommand::InspectSession => {
                authorize_session_capability(caller, vvmux_plugin_api::Permission::SessionRead)?;
                let panes = self
                    .panes
                    .keys()
                    .copied()
                    // Serves both `msg list-panes` and the plugin host's `session.inspect`, so a
                    // bulk listing never carries a native agent session reference.
                    .filter_map(|pane| self.pane_description(pane, AgentDisclosure::Presence))
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({
                    "session": self.name,
                    "session_instance": self.session_instance,
                    "session_sequence": self.session_sequence,
                    "actor_wakeups": self.actor_wakeups,
                    "layout_sequence": self.layout_revision,
                    "rendered_session_sequence": self.rendered_session_sequence(),
                    "panes": panes,
                }))
            }
            SessionCommand::ReadPaneText {
                pane_id,
                rows,
                source,
                max_bytes,
            } => {
                authorize_session_capability(caller, vvmux_plugin_api::Permission::PaneRead)?;
                let pane_id = self.resolve_session_command_pane(caller, pane_id)?;
                let pane = self.panes.get(&pane_id).ok_or_else(|| {
                    AutomationError::new("pane_not_found", format!("pane {pane_id} does not exist"))
                })?;
                // A row count defaults to one screenful, so `recent` reads stay bounded without
                // the caller having to know the pane's geometry.
                let rows = rows.unwrap_or_else(|| pane.terminal.rows());
                let text = match source {
                    TextSource::Visible => pane
                        .terminal
                        .visible_text(pane.copy.as_ref().map_or(0, |copy| copy.offset)),
                    TextSource::Recent => pane.terminal.latest_text_physical(rows),
                    TextSource::RecentUnwrapped => pane.terminal.latest_text(rows),
                    TextSource::Detection => crate::agent::detection_snapshot(&pane.terminal),
                };
                if text.len() > max_bytes {
                    return Err(AutomationError::new(
                        "limit_exceeded",
                        "pane text exceeds the bounded command result",
                    ));
                }
                Ok(match source {
                    // Detection is only meaningful beside the OSC fields the classifier also
                    // reads, so it answers with a record rather than bare text.
                    TextSource::Detection => serde_json::json!({
                        "text": text,
                        "osc_title": pane.terminal.agent_osc_title(),
                        "osc_progress": pane.terminal.agent_osc_progress(),
                        "rows": pane.terminal.rows(),
                    }),
                    _ => serde_json::Value::String(text),
                })
            }
            SessionCommand::WritePaneInput { pane_id, bytes } => {
                authorize_session_capability(caller, vvmux_plugin_api::Permission::PaneInput)?;
                let pane_id = self.resolve_session_command_pane(caller, pane_id)?;
                if bytes.len() > 1024 * 1024 {
                    return Err(AutomationError::new(
                        "limit_exceeded",
                        "PTY input exceeds 1 MiB",
                    ));
                }
                let pane = self.panes.get_mut(&pane_id).ok_or_else(|| {
                    AutomationError::new("pane_not_found", format!("pane {pane_id} does not exist"))
                })?;
                pane.input.send(&bytes).map_err(|error| {
                    AutomationError::new("runtime_unavailable", error.to_string())
                })?;
                if let Some(cause) = self.active_plugin_cause.clone() {
                    self.pending_pane_plugin_causes.insert(pane_id, cause);
                }
                Ok(serde_json::Value::Null)
            }
            SessionCommand::OpenPluginPane { launch } => {
                if !self.config.plugins.enabled || self.plugin_supervisor.is_none() {
                    return Err(plugin_disabled_error());
                }
                authorize_session_capability(caller, vvmux_plugin_api::Permission::PaneCreate)?;
                let (plugin_id, plugin_instance) = match &caller.origin {
                    CallerOrigin::Plugin {
                        plugin_id,
                        plugin_instance,
                    } => (plugin_id, plugin_instance),
                    CallerOrigin::Automation { .. } => {
                        return Err(AutomationError::new(
                            "scope_denied",
                            "plugin pane launch requires a broker-owned plugin identity",
                        ));
                    }
                };
                if plugin_id != &launch.scope.plugin_id
                    || plugin_instance != &launch.scope.plugin_instance
                    || caller.session_instance != launch.scope.session_instance
                {
                    return Err(AutomationError::new(
                        "scope_denied",
                        "plugin pane launch identity does not match its caller",
                    ));
                }
                let anchor = self.active_tab().map(|tab| tab.focused).ok_or_else(|| {
                    AutomationError::new("pane_not_found", "session has no pane to anchor launch")
                })?;
                let identity = PluginPaneIdentity {
                    session_instance: caller.session_instance.clone(),
                    plugin_id: plugin_id.clone(),
                    plugin_instance: plugin_instance.clone(),
                    package_digest: launch.package_digest.clone(),
                    entrypoint_id: launch.pane.id.clone(),
                    title: launch.pane.title.clone(),
                    accept_sync_input: launch.pane.accept_sync_input,
                };
                let placement = match launch.pane.placement {
                    vvmux_plugin_api::Placement::Split => crate::ipc::RunPlacement::Split {
                        axis: crate::ipc::Axis::Horizontal,
                    },
                    vvmux_plugin_api::Placement::Float => crate::ipc::RunPlacement::Float,
                    vvmux_plugin_api::Placement::Tab => crate::ipc::RunPlacement::Tab,
                };
                let vivid_capability = caller
                    .capabilities
                    .contains(&vvmux_plugin_api::Permission::MediaProduce);
                let mut extra_env = vec![
                    ("VVMUX_PLUGIN_ID".into(), plugin_id.clone()),
                    ("VVMUX_PLUGIN_INSTANCE".into(), plugin_instance.clone()),
                    ("VVMUX_PLUGIN_PANE".into(), launch.pane.id.clone()),
                ];
                if vivid_capability && let Some(helper) = launch.vivi_helper.as_ref() {
                    extra_env.extend([
                        (
                            "VVMUX_VIVI_BIN".into(),
                            helper.to_string_lossy().into_owned(),
                        ),
                        ("VVMUX_VIVI_PROTOCOL_VERSION".into(), "1.5".into()),
                    ]);
                }
                let spec = PaneSpawn {
                    command: None,
                    argv: Some(launch.pane.command.iter().map(OsString::from).collect()),
                    cwd: Some(launch.package_root),
                    transparent: None,
                    hold_on_exit: launch.pane.hold_on_exit,
                    extra_env,
                    role: PaneRole::Plugin(identity),
                    vivid_capability,
                };
                let mut result = self.place_pane(
                    anchor,
                    spec,
                    placement,
                    true,
                    (launch.pane.placement == vvmux_plugin_api::Placement::Tab)
                        .then(|| launch.pane.title.clone()),
                )?;
                if let Some(object) = result.as_object_mut() {
                    object.insert(
                        "plugin_id".into(),
                        serde_json::Value::String(plugin_id.clone()),
                    );
                    object.insert(
                        "plugin_instance".into(),
                        serde_json::Value::String(plugin_instance.clone()),
                    );
                    object.insert(
                        "entrypoint_id".into(),
                        serde_json::Value::String(launch.pane.id),
                    );
                    object.insert("vivid".into(), serde_json::Value::Bool(vivid_capability));
                }
                Ok(result)
            }
            SessionCommand::ClosePane { pane_id } => {
                let pane = self.panes.get(&pane_id).ok_or_else(|| {
                    AutomationError::new("pane_not_found", format!("pane {pane_id} does not exist"))
                })?;
                let owns = caller_owns_plugin_pane(caller, &pane.role);
                if owns {
                    if !caller
                        .capabilities
                        .contains(&vvmux_plugin_api::Permission::PaneManageAny)
                    {
                        authorize_session_capability(
                            caller,
                            vvmux_plugin_api::Permission::PaneManageOwn,
                        )?;
                    }
                } else {
                    authorize_session_capability(
                        caller,
                        vvmux_plugin_api::Permission::PaneManageAny,
                    )?;
                }
                self.close_pane(pane_id);
                Ok(serde_json::Value::Null)
            }
        }
    }

    pub(super) fn resolve_session_command_pane(
        &self,
        caller: &CallerContext,
        pane_id: Option<PaneId>,
    ) -> Result<PaneId, AutomationError> {
        pane_id
            .or_else(|| {
                caller
                    .focused_fallback
                    .then(|| self.active_tab().map(|tab| tab.focused))
                    .flatten()
            })
            .ok_or_else(|| {
                AutomationError::new(
                    "pane_required",
                    "command requires an explicit pane or focused fallback",
                )
            })
    }

    pub(super) fn handle_plugin_host_call(
        &mut self,
        scope: &crate::plugin_supervisor::RuntimeScope,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, AutomationError> {
        if !self.config.plugins.enabled || self.plugin_supervisor.is_none() {
            return Err(plugin_disabled_error());
        }
        let caller = CallerContext {
            origin: CallerOrigin::Plugin {
                plugin_id: scope.plugin_id.clone(),
                plugin_instance: scope.plugin_instance.clone(),
            },
            session_instance: scope.session_instance.clone(),
            focused_fallback: false,
            capabilities: scope.permissions.iter().copied().collect(),
        };
        if method != "pane.close" {
            let required = plugin_host_permission(method).ok_or_else(|| {
                AutomationError::new(
                    "action_not_found",
                    format!("unknown plugin host call `{method}`"),
                )
            })?;
            authorize_session_capability(&caller, required)?;
        }
        match method {
            "session.inspect" => {
                require_plugin_params(&params, &[])?;
                self.execute_session_command(&caller, SessionCommand::InspectSession)
            }
            "pane.get_text" => {
                require_plugin_params(&params, &["pane_id", "rows"])?;
                let pane_id = plugin_u64_param(&params, "pane_id")?;
                let rows = plugin_optional_u64_param(&params, "rows")?
                    .map(|rows| usize::try_from(rows).unwrap_or(usize::MAX));
                if rows.is_some_and(|rows| !(1..=1000).contains(&rows)) {
                    return Err(AutomationError::new(
                        "invalid_params",
                        "rows must be from 1 through 1000",
                    ));
                }
                self.execute_session_command(
                    &caller,
                    SessionCommand::ReadPaneText {
                        pane_id: Some(pane_id),
                        rows,
                        // Pinned to the pre-existing meaning of this host call rather than to the
                        // CLI default, so no installed plugin changes behavior. New sources are
                        // an automation surface; widening the plugin contract is a separate
                        // decision with its own compatibility story.
                        source: rows.map_or(TextSource::Visible, |_| TextSource::RecentUnwrapped),
                        max_bytes: vvmux_plugin_api::MAX_FRAME_BYTES / 2,
                    },
                )
            }
            "pane.input" => {
                require_plugin_params(&params, &["pane_id", "text"])?;
                let pane_id = plugin_u64_param(&params, "pane_id")?;
                let text = params
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        AutomationError::new("invalid_params", "text must be a string")
                    })?;
                self.execute_session_command(
                    &caller,
                    SessionCommand::WritePaneInput {
                        pane_id: Some(pane_id),
                        bytes: text.as_bytes().to_vec(),
                    },
                )
            }
            "pane.close" => {
                require_plugin_params(&params, &["pane_id"])?;
                let pane_id = plugin_u64_param(&params, "pane_id")?;
                self.execute_session_command(&caller, SessionCommand::ClosePane { pane_id })
            }
            _ => Err(AutomationError::new(
                "action_not_found",
                format!("unknown plugin host call `{method}`"),
            )),
        }
    }
}

fn plugin_u64_param(params: &serde_json::Value, name: &str) -> Result<u64, AutomationError> {
    params
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            AutomationError::new(
                "invalid_params",
                format!("{name} must be an unsigned integer"),
            )
        })
}

fn plugin_optional_u64_param(
    params: &serde_json::Value,
    name: &str,
) -> Result<Option<u64>, AutomationError> {
    match params.get(name) {
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            AutomationError::new(
                "invalid_params",
                format!("{name} must be an unsigned integer"),
            )
        }),
        None => Ok(None),
    }
}
