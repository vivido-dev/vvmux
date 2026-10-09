//! Automation request dispatch, pane resolution, replies, and plugin event subscriptions.

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
    pub(super) fn handle_automation(
        &mut self,
        client_id: u64,
        writer: SharedWriter,
        cancel: crate::platform::ConnectionCancel,
        request: AutomationRequest,
    ) {
        let mut target = AutomationReplyTarget {
            client_id,
            request_id: request.id,
            writer,
            cancel,
            idempotency_key: None,
        };
        let requests = self.automation_inflight.entry(client_id).or_default();
        if requests.contains(&request.id) {
            self.send_automation_response(
                &target,
                AutomationResponse::error(
                    target.request_id,
                    "duplicate_request_id",
                    "request ID is already in flight",
                ),
            );
            return;
        }
        if requests.len() >= MAX_AUTOMATION_REQUESTS_PER_CLIENT {
            self.reply_automation_error(
                target,
                AutomationError::new("limit_exceeded", "too many in-flight requests"),
            );
            return;
        }
        requests.insert(request.id);
        if let Err(error) = validate_automation_method(&request.method) {
            self.reply_automation_error(target, error);
            return;
        }
        let caller = CallerContext {
            origin: CallerOrigin::Automation { client_id },
            session_instance: self.session_instance.clone(),
            focused_fallback: request.allow_focused,
            capabilities: plugin_enforceable_permissions().into_iter().collect(),
        };

        let pane_id = if method_needs_pane(&request.method) {
            // A `WaitExit` for a pane that already exited resolves to its tombstone.
            let exited = request.pane_id.filter(|pane| {
                matches!(&request.method, AutomationMethod::WaitExit { .. })
                    && self
                        .exit_tombstones
                        .iter()
                        .any(|exit| exit.pane_id == *pane)
            });
            let resolved = if let Some(pane) = exited {
                Ok(pane)
            } else {
                self.resolve_automation_pane(
                    &request.method,
                    request.pane_id,
                    request.agent.as_ref(),
                    request.pane_name.as_ref(),
                    request.allow_focused,
                )
            };
            match resolved {
                Ok(pane) => Some(pane),
                Err(error) => {
                    self.reply_automation_error(target, error);
                    return;
                }
            }
        } else {
            None
        };

        // Input is recorded as a class and a byte count and never as bytes: a recording is a file,
        // and what is typed into a terminal is passwords and tokens.
        if self.recorder.is_some()
            && let Some(pane_id) = pane_id
            && let Some(bytes) = automation_input_bytes(&request.method)
        {
            self.record(crate::record::RecordedEvent::Input {
                pane_id,
                bytes,
                class: request.method.name().to_owned(),
            });
        }
        // A lease is checked first: being refused because somebody else holds the pane is a
        // different answer from being refused because the screen moved, and the caller should get
        // the one that is actually blocking them.
        if !matches!(request.method, AutomationMethod::Lease(_)) {
            let class = crate::ipc::METHOD_CAPABILITIES
                .iter()
                .find(|capability| capability.name == request.method.name())
                .map(|capability| capability.class);
            if let Some(class) = class
                && let Err(error) = self.leases.check(pane_id, class, request.lease.as_deref())
            {
                self.reply_automation_error(target, error);
                return;
            }
        }
        // Both guards run after the pane is resolved and before any handler: an expectation about
        // a pane's screen needs to know which pane, and a replayed key must not reach a PTY.
        if let Some(expect) = request.expect
            && let Err(error) = self.check_expected_state(pane_id, expect)
        {
            self.reply_automation_error(target, error);
            return;
        }
        if let Some(key) = request.idempotency_key.clone() {
            match self.claim_idempotency_key(&request.method, key.clone()) {
                Ok(None) => target.idempotency_key = Some(key),
                Ok(Some(previous)) => {
                    self.reply_automation(target, previous);
                    return;
                }
                Err(error) => {
                    self.reply_automation_error(target, error);
                    return;
                }
            }
        }

        match request.method {
            AutomationMethod::Capabilities => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation(
                        target,
                        automation_capabilities(disabled_plugin_capabilities(
                            &self.session_instance,
                        )),
                    );
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.capabilities(target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::GetConfig => {
                match serde_json::to_value(&self.config) {
                    Ok(config) => self.reply_automation(
                        target,
                        serde_json::json!({
                            "path": self
                                .config_path
                                .as_ref()
                                .map(|path| path.display().to_string()),
                            "config": config,
                        }),
                    ),
                    // The config is plain data with no custom serializers, so this cannot fail in
                    // practice; reporting it beats an unwrap on the session actor.
                    Err(error) => self.reply_automation_error(
                        target,
                        AutomationError::new("serialization_failed", error.to_string()),
                    ),
                }
            }
            AutomationMethod::ReloadConfig => match self.reload_config() {
                Ok(report) => self.reply_automation(target, report.to_json()),
                Err(error) => {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_config", error),
                    );
                }
            },
            AutomationMethod::ListPanes => {
                match self.execute_session_command(&caller, SessionCommand::InspectSession) {
                    Ok(value) => self.reply_automation(target, value),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::SessionInspect => {
                self.reply_automation(target, self.automation_session_inspect());
            }
            AutomationMethod::ListClients => {
                self.reply_automation(target, self.automation_list_clients());
            }
            AutomationMethod::DetachClient { client_id } => {
                if self.client_is(client_id) {
                    self.detach_client(client_id, Some("detached by another client"));
                    self.reply_automation(
                        target,
                        serde_json::json!({"client_id": client_id, "detached": true}),
                    );
                } else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "client_not_found",
                            format!("no attached client has ID {client_id}"),
                        ),
                    );
                }
            }
            AutomationMethod::ListTabs => {
                self.reply_automation(target, self.automation_tabs());
            }
            AutomationMethod::Layout => {
                // `request.pane_id` rather than the resolved pane: `layout` is session-scoped, so
                // nothing was resolved, and the raw field is the inherited `VVMUX_PANE_ID` that
                // says where the caller is sitting.
                let caller = request.pane_id;
                self.reply_automation(target, self.automation_layout(caller));
            }
            AutomationMethod::ResolvePane { ref tab, ref path } => {
                // `pane_name` comes from the request's targeting fields like every other method's
                // does. `resolve_pane` is session-scoped, so nothing consumed it, and a second
                // copy on the method would be two ways to say one thing.
                match self.automation_resolve_pane(
                    request.pane_id,
                    tab.as_ref(),
                    path,
                    request.pane_name.as_ref(),
                ) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::NewTab { ref name } => match self.automation_new_tab(name.clone()) {
                Ok(result) => self.reply_automation(target, result),
                Err(error) => self.reply_automation_error(target, error),
            },
            AutomationMethod::RenameTab { ref tab, ref name } => {
                match self.automation_rename_tab(tab, Some(name.clone())) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::ResetTabTitle { ref tab } => {
                match self.automation_rename_tab(tab, None) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::CloseTab { ref tab } => match self.automation_close_tab(tab) {
                Ok(result) => self.reply_automation(target, result),
                Err(error) => self.reply_automation_error(target, error),
            },
            AutomationMethod::SelectTab {
                ref tab,
                wait,
                timeout_ms,
            } => {
                let index = match self.resolve_tab(tab) {
                    Ok(index) => index,
                    Err(error) => {
                        self.reply_automation_error(target, error);
                        return;
                    }
                };
                let tab_id = self.tabs[index].id;
                let before_outer = self.outer_projection_revision;
                self.active_tab = index;
                self.force_full = true;
                self.relayout();
                let result = serde_json::json!({
                    "tab_id": tab_id,
                    "focused_pane_id": self.tabs[index].focused,
                    "session_sequence": self.session_sequence,
                    "layout_sequence": self.layout_revision,
                });
                self.finish_selection(target, wait, timeout_ms, before_outer, result);
            }
            AutomationMethod::Diagnose {
                pane_id: requested_pane,
                all_panes,
                trace_limit,
            } => match self.automation_diagnose(requested_pane, all_panes, trace_limit) {
                Ok(result) => self.reply_automation(target, result),
                Err(error) => self.reply_automation_error(target, error),
            },
            AutomationMethod::ReportAgent {
                agent,
                state,
                source,
                sequence,
                message,
                session_id,
                session_path,
            } => {
                let pane_id = required_pane(pane_id);
                let visible = self.pane_is_visibly_present(pane_id);
                let result = self
                    .agent_catalog
                    .identity(&agent)
                    .ok_or("agent definition is not enabled")
                    .and_then(|identity| {
                        self.panes
                            .get_mut(&pane_id)
                            .ok_or("pane no longer exists")
                            .and_then(|pane| {
                                pane.agent.report(
                                    crate::agent::AgentReport {
                                        identity,
                                        state,
                                        source,
                                        sequence,
                                        message,
                                        session: crate::agent::AgentSessionRef::new(
                                            session_id,
                                            session_path,
                                        ),
                                    },
                                    visible,
                                    Instant::now(),
                                )
                            })
                    });
                match result {
                    Ok(()) => {
                        self.sync_agent_status();
                        let agent = self.panes[&pane_id].agent.snapshot().map(agent_json);
                        self.reply_automation(
                            target,
                            serde_json::json!({"pane_id": pane_id, "agent": agent}),
                        );
                    }
                    Err(message) => self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_agent_report", message),
                    ),
                }
            }
            AutomationMethod::ReportAgentSession {
                agent,
                source,
                sequence,
                session_id,
                session_path,
            } => {
                let pane_id = required_pane(pane_id);
                let result = self
                    .agent_catalog
                    .identity(&agent)
                    .ok_or("agent definition is not enabled")
                    .and_then(|identity| {
                        let session = crate::agent::AgentSessionRef::new(session_id, session_path)
                            .ok_or("agent session identity is required")?;
                        self.panes
                            .get_mut(&pane_id)
                            .ok_or("pane no longer exists")?
                            .agent
                            .report_session(identity, &source, sequence, session)
                    });
                match result {
                    Ok(()) => {
                        let agent = self.panes[&pane_id].agent.snapshot().map(agent_json);
                        self.reply_automation(
                            target,
                            serde_json::json!({"pane_id": pane_id, "agent": agent}),
                        );
                    }
                    Err(message) => self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_agent_report", message),
                    ),
                }
            }
            AutomationMethod::ReportMetadata {
                source,
                sequence,
                tokens,
                ttl_ms,
                display_agent,
                state_labels,
                title,
            } => {
                let pane_id = required_pane(pane_id);
                let patch = crate::agent::AgentMetadataPatch {
                    tokens,
                    ttl: ttl_ms.map(Duration::from_millis),
                    display_agent,
                    state_labels,
                    title,
                };
                let result = self
                    .panes
                    .get_mut(&pane_id)
                    .ok_or("pane no longer exists")
                    .and_then(|pane| {
                        pane.agent
                            .report_metadata(&source, sequence, patch, Instant::now())
                    });
                match result {
                    Ok(changed) => {
                        if changed {
                            self.note_agent_display_change();
                        }
                        let metadata = self.panes[&pane_id].agent.metadata();
                        self.reply_automation(
                            target,
                            serde_json::json!({
                                "pane_id": pane_id,
                                "changed": changed,
                                "metadata": agent_metadata_json(metadata),
                            }),
                        );
                    }
                    Err(message) => self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_agent_report", message),
                    ),
                }
            }
            AutomationMethod::ClearAgentReport { source, sequence } => {
                let pane_id = required_pane(pane_id);
                let result = self
                    .panes
                    .get_mut(&pane_id)
                    .ok_or("pane no longer exists")
                    .and_then(|pane| pane.agent.clear_report(&source, sequence));
                match result {
                    Ok(()) => {
                        self.evaluate_agent_states();
                        let agent = self.panes[&pane_id].agent.snapshot().map(agent_json);
                        self.reply_automation(
                            target,
                            serde_json::json!({"pane_id": pane_id, "agent": agent}),
                        );
                    }
                    Err(message) => self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_agent_report", message),
                    ),
                }
            }
            AutomationMethod::SessionSnapshot => {
                let path = self.snapshot_paths.as_ref().map(|paths| &paths.snapshot);
                let written = path.and_then(|path| std::fs::metadata(path).ok());
                self.reply_automation(
                    target,
                    serde_json::json!({
                        "enabled": self.snapshot_paths.is_some(),
                        "path": path.map(|path| path.display().to_string()),
                        "schema": crate::session_state::SNAPSHOT_SCHEMA,
                        "bytes": written.as_ref().map(std::fs::Metadata::len),
                        "written": written.is_some(),
                        "restored_from_snapshot": self.restored_from_snapshot,
                        "pending_write": self.snapshot_dirty || self.snapshot_writing,
                    }),
                );
            }
            AutomationMethod::AgentRename { alias } => {
                let pane_id = required_pane(pane_id);
                // A launch in flight has no agent yet, and the one it is waiting for may never
                // arrive. Naming it now would attach the alias to whatever the pane's shell is
                // running instead, so the caller is told to wait for the launch to settle.
                let launch_pending = self.automation_waiters.iter().any(|waiter| {
                    waiter.pane_id == Some(pane_id)
                        && matches!(waiter.kind, AutomationWaitKind::AgentLaunch { .. })
                });
                let taken = alias
                    .as_ref()
                    .and_then(|alias| self.pane_with_agent_alias(alias))
                    .filter(|holder| *holder != pane_id);
                let result = if launch_pending {
                    Err(AutomationError::new(
                        "agent_launch_pending",
                        format!("pane {pane_id} is still starting an agent"),
                    ))
                } else if let Some(holder) = taken {
                    Err(AutomationError::new(
                        "agent_alias_taken",
                        format!(
                            "{} already names the agent in pane {holder}",
                            alias.as_ref().expect("taken implies an alias")
                        ),
                    ))
                } else {
                    self.panes
                        .get_mut(&pane_id)
                        .ok_or("pane no longer exists")
                        .and_then(|pane| pane.agent.set_alias(alias))
                        .map_err(|message| AutomationError::new("agent_not_detected", message))
                };
                match result {
                    Ok(changed) => {
                        if changed {
                            // The navigator and status row show the alias, so a rename is a display
                            // change. It is deliberately not a lifecycle change: see the `alias`
                            // field on `AgentRuntime`.
                            self.note_agent_display_change();
                        }
                        let pane = &self.panes[&pane_id];
                        self.reply_automation(
                            target,
                            serde_json::json!({
                                "pane_id": pane_id,
                                "changed": changed,
                                "alias": pane.agent.alias(),
                                "agent": pane.agent.snapshot().map(agent_json),
                            }),
                        );
                    }
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::AgentExplain => {
                let pane_id = required_pane(pane_id);
                let Some(pane) = self.panes.get(&pane_id) else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("pane_not_found", "pane no longer exists"),
                    );
                    return;
                };
                let Some(explanation) =
                    pane.agent
                        .explain(&self.agent_catalog, &pane.terminal, Instant::now())
                else {
                    // An empty success would read as "explained: nothing", when the real answer is
                    // that classification never ran for this pane.
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "agent_not_detected",
                            "pane has no detected or reported agent",
                        ),
                    );
                    return;
                };
                match serde_json::to_value(explanation) {
                    Ok(value) => self.reply_automation(
                        target,
                        serde_json::json!({"pane_id": pane_id, "explain": value}),
                    ),
                    Err(error) => self.reply_automation_error(
                        target,
                        AutomationError::new("serialization_failed", error.to_string()),
                    ),
                }
            }
            AutomationMethod::Inspect => {
                let pane_id = required_pane(pane_id);
                // The one surface that names a single pane on the owner-only socket, so the only
                // one that discloses a reported native agent session reference.
                let Some(pane) = self.pane_description(pane_id, AgentDisclosure::Full) else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("pane_not_found", "pane no longer exists"),
                    );
                    return;
                };
                self.reply_automation(
                    target,
                    serde_json::json!({
                        "session": self.name,
                        "session_sequence": self.session_sequence,
                        "layout_sequence": self.layout_revision,
                        "rendered_session_sequence": self.rendered_session_sequence(),
                        "pane": pane,
                        "limits": automation_limits(),
                    }),
                );
            }
            AutomationMethod::PaneRename { ref name } => {
                let pane_id = required_pane(pane_id);
                match self.automation_pane_rename(pane_id, name.clone()) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::ActivatePane => {
                let pane_id = required_pane(pane_id);
                match self.automation_activate_pane(pane_id) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Record(ref operation) => match self.automation_record(operation) {
                Ok(result) => self.reply_automation(target, result),
                Err(error) => self.reply_automation_error(target, error),
            },
            AutomationMethod::Lease(ref operation) => {
                let instance = self.session_instance.clone();
                let result = match operation {
                    crate::ipc::LeaseOperation::Acquire {
                        scope,
                        ttl_ms,
                        holder,
                    } => self.leases.acquire(
                        required_pane(pane_id),
                        *scope,
                        Duration::from_millis(*ttl_ms),
                        holder.clone(),
                        &instance,
                    ),
                    crate::ipc::LeaseOperation::Renew { lease_id, ttl_ms } => {
                        self.leases.renew(lease_id, Duration::from_millis(*ttl_ms))
                    }
                    crate::ipc::LeaseOperation::Release { lease_id } => {
                        self.leases.release(lease_id)
                    }
                    crate::ipc::LeaseOperation::List => Ok(self.leases.list()),
                };
                match result {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::ShellCommand {
                ref command,
                timeout_ms,
            } => {
                let pane_id = required_pane(pane_id);
                match self.start_shell_command(pane_id, command) {
                    Ok(kind) => self.add_automation_waiter(AutomationWaiter {
                        reply: target,
                        pane_id: Some(pane_id),
                        deadline: deadline(timeout_ms),
                        kind,
                    }),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::CaptureMedia {
                ref path,
                scale,
                force,
                timeout_ms,
            } => {
                let pane_id = required_pane(pane_id);
                if scale <= 1 {
                    match self.capture_media_payload(pane_id, path) {
                        Ok(result) => self.reply_automation(target, result),
                        Err(error) => self.reply_automation_error(target, error),
                    }
                    return;
                }
                match self.begin_scaled_capture(pane_id, scale, force) {
                    Ok(baseline) => self.add_automation_waiter(AutomationWaiter {
                        reply: target,
                        pane_id: Some(pane_id),
                        deadline: deadline(timeout_ms),
                        kind: AutomationWaitKind::CaptureMedia {
                            path: path.clone(),
                            baseline,
                            settling: None,
                        },
                    }),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Capture {
                no_activate,
                after_screen,
                stable_ms,
                rendered,
                grid,
                timeout_ms,
            } => {
                let pane_id = required_pane(pane_id);
                // Activation first and synchronously: a pane nobody can see has no settled state
                // to wait for, and its media projection is not running either.
                if !no_activate && let Err(error) = self.automation_activate_pane(pane_id) {
                    self.reply_automation_error(target, error);
                    return;
                }
                let after_session = rendered.then_some(self.session_sequence);
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id: Some(pane_id),
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::Capture {
                        after_screen,
                        quiet: stable_ms.map(Duration::from_millis),
                        rendered_after_session: after_session,
                        grid,
                    },
                });
            }
            AutomationMethod::Mouse {
                action,
                position,
                button,
                route,
                shift,
                alt,
                ctrl,
                scroll,
                ref points,
                ..
            } => {
                let pane_id = required_pane(pane_id);
                let request = MouseRequest {
                    action,
                    position,
                    button,
                    route,
                    shift,
                    alt,
                    ctrl,
                    scroll,
                    points: points.clone(),
                };
                match self.automation_mouse(pane_id, request) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Transcript {
                after_offset,
                base64,
                max_bytes,
            } => {
                let pane_id = required_pane(pane_id);
                match self.automation_transcript(pane_id, after_offset, base64, max_bytes) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::WaitOutput {
                pattern,
                regex,
                after_offset,
                timeout_ms,
            } => {
                let pattern = match automation_text_pattern(pattern, regex) {
                    Ok(pattern) => pattern,
                    Err(error) => {
                        self.reply_automation_error(target, error);
                        return;
                    }
                };
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::Output {
                        pattern,
                        after_offset,
                    },
                });
            }
            AutomationMethod::ResizePane { columns, rows } => {
                let pane_id = required_pane(pane_id);
                match self.automation_resize_pane(pane_id, columns, rows) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::MovePane {
                ref to_tab,
                swap,
                to_layer,
            } => {
                let pane_id = required_pane(pane_id);
                match self.automation_move_pane(pane_id, to_tab.as_ref(), swap, to_layer) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::SetFlag {
                flag,
                enabled,
                offset,
            } => {
                let pane_id = required_pane(pane_id);
                match self.automation_set_flag(pane_id, flag, enabled, offset) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Signal { signal } => {
                let pane_id = required_pane(pane_id);
                match self.automation_signal(pane_id, signal) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::InspectMedia => {
                let pane_id = required_pane(pane_id);
                let status = self.vivid.pane_status(
                    pane_id,
                    self.outer_media_projection(),
                    self.relay_metrics(),
                );
                self.reply_automation(target, serde_json::to_value(status).unwrap());
            }
            AutomationMethod::TraceMedia {
                after_sequence,
                limit,
                timeout_ms,
                filter,
            } => {
                let pane_id = required_pane(pane_id);
                let result = self
                    .media_trace
                    .query(after_sequence, limit, Some(pane_id), filter);
                if timeout_ms == 0 || result.gap.is_some() || !result.events.is_empty() {
                    self.reply_automation(target, serde_json::to_value(result).unwrap());
                } else {
                    self.add_automation_waiter(AutomationWaiter {
                        reply: target,
                        pane_id: Some(pane_id),
                        deadline: deadline(timeout_ms),
                        kind: AutomationWaitKind::MediaTrace {
                            after_sequence,
                            limit,
                            filter,
                        },
                    });
                }
            }
            AutomationMethod::Split { axis } => {
                let pane_id = required_pane(pane_id);
                match self.automation_split(pane_id, axis) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::SaveLayout { path } => match self.automation_save_layout(path) {
                Ok(result) => self.reply_automation(target, result),
                Err(error) => self.reply_automation_error(target, error),
            },
            AutomationMethod::Run {
                command,
                placement,
                cwd,
                hold,
                focus,
            } => {
                let pane_id = required_pane(pane_id);
                match self.automation_run(pane_id, command, placement, cwd, hold, focus) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Focus => {
                let pane_id = required_pane(pane_id);
                match self.automation_focus(pane_id) {
                    Ok(()) => self.reply_automation(target, serde_json::Value::Null),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::FocusWait { wait, timeout_ms } => {
                let pane_id = required_pane(pane_id);
                let before_outer = self.outer_projection_revision;
                match self.automation_focus(pane_id) {
                    Ok(()) => {
                        let result = serde_json::json!({
                            "pane_id": pane_id,
                            "tab_id": self.tabs[self.active_tab].id,
                            "session_sequence": self.session_sequence,
                            "layout_sequence": self.layout_revision,
                        });
                        self.finish_selection(target, Some(wait), timeout_ms, before_outer, result);
                    }
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::ClosePane => {
                let pane_id = required_pane(pane_id);
                self.close_pane(pane_id);
                self.reply_automation(target, serde_json::Value::Null);
            }
            AutomationMethod::Typing { text, report } => {
                self.automation_input(target, required_pane(pane_id), text.into_bytes(), report);
            }
            AutomationMethod::Key {
                key,
                modifiers,
                repeat,
                report,
            } => {
                let pane_id = required_pane(pane_id);
                let Some(pane) = self.panes.get(&pane_id) else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("pane_not_found", "pane no longer exists"),
                    );
                    return;
                };
                match encode_automation_key(&key, &modifiers, pane.terminal.modes()) {
                    Ok(encoded) => {
                        let total = encoded.len().saturating_mul(usize::from(repeat));
                        if total > 1024 * 1024 {
                            self.reply_automation_error(
                                target,
                                AutomationError::new(
                                    "limit_exceeded",
                                    "encoded key input exceeds 1 MiB",
                                ),
                            );
                            return;
                        }
                        self.automation_input(
                            target,
                            pane_id,
                            encoded.repeat(usize::from(repeat)),
                            report,
                        );
                    }
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Paste { text, report } => {
                let pane_id = required_pane(pane_id);
                let bracketed = self
                    .panes
                    .get(&pane_id)
                    .is_some_and(|pane| pane.terminal.modes().bracketed_paste);
                let mut bytes = sanitize_bracketed_paste(text.as_bytes());
                if bracketed {
                    bytes.splice(0..0, b"\x1b[200~".iter().copied());
                    bytes.extend_from_slice(b"\x1b[201~");
                }
                self.automation_input(target, pane_id, bytes, report);
            }
            AutomationMethod::SubmitLine { text, report } => {
                let pane_id = required_pane(pane_id);
                let Some(pane) = self.panes.get(&pane_id) else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("pane_not_found", "pane no longer exists"),
                    );
                    return;
                };
                let modes = pane.terminal.modes();
                let enter = match encode_automation_key("Enter", &[], modes) {
                    Ok(enter) => enter,
                    Err(error) => {
                        self.reply_automation_error(target, error);
                        return;
                    }
                };
                let mut bytes = sanitize_bracketed_paste(text.as_bytes());
                if modes.bracketed_paste {
                    bytes.splice(0..0, b"\x1b[200~".iter().copied());
                    bytes.extend_from_slice(b"\x1b[201~");
                }
                // One buffer, one write, one completion. Splitting this into `typing` then
                // `key Enter` is what lets a failure between them strand a half-typed command at
                // the prompt, which is exactly what a retry loop must not have to reason about.
                bytes.extend_from_slice(&enter);
                self.automation_input(target, pane_id, bytes, report);
            }
            AutomationMethod::AgentStart {
                agent,
                args,
                timeout_ms,
            } => {
                let pane_id = required_pane(pane_id);
                self.agent_start(target, pane_id, agent, args, timeout_ms);
            }
            AutomationMethod::AgentPrompt {
                text,
                wait,
                until,
                timeout_ms,
            } => {
                let pane_id = required_pane(pane_id);
                self.agent_prompt(target, pane_id, text, wait, until, timeout_ms);
            }
            AutomationMethod::AgentSendKeys { keys } => {
                let pane_id = required_pane(pane_id);
                self.agent_send_keys(target, pane_id, keys);
            }
            AutomationMethod::AgentRead { lines, json } => {
                self.agent_read(target, required_pane(pane_id), usize::from(lines), json);
            }
            AutomationMethod::GetText { rows, source } => {
                match self.execute_session_command(
                    &caller,
                    SessionCommand::ReadPaneText {
                        pane_id,
                        rows: rows.map(usize::from),
                        source,
                        max_bytes: AUTOMATION_REPLY_LIMIT,
                    },
                ) {
                    Ok(value) => self.reply_automation(target, value),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::GetGrid {
                start_line,
                row_count,
                since_screen,
            } => {
                let pane_id = required_pane(pane_id);
                match self.grid_snapshot(pane_id, start_line, row_count, since_screen) {
                    Ok(snapshot) => self.reply_automation(target, snapshot),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Search {
                pattern,
                regex,
                direction,
                start_line,
                start_column,
                limit,
            } => {
                let pane = &self.panes[&required_pane(pane_id)];
                match crate::search::compile(&pattern, regex, true) {
                    Ok(compiled) => {
                        let (matches, truncated) = automation_search(
                            &pane.terminal,
                            &compiled,
                            direction,
                            start_line,
                            start_column,
                            usize::from(limit),
                        );
                        self.reply_automation(
                            target,
                            serde_json::json!({
                                "matches": matches,
                                "truncated": truncated,
                            }),
                        );
                    }
                    Err(error) => self.reply_automation_error(
                        target,
                        AutomationError::new("invalid_params", error),
                    ),
                }
            }
            AutomationMethod::SetSyncInput { enabled } => {
                let pane_id = required_pane(pane_id);
                let Some(tab_index) = self.tabs.iter().position(|tab| tab.contains(pane_id)) else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("pane_not_found", "pane no longer exists"),
                    );
                    return;
                };
                self.tabs[tab_index].sync_input = enabled;
                self.session_sequence = self.session_sequence.wrapping_add(1);
                if tab_index == self.active_tab {
                    self.force_full = true;
                    self.schedule_render();
                }
                self.reply_automation(
                    target,
                    serde_json::json!({
                        "tab_id": self.tabs[tab_index].id,
                        "sync_input": enabled,
                    }),
                );
            }
            AutomationMethod::Action(action) => {
                let pane_id = required_pane(pane_id);
                match self.automation_action(pane_id, action) {
                    Ok(result) => self.reply_automation(target, result),
                    Err(error) => self.reply_automation_error(target, error),
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::Invoke {
                reference,
                input,
                detach,
            }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if detach {
                    match supervisor.invoke_detached(reference, input) {
                        Ok(job_id) => self.reply_automation(
                            target,
                            serde_json::json!({"job_id": job_id, "status": "queued"}),
                        ),
                        Err(error) => self.reply_automation_error(target, error),
                    }
                    return;
                }
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.invoke_automation(reference, input, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::JobStatus { job_id }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.job_status(job_id, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::JobCancel { job_id }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.job_cancel(job_id, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::JobLogs { job_id }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.job_logs(job_id, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::PaneOpen { reference }) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation_error(target, plugin_disabled_error());
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.open_pane(reference, target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::EventSubscribe {
                after_sequence,
            }) => {
                self.subscribe_plugin_events(
                    target,
                    after_sequence,
                    crate::ipc::EventFilter::default(),
                    true,
                );
            }
            AutomationMethod::Subscribe {
                after_sequence,
                filter,
            } => {
                self.subscribe_plugin_events(target, after_sequence, filter, false);
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::EventUnsubscribe {
                subscription_id,
            }) => {
                let removed = self
                    .plugin_event_subscriptions
                    .get(&subscription_id)
                    .is_some_and(|subscription| subscription.client_id == client_id)
                    && self
                        .plugin_event_subscriptions
                        .remove(&subscription_id)
                        .is_some();
                if removed {
                    self.reply_automation(
                        target,
                        serde_json::json!({"subscription_id": subscription_id, "subscribed": false}),
                    );
                } else {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "scope_denied",
                            "event subscription is not owned by this client",
                        ),
                    );
                }
            }
            AutomationMethod::Plugin(crate::ipc::PluginMethod::Reload) => {
                let Some(supervisor) = self.plugin_supervisor.clone() else {
                    self.reply_automation(
                        target,
                        serde_json::json!({
                            "disabled": true,
                            "generation": null,
                            "applied": [],
                            "deferred": [],
                            "failed": {},
                        }),
                    );
                    return;
                };
                if !self.register_pending_actor_work(&target) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("busy", "session pending-work quota is exhausted"),
                    );
                    return;
                }
                if let Err(error) = supervisor.reload_automation(target.clone()) {
                    self.complete_pending_actor_work(&target);
                    self.reply_automation_error(target, error);
                }
            }
            AutomationMethod::WaitText {
                text,
                regex,
                after_screen,
                timeout_ms,
            } => {
                let current = self.panes[&required_pane(pane_id)].screen_sequence;
                if after_screen.is_some_and(|sequence| sequence > current) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "sequence_gap",
                            format!(
                                "screen sequence is {current}, before requested {after_screen:?}"
                            ),
                        ),
                    );
                    return;
                }
                let pattern = if regex {
                    if text.len() > 8 * 1024 {
                        self.reply_automation_error(
                            target,
                            AutomationError::new(
                                "limit_exceeded",
                                "regular expression exceeds 8 KiB",
                            ),
                        );
                        return;
                    }
                    match regex::Regex::new(&text) {
                        Ok(regex) => AutomationTextPattern::Regex(regex),
                        Err(error) => {
                            self.reply_automation_error(
                                target,
                                AutomationError::new("regex_invalid", error.to_string()),
                            );
                            return;
                        }
                    }
                } else {
                    AutomationTextPattern::Literal(text)
                };
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::Text {
                        pattern,
                        after_screen,
                    },
                });
            }
            AutomationMethod::WaitScreenChange {
                after_screen,
                timeout_ms,
            } => {
                let pane_id = required_pane(pane_id);
                if after_screen
                    .is_some_and(|sequence| sequence > self.panes[&pane_id].screen_sequence)
                {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "sequence_gap",
                            "after-screen is newer than the pane screen sequence",
                        ),
                    );
                    return;
                }
                let after_screen = after_screen.unwrap_or(self.panes[&pane_id].screen_sequence);
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id: Some(pane_id),
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::ScreenChange { after_screen },
                });
            }
            AutomationMethod::WaitScreenStable {
                quiet_ms,
                ignore_bottom,
                after_screen,
                timeout_ms,
            } => {
                let current = self.panes[&required_pane(pane_id)].screen_sequence;
                if after_screen.is_some_and(|sequence| sequence > current) {
                    self.reply_automation_error(
                        target,
                        AutomationError::new(
                            "sequence_gap",
                            "after-screen is newer than the pane screen sequence",
                        ),
                    );
                    return;
                }
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::ScreenStable {
                        quiet: Duration::from_millis(quiet_ms),
                        ignore_bottom,
                        after_screen,
                    },
                });
            }
            AutomationMethod::WaitRendered {
                after_session,
                timeout_ms,
            } => {
                if self.clients.is_empty() {
                    self.reply_automation_error(
                        target,
                        AutomationError::new("unsupported", "session has no attached client"),
                    );
                } else {
                    self.add_automation_waiter(AutomationWaiter {
                        reply: target,
                        pane_id: None,
                        deadline: deadline(timeout_ms),
                        kind: AutomationWaitKind::Rendered { after_session },
                    });
                }
            }
            AutomationMethod::WaitAgentState { until, timeout_ms } => {
                let pane_id = required_pane(pane_id);
                // A pane without an agent is not an error: detection needs a process-tree poll and
                // a startup grace, so "launch an agent, then wait for it" — the flow this command
                // exists for — always registers before the agent is visible. Absence is simply
                // "not matching yet", and the caller's timeout is what bounds it. A wait that ends
                // without an agent ever appearing says so.
                let initial = self
                    .panes
                    .get(&pane_id)
                    .and_then(|pane| pane.agent.snapshot())
                    .map(|snapshot| snapshot.status);
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id: Some(pane_id),
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::AgentState { until, initial },
                });
            }
            AutomationMethod::WaitExit { timeout_ms } => {
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::Exit,
                });
            }
            AutomationMethod::WaitMedia {
                after_virtual_revision,
                after_outer_revision,
                timeout_ms,
            } => {
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::Media {
                        after_virtual_revision,
                        after_outer_revision,
                    },
                });
            }
            AutomationMethod::WaitMediaTrack {
                identity,
                condition,
                timeout_ms,
            } => {
                self.add_automation_waiter(AutomationWaiter {
                    reply: target,
                    pane_id,
                    deadline: deadline(timeout_ms),
                    kind: AutomationWaitKind::MediaTrack {
                        identity,
                        condition,
                    },
                });
            }
        }
    }

    /// The pane holding the agent named `alias`, if one does.
    ///
    /// A linear scan rather than a reverse index: a session holds at most
    /// [`MAX_SESSION_PANES`] panes, and an index would be a second copy of state
    /// that has to be invalidated everywhere an alias is cleared — including the process-exit paths
    /// inside `AgentRuntime`, which know nothing about the session. Scanning cannot go stale.
    pub(super) fn pane_with_agent_alias(&self, alias: &crate::agent::AgentAlias) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|(_, pane)| pane.agent.alias() == Some(alias))
            .map(|(pane_id, _)| *pane_id)
    }

    pub(super) fn resolve_automation_pane(
        &self,
        method: &AutomationMethod,
        requested: Option<PaneId>,
        alias: Option<&crate::agent::AgentAlias>,
        pane_name: Option<&crate::layout::PaneName>,
        allow_focused: bool,
    ) -> Result<PaneId, AutomationError> {
        if let Some(pane) = requested {
            return self
                .panes
                .contains_key(&pane)
                .then_some(pane)
                .ok_or_else(|| {
                    AutomationError::new("pane_not_found", format!("pane {pane} does not exist"))
                });
        }
        // An alias outranks the focused pane but never an explicit pane ID: a caller that named both
        // is answered by the more specific one, and a caller that named an agent meant that agent
        // rather than wherever focus happens to be.
        if let Some(alias) = alias {
            return self.pane_with_agent_alias(alias).ok_or_else(|| {
                AutomationError::new(
                    "agent_alias_not_found",
                    format!("no agent is named {alias}"),
                )
            });
        }
        // Below the alias: an alias names a process that may have moved, a name names this pane.
        // A caller that gave both meant the agent, and is told so by being sent to it.
        if let Some(name) = pane_name {
            return self.pane_with_name(name).ok_or_else(|| {
                AutomationError::new("pane_not_found", format!("no pane is named {name}"))
            });
        }
        if allow_focused {
            return self
                .active_tab()
                .map(|tab| tab.focused)
                .filter(|pane| self.panes.contains_key(pane))
                .ok_or_else(|| AutomationError::new("no_focused_pane", "no focused vvmux pane"));
        }
        // Name the method: a caller batching several requests gets one error per request and
        // otherwise cannot tell which of them was the one missing a target.
        Err(AutomationError::new(
            "invalid_params",
            format!("{} requires a pane ID", method.name()),
        ))
    }

    pub(super) fn reply_automation(
        &mut self,
        target: AutomationReplyTarget,
        result: serde_json::Value,
    ) {
        // Only a success is remembered. A failed request changed nothing, so a retry of it should
        // run rather than be handed back the failure.
        if let Some(key) = &target.idempotency_key
            && let Some(slot) = self.idempotency_keys.get_mut(key)
        {
            *slot = result.clone();
        }
        self.finish_automation_request(target.client_id, target.request_id);
        self.send_automation_response(
            &target,
            AutomationResponse::success(target.request_id, result),
        );
    }

    pub(super) fn reply_automation_error(
        &mut self,
        target: AutomationReplyTarget,
        error: AutomationError,
    ) {
        // Release the claim: nothing happened, so the key must not shadow a later attempt.
        if let Some(key) = &target.idempotency_key {
            self.idempotency_keys.remove(key);
            self.idempotency_order.retain(|held| held != key);
        }
        self.finish_automation_request(target.client_id, target.request_id);
        self.send_automation_response(
            &target,
            AutomationResponse {
                id: target.request_id,
                ok: false,
                result: None,
                error: Some(error),
            },
        );
    }

    pub(super) fn subscribe_plugin_events(
        &mut self,
        target: AutomationReplyTarget,
        after_sequence: Option<u64>,
        filter: crate::ipc::EventFilter,
        require_plugins: bool,
    ) {
        // `msg subscribe` deliberately does not require plugins: pane, layout, and agent events
        // describe the session, not the plugin system. The plugin-facing subscription keeps its
        // gate, since a plugin stream with no plugin runtime is a caller mistake worth naming.
        if require_plugins && self.plugin_supervisor.is_none() {
            self.reply_automation_error(target, plugin_disabled_error());
            return;
        }
        if self.plugin_event_subscriptions.len() >= PLUGIN_EVENT_SUBSCRIPTIONS {
            self.reply_automation_error(
                target,
                AutomationError::new("busy", "plugin event subscription limit reached"),
            );
            return;
        }
        let subscription_id = format!(
            "{}/events-{:016x}",
            self.session_instance, self.next_plugin_subscription
        );
        self.next_plugin_subscription = self.next_plugin_subscription.wrapping_add(1).max(1);
        let (sender, receiver) = mpsc::sync_channel(PLUGIN_EVENT_STREAM_QUEUE);
        let writer = Arc::clone(&target.writer);
        let cancel = target.cancel.clone();
        let stream_subscription_id = subscription_id.clone();
        if std::thread::Builder::new()
            .name("vvmux-plugin-event-stream".into())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    let message = match message {
                        PluginStreamMessage::Response(response) => {
                            ServerMessage::Automation(response)
                        }
                        PluginStreamMessage::Event(envelope) => ServerMessage::PluginEvent {
                            subscription_id: stream_subscription_id.clone(),
                            envelope: Box::new(envelope),
                        },
                    };
                    if crate::ipc::send(&writer, &message).is_err() {
                        cancel.cancel();
                        break;
                    }
                }
            })
            .is_err()
        {
            self.reply_automation_error(
                target,
                AutomationError::new("runtime_unavailable", "could not start event stream"),
            );
            return;
        }
        self.finish_automation_request(target.client_id, target.request_id);
        let response = AutomationResponse::success(
            target.request_id,
            serde_json::json!({
                "subscription_id": subscription_id,
                "after_sequence": after_sequence,
                "latest_sequence": self.plugin_event_sequence,
            }),
        );
        if sender
            .try_send(PluginStreamMessage::Response(response))
            .is_err()
        {
            target.cancel.cancel();
            return;
        }
        if let Some(after) = after_sequence {
            // Replay is computed against the global sequence and filtered afterwards, so retention
            // gaps stay truthful. A filtered stream will show jumps in event sequence numbers;
            // that is the filter working, and it is distinguishable from a gap record.
            for envelope in self.plugin_event_journal.replay(
                after,
                self.plugin_event_sequence,
                PLUGIN_EVENT_STREAM_QUEUE.saturating_sub(1),
            ) {
                if !filter.accepts(&envelope) {
                    continue;
                }
                if sender
                    .try_send(PluginStreamMessage::Event(envelope))
                    .is_err()
                {
                    target.cancel.cancel();
                    return;
                }
            }
        }
        self.plugin_event_subscriptions.insert(
            subscription_id,
            PluginEventSubscription {
                client_id: target.client_id,
                sender,
                cancel: target.cancel,
                filter,
            },
        );
    }

    pub(super) fn queue_plugin_state_event(
        &mut self,
        name: &str,
        key: String,
        payload: serde_json::Value,
        pane_id: Option<PaneId>,
    ) {
        self.pending_plugin_state_events.insert(
            (name.to_owned(), key),
            (payload, pane_id, self.active_plugin_cause.clone()),
        );
    }

    pub(super) fn flush_plugin_state_events(&mut self) {
        let events = std::mem::take(&mut self.pending_plugin_state_events);
        for ((name, _), (payload, pane_id, cause)) in events {
            let previous_cause = std::mem::replace(&mut self.active_plugin_cause, cause);
            self.publish_plugin_event(&name, payload, pane_id, None);
            self.active_plugin_cause = previous_cause;
        }
    }

    pub(super) fn publish_plugin_event(
        &mut self,
        name: &str,
        payload: serde_json::Value,
        pane_id: Option<PaneId>,
        context: Option<vvmux_plugin_api::InvocationContext>,
    ) {
        self.plugin_event_sequence = self.plugin_event_sequence.saturating_add(1);
        let sequence = self.plugin_event_sequence;
        let cause = self.active_plugin_cause.clone();
        let context = context.unwrap_or_else(|| vvmux_plugin_api::InvocationContext {
            correlation_id: cause.as_ref().map_or_else(
                || format!("{}-event-{sequence:016x}", self.session_instance),
                |cause| cause.correlation_id.clone(),
            ),
            causation_id: cause.as_ref().map_or_else(
                || format!("{}-event-{sequence:016x}", self.session_instance),
                |cause| cause.causation_id.clone(),
            ),
            causation_depth: cause.as_ref().map_or(0, |cause| cause.causation_depth),
            source: cause.map_or_else(|| "session".into(), |cause| cause.source),
            session_instance: self.session_instance.clone(),
            pane_id,
            tab_id: pane_id.and_then(|pane_id| {
                self.tabs
                    .iter()
                    .find(|tab| tab.contains(pane_id))
                    .map(|tab| tab.id)
            }),
            deadline_unix_ms: 0,
        });
        let envelope = PluginEventEnvelope::Event {
            sequence,
            name: name.to_owned(),
            payload,
            context,
        };
        self.plugin_event_journal.push(envelope.clone());
        self.plugin_event_subscriptions.retain(|_, subscription| {
            if !subscription.filter.accepts(&envelope) {
                return true;
            }
            let sent = subscription
                .sender
                .try_send(PluginStreamMessage::Event(envelope.clone()))
                .is_ok();
            if !sent {
                subscription.cancel.cancel();
            }
            sent
        });
        if let Some(supervisor) = &self.plugin_supervisor {
            supervisor.publish_event(envelope);
        }
    }

    pub(super) fn send_automation_response(
        &self,
        target: &AutomationReplyTarget,
        response: AutomationResponse,
    ) {
        let job = AutomationResponseJob {
            writer: Arc::clone(&target.writer),
            response,
        };
        if self.response_sender.try_send(job).is_err() {
            target.cancel.cancel();
        }
    }

    pub(super) fn finish_automation_request(&mut self, client_id: u64, request_id: u64) {
        let mut empty = false;
        if let Some(requests) = self.automation_inflight.get_mut(&client_id) {
            requests.remove(&request_id);
            empty = requests.is_empty();
        }
        if empty {
            self.automation_inflight.remove(&client_id);
        }
    }

    /// Reject pane creation once the session holds [`MAX_SESSION_PANES`] live panes.
    pub(super) fn check_session_pane_cap(&self) -> Result<(), AutomationError> {
        if self.panes.len() >= MAX_SESSION_PANES {
            return Err(AutomationError::new(
                "limit_exceeded",
                format!("a session holds at most {MAX_SESSION_PANES} panes"),
            ));
        }
        Ok(())
    }
}

/// How many bytes a request would write to a PTY, when it writes any.
///
/// The length only. A recording that stored the bytes would be a credential dump wearing a
/// feature's clothes, and the shape of a session is reproducible without them.
fn automation_input_bytes(method: &AutomationMethod) -> Option<usize> {
    match method {
        AutomationMethod::Typing { text, .. }
        | AutomationMethod::Paste { text, .. }
        | AutomationMethod::SubmitLine { text, .. } => Some(text.len()),
        AutomationMethod::Key { key, repeat, .. } => Some(key.len() * usize::from(*repeat).max(1)),
        AutomationMethod::ShellCommand { command, .. } => Some(command.len()),
        AutomationMethod::Mouse { .. } => Some(0),
        _ => None,
    }
}

/// Build a text pattern from a literal or a bounded regular expression.
///
/// Shared by `wait text` and `wait output` so the two cannot come to disagree about how large a
/// regex may be or what an invalid one is called.
fn automation_text_pattern(
    pattern: String,
    regex: bool,
) -> Result<AutomationTextPattern, AutomationError> {
    if !regex {
        return Ok(AutomationTextPattern::Literal(pattern));
    }
    if pattern.len() > 8 * 1024 {
        return Err(AutomationError::new(
            "limit_exceeded",
            "regular expression exceeds 8 KiB",
        ));
    }
    regex::Regex::new(&pattern)
        .map(AutomationTextPattern::Regex)
        .map_err(|error| AutomationError::new("regex_invalid", error.to_string()))
}

fn automation_search(
    terminal: &Terminal,
    pattern: &SearchPattern,
    direction: SearchDirection,
    start_line: Option<isize>,
    start_column: Option<usize>,
    limit: usize,
) -> (Vec<serde_json::Value>, bool) {
    if direction == SearchDirection::Forward && start_line.is_none() {
        let (found, truncated) = find_all(terminal, pattern, limit);
        return (search_values(terminal, &found), truncated);
    }
    let first = -(terminal.history_len() as isize);
    let last = terminal.rows() as isize - 1;
    let start = start_line
        .unwrap_or(match direction {
            SearchDirection::Forward => first,
            SearchDirection::Backward => last,
        })
        .clamp(first, last);
    let start_column = start_column.unwrap_or(match direction {
        SearchDirection::Forward => 0,
        SearchDirection::Backward => terminal.cols(),
    });
    let mut found = Vec::new();

    let lines: Box<dyn Iterator<Item = isize>> = match direction {
        SearchDirection::Forward => Box::new(start..=last),
        SearchDirection::Backward => Box::new((first..=start).rev()),
    };
    for (scanned, line) in lines.enumerate() {
        if scanned == crate::search::MAX_SEARCH_SCAN_LINES {
            return (search_values(terminal, &found), true);
        }
        let mut line_matches = find_on_line(terminal, pattern, line);
        if direction == SearchDirection::Backward {
            line_matches.reverse();
        }
        for candidate in line_matches {
            if line == start
                && match direction {
                    SearchDirection::Forward => candidate.start_column < start_column,
                    SearchDirection::Backward => candidate.start_column > start_column,
                }
            {
                continue;
            }
            if found.len() == limit {
                return (search_values(terminal, &found), true);
            }
            found.push(candidate);
        }
    }
    (search_values(terminal, &found), false)
}
