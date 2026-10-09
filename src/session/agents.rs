//! Agent prompts, reads, launches, probes, and agent-state tracking.

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
    pub(super) fn automation_action(
        &mut self,
        pane_id: PaneId,
        action: Action,
    ) -> Result<serde_json::Value, AutomationError> {
        if matches!(action, Action::CopyInput(_)) {
            return Err(AutomationError::new(
                "unsupported",
                "copy-mode input is not an automation action; use `vvmux msg key`",
            ));
        }
        self.automation_focus(pane_id)?;
        self.action(action);
        Ok(serde_json::json!({
            "pane_id": pane_id,
            "session_sequence": self.session_sequence,
        }))
    }

    pub(super) fn automation_input(
        &mut self,
        target: AutomationReplyTarget,
        pane_id: PaneId,
        bytes: Vec<u8>,
        report: bool,
    ) {
        if self.invalidate_mouse_selection_for_pane(pane_id) {
            self.schedule_render();
        }
        if bytes.len() > 1024 * 1024 {
            self.reply_automation_error(
                target,
                AutomationError::new("limit_exceeded", "PTY input exceeds 1 MiB"),
            );
            return;
        }
        if !self.register_pending_actor_work(&target) {
            self.reply_automation_error(
                target,
                AutomationError::new("limit_exceeded", "session pending-work quota is exhausted"),
            );
            return;
        }
        let receiver = match self
            .panes
            .get(&pane_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pane no longer exists"))
            .and_then(|pane| pane.input.send_with_completion(&bytes))
        {
            Ok(receiver) => receiver,
            Err(error) => {
                self.complete_pending_actor_work(&target);
                self.reply_automation_error(
                    target,
                    AutomationError::new("pty_closed", error.to_string()),
                );
                return;
            }
        };
        let byte_count = bytes.len();
        let sender = self.sender.clone();
        let completion_target = target.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("vvmux-automation-input-{pane_id}"))
            .spawn(move || {
                let result = match receiver.recv_timeout(PTY_WRITE_TIMEOUT) {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        Err("PTY write did not complete within five seconds".into())
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        Err("PTY writer closed before acknowledging input".into())
                    }
                };
                let _ = sender.send(ActorEvent::AutomationInputComplete {
                    reply: completion_target,
                    result,
                    pane_id,
                    byte_count,
                    report,
                });
            });
        if let Err(error) = spawn {
            self.complete_pending_actor_work(&target);
            self.reply_automation_error(
                target,
                AutomationError::new("unsupported", error.to_string()),
            );
        }
    }

    /// Park a request on a fresh scan of a pane's foreground job.
    ///
    /// Reading the process table means `/proc`, `sysctl`, or a process snapshot — bounded, but not
    /// bounded by anything this session controls, so it happens on a worker and returns as
    /// `ActorEvent::AgentProbeComplete`. The alternative, reusing the detector's cache, is wrong
    /// for the callers that need this: between two detector polls a shell can start a command, and
    /// deciding it is safe to type from a stale answer types into that command instead.
    pub(super) fn probe_agent_foreground(
        &mut self,
        target: AutomationReplyTarget,
        pane_id: PaneId,
        probe: AgentProbe,
    ) {
        let Some(pane) = self.panes.get(&pane_id) else {
            self.reply_automation_error(
                target,
                AutomationError::new("pane_not_found", "pane no longer exists"),
            );
            return;
        };
        let child_pid = pane.child_pid;
        let group = pane.control.foreground_process_group_id();
        if !self.register_pending_actor_work(&target) {
            self.reply_automation_error(
                target,
                AutomationError::new("limit_exceeded", "session pending-work quota is exhausted"),
            );
            return;
        }
        let catalog = Arc::clone(&self.agent_catalog);
        let sender = self.sender.clone();
        let completion_target = target.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("vvmux-agent-probe-{pane_id}"))
            .spawn(move || {
                let (identity, processes) =
                    crate::agent::foreground_job(&catalog, child_pid, group);
                let _ = sender.send(ActorEvent::AgentProbeComplete {
                    reply: completion_target,
                    pane_id,
                    probe,
                    job: ForegroundProbe {
                        identity,
                        processes,
                    },
                });
            });
        if let Err(error) = spawn {
            self.complete_pending_actor_work(&target);
            self.reply_automation_error(
                target,
                AutomationError::new("unsupported", error.to_string()),
            );
        }
    }

    /// Resume a request parked on `probe_agent_foreground`.
    ///
    /// The pane is re-resolved here rather than trusted from before the probe: it can close while
    /// the worker scans, and answering about a pane that no longer exists is worse than saying so.
    pub(super) fn resume_agent_probe(
        &mut self,
        reply: AutomationReplyTarget,
        pane_id: PaneId,
        probe: AgentProbe,
        job: ForegroundProbe,
    ) {
        let Some(pane) = self.panes.get(&pane_id) else {
            self.reply_automation_error(
                reply,
                AutomationError::new("pane_not_found", "pane no longer exists"),
            );
            return;
        };
        let child_pid = pane.child_pid;
        let group = pane.control.foreground_process_group_id();
        match probe {
            AgentProbe::ShellAvailable {
                agent,
                argv,
                timeout_ms,
            } => {
                let shell =
                    crate::agent_drive::available_pane_shell(child_pid, group, &job.processes);
                self.launch_agent_in_shell(reply, pane_id, agent, argv, timeout_ms, shell);
            }
            AgentProbe::HostsAgent { agent, prompt } => {
                let hosts = job
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.id == agent);
                match prompt {
                    Some(pending) => {
                        self.resolve_hosted_prompt(reply, pane_id, pending, agent, hosts);
                    }
                    None => {
                        self.resolve_hosts_agent_probe(reply, pane_id, agent, hosts);
                    }
                }
            }
        }
    }

    pub(super) fn resolve_hosted_prompt(
        &mut self,
        reply: AutomationReplyTarget,
        pane_id: PaneId,
        pending: PendingAgentPrompt,
        agent: crate::agent::AgentId,
        hosts: bool,
    ) {
        if !hosts {
            self.reply_automation_error(
                reply,
                AutomationError::new(
                    "agent_not_ready",
                    format!("{agent} is no longer the pane foreground process"),
                ),
            );
            return;
        }
        self.submit_agent_prompt(reply, pane_id, pending, agent);
    }

    /// Submit prompt text to a pane whose foreground host check just confirmed it still owns
    /// `agent`.
    pub(super) fn submit_agent_prompt(
        &mut self,
        reply: AutomationReplyTarget,
        pane_id: PaneId,
        mut pending: PendingAgentPrompt,
        agent: crate::agent::AgentId,
    ) {
        let Some(pane) = self.panes.get(&pane_id) else {
            self.reply_automation_error(
                reply,
                AutomationError::new("pane_not_found", "pane no longer exists"),
            );
            return;
        };
        let Some(snapshot) = pane.agent.snapshot() else {
            self.reply_automation_error(
                reply,
                AutomationError::new("agent_not_ready", format!("{agent} is no longer detected")),
            );
            return;
        };
        let mut bytes = sanitize_bracketed_paste(pending.text.as_bytes());
        if pane.terminal.modes().bracketed_paste {
            bytes.splice(0..0, b"\x1b[200~".iter().copied());
            bytes.extend_from_slice(b"\x1b[201~");
        }

        if !pending.wait {
            let enter = match encode_automation_key("Enter", &[], pane.terminal.modes()) {
                Ok(enter) => enter,
                Err(error) => {
                    self.reply_automation_error(reply, error);
                    return;
                }
            };
            bytes.extend_from_slice(&enter);
            if let Err(error) = pane.input.send(&bytes) {
                self.reply_automation_error(
                    reply,
                    AutomationError::new("agent_prompt_failed", error.to_string()),
                );
            } else {
                let status = snapshot.status;
                self.reply_automation(
                    reply,
                    serde_json::json!({
                        "pane_id": pane_id,
                        "agent": agent_json(snapshot),
                        "status": status,
                        "session_sequence": self.session_sequence,
                    }),
                );
            }
            return;
        }

        if let Err(error) = pane.input.send(&bytes) {
            self.reply_automation_error(
                reply,
                AutomationError::new("agent_prompt_failed", error.to_string()),
            );
            return;
        }

        let enter = match encode_automation_key("Enter", &[], pane.terminal.modes()) {
            Ok(enter) => enter,
            Err(error) => {
                self.reply_automation_error(reply, error);
                return;
            }
        };

        if !self.queue_delayed_input(
            pane_id,
            enter,
            crate::agent_drive::AGENT_PROMPT_SUBMIT_DELAY,
        ) {
            self.reply_automation_error(
                reply,
                AutomationError::new(
                    "agent_prompt_failed",
                    "agent prompt queued enter on a full input queue",
                ),
            );
            return;
        }

        let now = Instant::now();
        let timeout = deadline(pending.timeout_ms);
        let stall_window = std::cmp::min(
            crate::agent_drive::AGENT_PROMPT_EFFECT_TIMEOUT,
            timeout.saturating_duration_since(now),
        );
        let phase = if snapshot.status == AgentStatus::Working {
            AgentPromptPhase::Settle
        } else {
            AgentPromptPhase::Stall {
                baseline_seq: pending.baseline_seq,
                stall_deadline: now + stall_window,
            }
        };
        self.add_automation_waiter(AutomationWaiter {
            reply,
            pane_id: Some(pane_id),
            deadline: timeout,
            kind: AutomationWaitKind::AgentPrompt {
                phase,
                until: std::mem::take(&mut pending.until),
                baseline_status: pending.baseline_status,
            },
        });
    }

    pub(super) fn agent_prompt(
        &mut self,
        target: AutomationReplyTarget,
        pane_id: PaneId,
        text: String,
        wait: bool,
        until: Vec<crate::agent::AgentStatus>,
        timeout_ms: u64,
    ) {
        let until = if wait && until.is_empty() {
            vec![
                crate::agent::AgentStatus::Idle,
                crate::agent::AgentStatus::Blocked,
                crate::agent::AgentStatus::Done,
            ]
        } else {
            until
        };
        let Some(pane) = self.panes.get(&pane_id) else {
            self.reply_automation_error(
                target,
                AutomationError::new("pane_not_found", "pane no longer exists"),
            );
            return;
        };
        let Some(snapshot) = pane.agent.snapshot() else {
            self.reply_automation_error(
                target,
                AutomationError::new("agent_not_ready", "no agent is detected in this pane"),
            );
            return;
        };
        let agent = snapshot.kind.clone();
        self.probe_agent_foreground(
            target,
            pane_id,
            AgentProbe::HostsAgent {
                agent: agent.clone(),
                prompt: Some(PendingAgentPrompt {
                    text,
                    wait,
                    until,
                    timeout_ms,
                    baseline_seq: pane.agent_change_seq,
                    baseline_status: Some(snapshot.status),
                }),
            },
        );
    }

    pub(super) fn agent_send_keys(
        &mut self,
        target: AutomationReplyTarget,
        pane_id: PaneId,
        keys: Vec<String>,
    ) {
        let Some(pane) = self.panes.get(&pane_id) else {
            self.reply_automation_error(
                target,
                AutomationError::new("pane_not_found", "pane no longer exists"),
            );
            return;
        };
        if pane.agent.snapshot().is_none() {
            self.reply_automation_error(
                target,
                AutomationError::new(
                    "agent_not_ready",
                    format!("no agent is detected in pane {pane_id}"),
                ),
            );
            return;
        }
        let mut bytes = Vec::new();
        for key in keys {
            let (normalized, modifiers) = match normalize_agent_send_key(&key) {
                Ok(result) => result,
                Err(error) => {
                    self.reply_automation_error(target, error);
                    return;
                }
            };
            let encoded =
                match encode_automation_key(&normalized, &modifiers, pane.terminal.modes()) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        self.reply_automation_error(
                            target,
                            AutomationError::new("invalid_key", error.message),
                        );
                        return;
                    }
                };
            bytes.extend_from_slice(&encoded);
        }
        self.automation_input(target, pane_id, bytes, false);
    }

    pub(super) fn agent_read(
        &mut self,
        target: AutomationReplyTarget,
        pane_id: PaneId,
        lines: usize,
        json: bool,
    ) {
        let Some(pane) = self.panes.get(&pane_id) else {
            self.reply_automation_error(
                target,
                AutomationError::new("pane_not_found", "pane no longer exists"),
            );
            return;
        };
        let Some(snapshot) = pane.agent.snapshot() else {
            self.reply_automation_error(
                target,
                AutomationError::new(
                    "agent_not_detected",
                    "pane has no detected or reported agent",
                ),
            );
            return;
        };
        if snapshot.status != AgentStatus::Idle {
            self.reply_automation_error(
                target,
                AutomationError::new(
                    "agent_not_idle",
                    format!(
                        "cannot read {lines} lines while {} is {}: its alternate-screen history can only be captured by scrolling while idle. Wait and retry, or use get-text without --lines",
                        snapshot.kind,
                        snapshot.status.label(),
                    ),
                ),
            );
            return;
        }
        if !pane.terminal.alternate_screen() {
            self.reply_automation_error(
                target,
                AutomationError::new(
                    "not_alternate_screen",
                    "agent is not using the alternate screen",
                ),
            );
            return;
        }
        let modes = pane.terminal.modes();
        if !(modes.mouse_clicks || modes.mouse_motion) {
            self.reply_automation_error(
                target,
                AutomationError::new(
                    "mouse_reporting_disabled",
                    "agent has not enabled terminal mouse reporting",
                ),
            );
            return;
        }
        if self.alt_reads.contains_key(&pane_id) {
            self.reply_automation_error(
                target,
                AutomationError::new(
                    "alt_read_in_progress",
                    "an alternate-screen read is already in progress for this pane",
                ),
            );
            return;
        }
        if self.alt_reads.len() >= crate::alt_read::MAX_ALT_SCREEN_READS {
            self.reply_automation_error(
                target,
                AutomationError::new("busy", "alternate-screen read concurrency limit reached"),
            );
            return;
        }
        if lines <= pane.terminal.rows() {
            let text = crate::alt_read::visible_text(&pane.terminal, lines);
            self.reply_agent_read(target, text, lines, false, false, "visible", json);
            return;
        }
        self.alt_reads.insert(
            pane_id,
            PendingAgentRead {
                reply: target,
                read: crate::alt_read::PendingAltRead::start(&pane.terminal, lines, Instant::now()),
                lines,
                json,
            },
        );
    }

    pub(super) fn next_alt_read_deadline(&self) -> Duration {
        let now = Instant::now();
        self.alt_reads
            .values()
            .map(|pending| pending.read.next_deadline().saturating_duration_since(now))
            .min()
            .unwrap_or(IDLE_WAKE_INTERVAL)
    }

    pub(super) fn poll_alt_reads(&mut self) {
        let now = Instant::now();
        let cell_size = (self.last_display.cell_width, self.last_display.cell_height);
        let reads = std::mem::take(&mut self.alt_reads);
        let mut completed = Vec::new();
        for (pane_id, pending) in reads {
            let Some(pane) = self.panes.get(&pane_id) else {
                completed.push((
                    pending.reply,
                    Err(AutomationError::new(
                        "pane_not_found",
                        "pane no longer exists",
                    )),
                ));
                continue;
            };
            let idle = pane
                .agent
                .snapshot()
                .is_some_and(|snapshot| snapshot.status == AgentStatus::Idle);
            match pending
                .read
                .poll(&pane.terminal, &pane.input, idle, cell_size, now)
            {
                crate::alt_read::PollOutcome::Pending(read) => {
                    self.alt_reads
                        .insert(pane_id, PendingAgentRead { read, ..pending });
                }
                crate::alt_read::PollOutcome::Success(result) => completed.push((
                    pending.reply,
                    Ok((
                        result.text,
                        pending.lines,
                        result.truncated,
                        false,
                        "alternate_screen",
                        pending.json,
                    )),
                )),
                crate::alt_read::PollOutcome::Fallback => completed.push((
                    pending.reply,
                    Ok((
                        crate::alt_read::visible_text(&pane.terminal, pane.terminal.rows()),
                        pending.lines,
                        true,
                        true,
                        "visible",
                        pending.json,
                    )),
                )),
            }
        }
        for (reply, result) in completed {
            match result {
                Ok((text, lines, truncated, fallback, source, json)) => {
                    self.reply_agent_read(reply, text, lines, truncated, fallback, source, json);
                }
                Err(error) => self.reply_automation_error(reply, error),
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "each reply field is reported separately to the automation caller"
    )]
    pub(super) fn reply_agent_read(
        &mut self,
        target: AutomationReplyTarget,
        text: String,
        lines: usize,
        truncated: bool,
        fallback: bool,
        source: &'static str,
        json: bool,
    ) {
        let result = if json {
            serde_json::json!({
                "text": text,
                "lines": lines,
                "truncated": truncated,
                "fallback": fallback,
                "source": source,
            })
        } else {
            serde_json::Value::String(text)
        };
        self.reply_automation(target, result);
    }

    /// Validate and admit an agent launch, then park it on a fresh foreground probe.
    ///
    /// Everything cheap and certain is decided here; the one question that needs the process table
    /// — is this pane actually sitting at its own shell prompt — is answered off the actor, because
    /// a stale answer types a command line into whatever is really running.
    pub(super) fn agent_start(
        &mut self,
        target: AutomationReplyTarget,
        pane_id: PaneId,
        agent: crate::agent::AgentId,
        args: Vec<String>,
        timeout_ms: u64,
    ) {
        let Some(pane) = self.panes.get(&pane_id) else {
            self.reply_automation_error(
                target,
                AutomationError::new("pane_not_found", "pane no longer exists"),
            );
            return;
        };
        // A pane already hosting an agent is not available, and saying so now beats discovering it
        // from a probe: the answer does not depend on the process table.
        if let Some(existing) = pane.agent.snapshot() {
            self.reply_automation_error(
                target,
                AutomationError::new(
                    "agent_pane_busy",
                    format!("pane is already running {}", existing.label),
                ),
            );
            return;
        }
        if self.agent_catalog.identity(&agent).is_none() {
            self.reply_automation_error(
                target,
                AutomationError::new(
                    "invalid_agent_kind",
                    format!("agent `{agent}` is not enabled"),
                ),
            );
            return;
        }
        // Detection-only providers are refused rather than guessed at. The detection matchers
        // describe a *running* agent — wrapper scripts, package paths — which is not the command
        // that starts one.
        let Some(executable) = self.agent_catalog.launch_executable(&agent) else {
            self.reply_automation_error(
                target,
                AutomationError::new(
                    "agent_not_launchable",
                    format!("agent `{agent}` declares no launch command"),
                ),
            );
            return;
        };
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(executable.to_owned());
        argv.extend(args);
        self.probe_agent_foreground(
            target,
            pane_id,
            AgentProbe::ShellAvailable {
                agent,
                argv,
                timeout_ms,
            },
        );
    }

    /// Type an agent's command at a pane's shell, then wait for that pane to be running it.
    pub(super) fn launch_agent_in_shell(
        &mut self,
        reply: AutomationReplyTarget,
        pane_id: PaneId,
        agent: crate::agent::AgentId,
        argv: Vec<String>,
        timeout_ms: u64,
        shell: Option<String>,
    ) {
        let Some(shell) = shell else {
            self.reply_automation_error(
                reply,
                AutomationError::new(
                    "agent_pane_busy",
                    "pane is not an available shell: its foreground is running something else",
                ),
            );
            return;
        };
        if let Err(error) = self.type_agent_command(pane_id, &argv, &shell) {
            // Named rather than left to time out: a queue that refused the command is a different
            // problem from an agent that failed to start, and thirty seconds of waiting would
            // describe it as the second one.
            self.reply_automation_error(reply, error);
            return;
        }
        // The waiter is registered after the write but within the same actor turn, so no
        // transition can slip past unobserved — nothing else runs in between.
        self.add_automation_waiter(AutomationWaiter {
            reply,
            pane_id: Some(pane_id),
            deadline: deadline(timeout_ms),
            kind: AutomationWaitKind::AgentLaunch {
                agent,
                argv,
                ready_after: Instant::now() + crate::agent_drive::AGENT_START_SETTLE,
            },
        });
    }

    /// Type one agent's command line at a pane's shell, as a single write.
    ///
    /// Shared by `agent-start` and by session restore, which need the same quoting, the same
    /// bracketed-paste handling, and the same all-or-nothing write, and differ only in whether
    /// anyone is waiting for the result. A failure part way through would leave half an agent
    /// command sitting at the prompt for the user to find.
    pub(super) fn type_agent_command(
        &mut self,
        pane_id: PaneId,
        argv: &[String],
        shell: &str,
    ) -> Result<(), AutomationError> {
        let command = crate::agent_drive::shell_command_line(argv, shell)
            .ok_or_else(|| AutomationError::new("invalid_params", "agent launch has no command"))?;
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or_else(|| AutomationError::new("pane_not_found", "pane no longer exists"))?;
        let modes = pane.terminal.modes();
        let enter = encode_automation_key("Enter", &[], modes)?;
        let mut bytes = sanitize_bracketed_paste(command.as_bytes());
        if modes.bracketed_paste {
            bytes.splice(0..0, b"\x1b[200~".iter().copied());
            bytes.extend_from_slice(b"\x1b[201~");
        }
        bytes.extend_from_slice(&enter);
        pane.input
            .send(&bytes)
            .map_err(|error| AutomationError::new("agent_start_input_failed", error.to_string()))
    }

    /// Answer a still-hosting probe.
    ///
    /// The probe runs on a worker so the actor never reads the process table; this applies its
    /// answer. `agent-prompt` submits through here.
    pub(super) fn resolve_hosts_agent_probe(
        &mut self,
        reply: AutomationReplyTarget,
        pane_id: PaneId,
        agent: crate::agent::AgentId,
        hosts: bool,
    ) {
        if hosts {
            self.reply_automation(
                reply,
                serde_json::json!({ "pane_id": pane_id, "agent": agent }),
            );
        } else {
            self.reply_automation_error(
                reply,
                AutomationError::new(
                    "agent_not_ready",
                    format!("{agent} is no longer the pane foreground process"),
                ),
            );
        }
    }

    /// Admit work that may block outside the single-writer session actor.
    ///
    /// The actor performs ordered validation and admission, records a bounded completion key, and
    /// yields. A worker owns the blocking wait and can only return through a typed `ActorEvent`;
    /// mutation and reply emission resume on the actor after this key is released.
    pub(super) fn register_pending_actor_work(&mut self, target: &AutomationReplyTarget) -> bool {
        if self.pending_actor_work.len() >= MAX_PENDING_ACTOR_WORK {
            return false;
        }
        self.pending_actor_work
            .insert((target.client_id, target.request_id))
    }

    pub(super) fn complete_pending_actor_work(&mut self, target: &AutomationReplyTarget) {
        self.pending_actor_work
            .remove(&(target.client_id, target.request_id));
    }

    pub(super) fn refresh_agent_detector_targets(&self) {
        self.agent_detector.replace_targets(
            self.panes
                .values()
                .filter(|pane| pane.exit_status.is_none())
                .map(|pane| ProbeTarget {
                    pane_id: pane.id,
                    child_pid: pane.child_pid,
                    control: pane.control.clone(),
                })
                .collect(),
        );
    }

    pub(super) fn evaluate_agent_states(&mut self) {
        let visible = if self.any_client_focused() {
            let area = self.content_area();
            self.attached_projections(area)
                .into_iter()
                .map(|projection| projection.pane_id)
                .collect::<HashSet<_>>()
        } else {
            HashSet::new()
        };
        let now = Instant::now();
        let mut expired = false;
        for pane in self.panes.values_mut() {
            expired |= pane.agent.expire_metadata(now);
            pane.agent.evaluate_terminal(
                &self.agent_catalog,
                &pane.terminal,
                visible.contains(&pane.id),
                now,
            );
        }
        if expired {
            self.note_agent_display_change();
        }
        self.sync_agent_status();
    }

    /// Reconcile every pane's published agent snapshot with its live one.
    ///
    /// Agent state changes for six unrelated reasons: screen/OSC classification, an authoritative
    /// report, a report release, a foreground process change, catalog reconciliation, and the
    /// navigator acknowledging a finished agent. Each of those routes through here, so a
    /// transition is sequenced and observed exactly once no matter which one produced it, and a
    /// mutation that leaves the snapshot unchanged costs nothing.
    ///
    /// Consumers that react to a transition — lifecycle events, state waiters, notifications —
    /// belong in the loop below rather than at the six call sites.
    pub(super) fn sync_agent_status(&mut self) {
        self.adopt_pending_aliases();
        let mut transitions = Vec::new();
        for pane in self.panes.values_mut() {
            let current = pane.agent.snapshot();
            if pane.agent_published == current {
                continue;
            }
            let previous = std::mem::replace(&mut pane.agent_published, current.clone());
            pane.agent_change_seq = pane.agent_change_seq.saturating_add(1);
            transitions.push((pane.id, previous, current));
        }
        if transitions.is_empty() {
            return;
        }
        self.note_agent_display_change();
        for (pane_id, previous, current) in transitions {
            self.publish_plugin_event(
                "agent.status_changed",
                agent_status_changed_payload(pane_id, previous.as_ref(), current.as_ref()),
                Some(pane_id),
                None,
            );
            self.notify_agent_status(pane_id, current.as_ref());
        }
        // Resolve state waits here rather than leaving them to the next actor tick. A report
        // arrives as an event and the run loop checks waiters after handling one, but screen
        // classification runs in `evaluate_agent_states`, which the loop calls *after* that check
        // — so a screen-detected transition would otherwise sit unnoticed until the next wake.
        self.check_automation_waiters();
    }

    /// Ask the attached client to raise a desktop notification for a transition worth interrupting
    /// for.
    ///
    /// `done` is already derived only when the pane was not visible, so the default set never
    /// fires for an agent the user is watching. The per-pane floor keeps a flapping agent from
    /// spamming the desktop.
    pub(super) fn notify_agent_status(&mut self, pane_id: PaneId, current: Option<&AgentSnapshot>) {
        let settings = &self.config.notifications;
        let Some(agent) = current else {
            return;
        };
        let Some(kind) = notification_kind(agent.status, settings) else {
            return;
        };
        // One desktop notification, not one per attached terminal: the presenter's host, else
        // whoever is using the session.
        let Some(writer) = self
            .witness_client()
            .map(|client| Arc::clone(&client.writer))
        else {
            return;
        };
        let now = Instant::now();
        if !notification_allowed(
            self.last_notified.get(&pane_id).copied(),
            now,
            Duration::from_millis(settings.min_interval_ms),
        ) {
            return;
        }
        self.last_notified.insert(pane_id, now);
        let title = match kind {
            crate::ipc::NotifyKind::AgentBlocked => format!("{} needs you", agent.label),
            crate::ipc::NotifyKind::AgentDone => format!("{} finished", agent.label),
        };
        let body = agent
            .message
            .clone()
            .unwrap_or_else(|| format!("pane {pane_id}"));
        let _ = crate::ipc::send(
            &writer,
            &ServerMessage::Notify {
                kind,
                title: bounded_notification_text(&title),
                body: Some(bounded_notification_text(&body)),
            },
        );
    }

    /// Record an observable agent-related change: advance the session sequence and repaint.
    ///
    /// Display-only metadata calls this directly rather than travelling through
    /// [`Self::sync_agent_status`]. Metadata can churn on every tool call, and a progress counter
    /// must not become a stream of lifecycle transitions for waiters, events, and notifications to
    /// react to.
    pub(super) fn note_agent_display_change(&mut self) {
        self.session_sequence = self.session_sequence.wrapping_add(1);
        // Agent state is drawn only by the navigator popup; the status row does not carry it.
        if self.agent_navigator.is_some() {
            self.schedule_render();
        }
    }

    pub(super) fn next_agent_evaluation_delay(&self) -> Duration {
        let now = Instant::now();
        self.panes
            .values()
            .flat_map(|pane| {
                [
                    pane.agent.next_evaluation_delay(now),
                    // A token TTL has to wake the actor on its own: a pane whose agent needs no
                    // further classification would otherwise leave an expired token on screen
                    // until unrelated traffic happened to arrive.
                    pane.agent
                        .metadata()
                        .next_expiry()
                        .map(|expiry| expiry.saturating_duration_since(now)),
                ]
            })
            .flatten()
            .min()
            .unwrap_or(IDLE_WAKE_INTERVAL)
    }
}

/// The `agent.status_changed` payload.
///
/// Carries the lifecycle fact only. Display-only metadata is deliberately absent: it can change on
/// every tool call, and a subscriber reacting to lifecycle must not be woken by a progress
/// counter. A `null` status means the agent left the pane.
fn agent_status_changed_payload(
    pane_id: PaneId,
    previous: Option<&AgentSnapshot>,
    current: Option<&AgentSnapshot>,
) -> serde_json::Value {
    serde_json::json!({
        "pane_id": pane_id,
        "status": current.map(|agent| agent.status),
        "previous_status": previous.map(|agent| agent.status),
        "state": current.map(|agent| agent.state),
        "kind": current.map(|agent| &agent.kind),
        "label": current.map(|agent| &agent.label),
        "provider": current.map(|agent| &agent.provider),
        "source": current.map(|agent| agent.source),
        "message": current.and_then(|agent| agent.message.as_deref()),
        "session_present": current.is_some_and(|agent| agent.session_present),
    })
}

fn normalize_agent_send_key(key: &str) -> Result<(String, Vec<String>), AutomationError> {
    if key.is_empty() {
        return Err(AutomationError::new(
            "invalid_key",
            "agent-send-keys keys must be non-empty",
        ));
    }
    if key.chars().any(char::is_control) {
        return Err(AutomationError::new(
            "invalid_key",
            "agent-send-keys keys may not contain control characters",
        ));
    }
    match key.to_ascii_lowercase().as_str() {
        "c-c" | "c_c" | "ctrl+c" | "ctrl-c" => Ok(("c".to_owned(), vec!["Ctrl".to_owned()])),
        "+" => Ok(("plus".to_owned(), Vec::new())),
        _ => Ok((key.to_owned(), Vec::new())),
    }
}
