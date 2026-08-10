//! Passive and explicitly reported AI-agent state for terminal panes.
//!
//! Process discovery and terminal rules are adapted from HerdR commit
//! 6c6ddcd49384d6ea9f0ee2e63bf7b2643dfd5bcf (Apache-2.0). See
//! `agent/PROVENANCE.md` for the source inventory and adaptation notes.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vvmux_terminal::Terminal;
use vvmux_terminal::pty::PtyControl;

use crate::layout::PaneId;

pub const MAX_REPORT_SOURCE_BYTES: usize = 128;
const IDLE_CONFIRMATIONS: u8 = 3;
const IDLE_CONFIRMATION_LIMIT: Duration = Duration::from_millis(700);
const IDLE_CONFIRMATION_RECHECK: Duration = Duration::from_millis(100);
const STARTUP_GRACE: Duration = Duration::from_secs(3);
const FULL_PROCESS_RECHECK: Duration = Duration::from_secs(5);
const MAX_PROCESS_ARGV_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
    Opencode,
    Hermes,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Hermes => "hermes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
}

impl AgentStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }

    pub fn urgency(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Done => 1,
            Self::Working => 2,
            Self::Idle => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    Screen,
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub kind: AgentKind,
    pub state: AgentState,
    pub status: AgentStatus,
    pub source: AgentSource,
}

#[derive(Debug, Clone)]
struct ReportedState {
    source: String,
    state: AgentState,
    sequence: u64,
}

#[derive(Debug)]
pub struct AgentRuntime {
    kind: Option<AgentKind>,
    process_group: Option<u32>,
    state: AgentState,
    source: AgentSource,
    done: bool,
    identified_at: Instant,
    pending_idle: Option<(Instant, u8)>,
    report: Option<ReportedState>,
    report_sequences: HashMap<String, u64>,
}

impl AgentRuntime {
    pub fn new() -> Self {
        Self {
            kind: None,
            process_group: None,
            state: AgentState::Idle,
            source: AgentSource::Screen,
            done: false,
            identified_at: Instant::now(),
            pending_idle: None,
            report: None,
            report_sequences: HashMap::new(),
        }
    }

    pub fn snapshot(&self) -> Option<AgentSnapshot> {
        let kind = self.kind?;
        let status = match self.state {
            AgentState::Idle if self.done => AgentStatus::Done,
            AgentState::Idle => AgentStatus::Idle,
            AgentState::Working => AgentStatus::Working,
            AgentState::Blocked => AgentStatus::Blocked,
        };
        Some(AgentSnapshot {
            kind,
            state: self.state,
            status,
            source: self.source,
        })
    }

    pub fn next_evaluation_delay(&self, now: Instant) -> Option<Duration> {
        self.kind?;
        if self.report.is_some() {
            return None;
        }
        let grace_end = self.identified_at + STARTUP_GRACE;
        if now < grace_end {
            return Some(grace_end.saturating_duration_since(now));
        }
        self.pending_idle
            .is_some()
            .then_some(IDLE_CONFIRMATION_RECHECK)
    }

    /// Returns true when stale OSC evidence must be discarded.
    pub fn observe_process(&mut self, group: Option<u32>, kind: Option<AgentKind>) -> bool {
        let changed_group = self.process_group != group;
        if changed_group {
            let startup_report_matches = self.process_group.is_none()
                && self.report.is_some()
                && kind.is_some()
                && kind == self.kind
                && self.identified_at.elapsed() < STARTUP_GRACE;
            if startup_report_matches {
                self.process_group = group;
                return false;
            }
            self.process_group = group;
            self.report = None;
            self.report_sequences.clear();
            self.pending_idle = None;
            self.done = false;
            self.kind = kind;
            self.state = AgentState::Idle;
            self.source = AgentSource::Screen;
            self.identified_at = Instant::now();
            return true;
        }
        if let Some(kind) = kind {
            if self.kind != Some(kind) {
                self.report = None;
                self.pending_idle = None;
                self.done = false;
                self.state = AgentState::Idle;
                self.source = AgentSource::Screen;
                self.identified_at = Instant::now();
            }
            self.kind = Some(kind);
        } else if self.kind.is_some() {
            self.report = None;
            self.report_sequences.clear();
            self.pending_idle = None;
            self.kind = None;
            self.done = false;
            self.state = AgentState::Idle;
            self.source = AgentSource::Screen;
        }
        false
    }

    pub fn report(
        &mut self,
        kind: AgentKind,
        state: AgentState,
        source: String,
        sequence: u64,
        visible: bool,
    ) -> Result<(), &'static str> {
        if source.is_empty() || source.len() > MAX_REPORT_SOURCE_BYTES {
            return Err("agent report source must contain 1..=128 bytes");
        }
        if self
            .report_sequences
            .get(&source)
            .is_some_and(|previous| sequence <= *previous)
        {
            return Err("agent report sequence is stale");
        }
        if self.kind.is_some_and(|detected| detected != kind) {
            return Err("reported agent does not match the pane foreground process");
        }
        self.kind = Some(kind);
        self.report_sequences.insert(source.clone(), sequence);
        self.report = Some(ReportedState {
            source,
            state,
            sequence,
        });
        self.pending_idle = None;
        self.commit(state, AgentSource::Report, visible);
        Ok(())
    }

    pub fn clear_report(&mut self, source: &str, sequence: u64) -> Result<(), &'static str> {
        if source.is_empty() || source.len() > MAX_REPORT_SOURCE_BYTES {
            return Err("agent report source must contain 1..=128 bytes");
        }
        if self
            .report_sequences
            .get(source)
            .is_some_and(|previous| sequence <= *previous)
        {
            return Err("agent report sequence is stale");
        }
        self.report_sequences.insert(source.to_owned(), sequence);
        if self
            .report
            .as_ref()
            .is_some_and(|report| report.source == source && sequence > report.sequence)
        {
            self.report = None;
            self.source = AgentSource::Screen;
        }
        Ok(())
    }

    pub fn evaluate_terminal(&mut self, terminal: &Terminal, visible: bool, now: Instant) {
        let Some(kind) = self.kind else { return };
        if let Some(report) = &self.report {
            let state = report.state;
            self.commit(state, AgentSource::Report, visible);
            return;
        }
        let rows = terminal.rows();
        let mut screen = terminal.latest_text(rows);
        if screen.len() > 64 * 1024 {
            let mut start = screen.len() - 64 * 1024;
            while !screen.is_char_boundary(start) {
                start += 1;
            }
            screen = screen[start..].to_owned();
        }
        let Some(candidate) = detect_candidate(
            kind,
            &screen,
            terminal.agent_osc_title(),
            terminal.agent_osc_progress(),
        ) else {
            self.pending_idle = None;
            return;
        };
        if now.saturating_duration_since(self.identified_at) < STARTUP_GRACE {
            self.commit(AgentState::Idle, AgentSource::Screen, visible);
            return;
        }
        if candidate.state == AgentState::Idle
            && !candidate.visible_idle
            && self.state == AgentState::Working
        {
            let (started, confirmations) = self.pending_idle.get_or_insert((now, 0));
            *confirmations = confirmations.saturating_add(1);
            if *confirmations < IDLE_CONFIRMATIONS
                && now.saturating_duration_since(*started) < IDLE_CONFIRMATION_LIMIT
            {
                return;
            }
        } else {
            self.pending_idle = None;
        }
        self.pending_idle = None;
        self.commit(candidate.state, AgentSource::Screen, visible);
    }

    pub fn mark_seen(&mut self) {
        if self.state == AgentState::Idle {
            self.done = false;
        }
    }

    fn commit(&mut self, state: AgentState, source: AgentSource, visible: bool) {
        if state != self.state {
            self.done = state == AgentState::Idle
                && matches!(self.state, AgentState::Working | AgentState::Blocked)
                && !visible;
            self.state = state;
        }
        self.source = source;
        if visible && state == AgentState::Idle {
            self.done = false;
        }
    }
}

#[derive(Debug, Deserialize)]
struct ManifestDefinition {
    rules: Vec<RuleDefinition>,
}

#[derive(Debug, Deserialize)]
struct RuleDefinition {
    #[allow(dead_code)]
    id: Option<String>,
    state: ManifestState,
    priority: u16,
    #[serde(default = "default_rule_region")]
    region: String,
    #[serde(default)]
    visible_idle: bool,
    #[serde(default)]
    skip_state_update: bool,
    #[serde(flatten)]
    gate: GateDefinition,
}

#[derive(Debug, Default, Deserialize)]
struct GateDefinition {
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
    #[serde(default)]
    all: Vec<GateDefinition>,
    #[serde(default)]
    any: Vec<GateDefinition>,
    #[serde(default, rename = "not")]
    not_gate: Vec<GateDefinition>,
}

#[derive(Debug)]
struct CompiledManifest {
    rules: Vec<CompiledRule>,
}

#[derive(Debug)]
struct CompiledRule {
    state: ManifestState,
    priority: u16,
    region: String,
    visible_idle: bool,
    skip_state_update: bool,
    gate: CompiledGate,
}

#[derive(Debug)]
struct CompiledGate {
    contains: Vec<String>,
    regex: Vec<regex::Regex>,
    line_regex: Vec<regex::Regex>,
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not_gate: Vec<CompiledGate>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManifestState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl ManifestState {
    fn agent_state(self) -> Option<AgentState> {
        match self {
            Self::Idle => Some(AgentState::Idle),
            Self::Working => Some(AgentState::Working),
            Self::Blocked => Some(AgentState::Blocked),
            Self::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DetectionCandidate {
    state: AgentState,
    visible_idle: bool,
}

impl CompiledManifest {
    fn compile(source: &str) -> Self {
        let definition: ManifestDefinition =
            toml::from_str(source).expect("embedded agent manifest must parse");
        let mut rules = definition
            .rules
            .into_iter()
            .map(|rule| CompiledRule {
                state: rule.state,
                priority: rule.priority,
                region: rule.region,
                visible_idle: rule.visible_idle,
                skip_state_update: rule.skip_state_update,
                gate: CompiledGate::compile(rule.gate),
            })
            .collect::<Vec<_>>();
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.priority));
        Self { rules }
    }

    fn detect(&self, screen: &str, title: &str, progress: &str) -> Option<DetectionCandidate> {
        for rule in &self.rules {
            let text = rule_region(screen, title, progress, &rule.region);
            if !rule.gate.matches(text) {
                continue;
            }
            if rule.skip_state_update {
                return None;
            }
            return rule.state.agent_state().map(|state| DetectionCandidate {
                state,
                visible_idle: rule.visible_idle,
            });
        }
        Some(DetectionCandidate {
            state: AgentState::Idle,
            visible_idle: false,
        })
    }
}

impl CompiledGate {
    fn compile(gate: GateDefinition) -> Self {
        Self {
            contains: gate
                .contains
                .into_iter()
                .map(|needle| needle.to_lowercase())
                .collect(),
            regex: compile_regexes(gate.regex),
            line_regex: compile_regexes(gate.line_regex),
            all: gate.all.into_iter().map(Self::compile).collect(),
            any: gate.any.into_iter().map(Self::compile).collect(),
            not_gate: gate.not_gate.into_iter().map(Self::compile).collect(),
        }
    }

    fn matches(&self, text: &str) -> bool {
        let lower = text.to_lowercase();
        self.matches_lower(text, &lower)
    }

    fn matches_lower(&self, text: &str, lower: &str) -> bool {
        self.contains.iter().all(|needle| lower.contains(needle))
            && self.regex.iter().all(|pattern| pattern.is_match(text))
            && self
                .line_regex
                .iter()
                .all(|pattern| text.lines().any(|line| pattern.is_match(line)))
            && self.all.iter().all(|gate| gate.matches_lower(text, lower))
            && (self.any.is_empty() || self.any.iter().any(|gate| gate.matches_lower(text, lower)))
            && !self
                .not_gate
                .iter()
                .any(|gate| gate.matches_lower(text, lower))
    }
}

fn compile_regexes(patterns: Vec<String>) -> Vec<regex::Regex> {
    patterns
        .into_iter()
        .map(|pattern| regex::Regex::new(&pattern).expect("embedded agent regex"))
        .collect()
}

fn default_rule_region() -> String {
    "whole_recent".into()
}

fn detect_candidate(
    kind: AgentKind,
    screen: &str,
    osc_title: &str,
    osc_progress: &str,
) -> Option<DetectionCandidate> {
    static CLAUDE: OnceLock<CompiledManifest> = OnceLock::new();
    static CODEX: OnceLock<CompiledManifest> = OnceLock::new();
    static OPENCODE: OnceLock<CompiledManifest> = OnceLock::new();
    static HERMES: OnceLock<CompiledManifest> = OnceLock::new();
    let manifest = match kind {
        AgentKind::Claude => CLAUDE
            .get_or_init(|| CompiledManifest::compile(include_str!("agent/manifests/claude.toml"))),
        AgentKind::Codex => CODEX
            .get_or_init(|| CompiledManifest::compile(include_str!("agent/manifests/codex.toml"))),
        AgentKind::Opencode => OPENCODE.get_or_init(|| {
            CompiledManifest::compile(include_str!("agent/manifests/opencode.toml"))
        }),
        AgentKind::Hermes => HERMES
            .get_or_init(|| CompiledManifest::compile(include_str!("agent/manifests/hermes.toml"))),
    };
    manifest.detect(screen, osc_title, osc_progress)
}

#[cfg(test)]
fn detect_state(kind: AgentKind, screen: &str, osc_title: &str, osc_progress: &str) -> AgentState {
    detect_candidate(kind, screen, osc_title, osc_progress)
        .map_or(AgentState::Idle, |candidate| candidate.state)
}

fn rule_region<'a>(screen: &'a str, title: &'a str, progress: &'a str, region: &str) -> &'a str {
    match region {
        "osc_title" => title,
        "osc_progress" => progress,
        "whole_recent" => screen,
        "after_last_prompt_marker" => after_last_prompt_marker(screen),
        "prompt_box_body" => prompt_box_body(screen).unwrap_or(""),
        "after_last_horizontal_rule" => after_last_horizontal_rule(screen),
        other => region_count(other, "bottom_non_empty_lines")
            .map_or("", |count| bottom_non_empty_lines(screen, count)),
    }
}

fn region_count(spec: &str, name: &str) -> Option<usize> {
    spec.strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')?
        .parse()
        .ok()
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines = content.lines().collect::<Vec<_>>();
    let Some(start) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    slice_from_line(content, &lines, start)
}

fn after_last_prompt_marker(content: &str) -> &str {
    let lines = content.lines().collect::<Vec<_>>();
    let Some(index) = lines
        .iter()
        .rposition(|line| *line == "›" || line.starts_with("› "))
    else {
        return content;
    };
    slice_from_line(content, &lines, index + 1)
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines = content.lines().collect::<Vec<_>>();
    let mut borders = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| is_horizontal_rule(line));
    let _bottom = borders.next()?;
    let (top, _) = borders.next()?;
    let start = line_offset(content, &lines, top + 1);
    let end_line = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map_or(lines.len(), |relative| top + 1 + relative);
    let end = line_offset(content, &lines, end_line);
    Some(&content[start..end])
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let lines = content.lines().collect::<Vec<_>>();
    let Some(index) = lines.iter().rposition(|line| is_horizontal_rule(line)) else {
        return content;
    };
    slice_from_line(content, &lines, index + 1)
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let rules = trimmed
        .chars()
        .take_while(|character| *character == '─')
        .count();
    if rules == 0 {
        return false;
    }
    let suffix_start = trimmed
        .char_indices()
        .nth(rules)
        .map_or(trimmed.len(), |(index, _)| index);
    trimmed[suffix_start..].trim_start().is_empty() || rules >= 3
}

fn slice_from_line<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    &content[line_offset(content, lines, index)..]
}

fn line_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

#[derive(Clone)]
pub struct ProbeTarget {
    pub pane_id: PaneId,
    pub child_pid: u32,
    pub control: PtyControl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessUpdate {
    pub pane_id: PaneId,
    pub process_group: Option<u32>,
    pub kind: Option<AgentKind>,
}

pub struct DetectorHandle {
    targets: mpsc::Sender<Vec<ProbeTarget>>,
}

impl DetectorHandle {
    pub fn replace_targets(&self, targets: Vec<ProbeTarget>) {
        let _ = self.targets.send(targets);
    }
}

pub fn start_detector(
    mut notify: impl FnMut(Vec<ProcessUpdate>) + Send + 'static,
) -> std::io::Result<DetectorHandle> {
    let (sender, receiver) = mpsc::channel::<Vec<ProbeTarget>>();
    std::thread::Builder::new()
        .name("vvmux-agent-detector".into())
        .spawn(move || {
            let mut targets = Vec::new();
            let mut cached = BTreeMap::<PaneId, (Option<u32>, Option<AgentKind>, Instant)>::new();
            loop {
                match receiver.recv_timeout(Duration::from_millis(400)) {
                    Ok(next) => targets = next,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                while let Ok(next) = receiver.try_recv() {
                    targets = next;
                }
                let mut updates = Vec::new();
                let live = targets
                    .iter()
                    .map(|target| target.pane_id)
                    .collect::<Vec<_>>();
                cached.retain(|pane, _| live.contains(pane));
                for target in &targets {
                    let group = target.control.foreground_process_group_id();
                    let previous = cached.get(&target.pane_id).copied();
                    let needs_full = previous.is_none_or(|(old_group, old_kind, checked)| {
                        old_group != group
                            || checked.elapsed()
                                >= if old_kind.is_some() {
                                    FULL_PROCESS_RECHECK
                                } else {
                                    Duration::from_millis(500)
                                }
                    });
                    if !needs_full {
                        continue;
                    }
                    let kind = identify_foreground_agent(target.child_pid, group);
                    let next = (group, kind, Instant::now());
                    if previous.is_none_or(|(old_group, old_kind, _)| {
                        old_group != group || old_kind != kind
                    }) {
                        updates.push(ProcessUpdate {
                            pane_id: target.pane_id,
                            process_group: group,
                            kind,
                        });
                    }
                    cached.insert(target.pane_id, next);
                }
                if !updates.is_empty() {
                    notify(updates);
                }
            }
        })?;
    Ok(DetectorHandle { targets: sender })
}

fn identify_foreground_agent(child_pid: u32, group: Option<u32>) -> Option<AgentKind> {
    foreground_processes(child_pid, group)
        .into_iter()
        .find_map(|process| identify_agent(&process))
}

#[derive(Debug)]
struct ProcessInfo {
    name: String,
    argv: Vec<String>,
}

fn identify_agent(process: &ProcessInfo) -> Option<AgentKind> {
    if let Some(kind) = agent_from_token(&process.name) {
        return Some(kind);
    }
    let runtime = normalized_program_name(&process.name);
    let token = match runtime.as_str() {
        "node" | "bun" => script_argument(&process.argv, &["-e", "--eval", "-p", "--print"]),
        runtime if runtime == "python" || runtime.starts_with("python3") => {
            python_script_argument(&process.argv)
        }
        "sh" | "bash" | "zsh" | "fish" => shell_command_argument(&process.argv),
        "cmd" => windows_command_argument(&process.argv),
        "powershell" | "pwsh" => powershell_command_argument(&process.argv),
        _ => process.argv.first().map(String::as_str),
    };
    token.and_then(agent_from_token)
}

fn normalized_program_name(value: &str) -> String {
    let mut name = path_basename(value).trim().to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1", ".js", ".mjs", ".cjs", ".py"] {
        if name.ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    name
}

fn path_basename(value: &str) -> &str {
    value
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(value)
}

fn agent_from_token(value: &str) -> Option<AgentKind> {
    let trimmed = value.trim_matches(|character| matches!(character, '\'' | '"'));
    let lowered = trimmed.to_ascii_lowercase();
    match normalized_program_name(trimmed).as_str() {
        "claude" | "claude-code" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        "opencode" | "opencode2" | "open-code" => Some(AgentKind::Opencode),
        "hermes" | "hermes-agent" | "hermes_agent" => Some(AgentKind::Hermes),
        _ if lowered.contains("@anthropic-ai/claude-code") => Some(AgentKind::Claude),
        _ if lowered.contains("@openai/codex") => Some(AgentKind::Codex),
        _ if lowered.contains("opencode-ai/bin/opencode") => Some(AgentKind::Opencode),
        _ => None,
    }
}

fn script_argument<'a>(argv: &'a [String], eval_flags: &[&str]) -> Option<&'a str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            return arguments.next().map(String::as_str);
        }
        if eval_flags.iter().any(|flag| {
            argument == flag
                || argument.strip_prefix(flag).is_some_and(|rest| {
                    (!flag.starts_with("--") && !rest.is_empty()) || rest.starts_with('=')
                })
        }) {
            return None;
        }
        if argument.starts_with('-') {
            if matches!(
                argument.as_str(),
                "-r" | "--require" | "--loader" | "--import" | "--experimental-loader"
            ) {
                let _ = arguments.next();
            }
            continue;
        }
        return Some(argument);
    }
    None
}

fn python_script_argument(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-c" => return None,
            "-m" => return arguments.next().map(String::as_str),
            "--" => return arguments.next().map(String::as_str),
            value if value.starts_with('-') => continue,
            _ => return Some(argument),
        }
    }
    None
}

fn shell_command_argument(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if argument
            .strip_prefix('-')
            .is_some_and(|flags| flags.contains('c'))
        {
            return arguments.next().and_then(|command| command_token(command));
        }
        if !argument.starts_with('-') {
            return Some(argument);
        }
    }
    None
}

fn windows_command_argument(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        if matches!(argument.to_ascii_lowercase().as_str(), "/c" | "/k") {
            return arguments.next().and_then(|command| command_token(command));
        }
    }
    None
}

fn powershell_command_argument(argv: &[String]) -> Option<&str> {
    let mut arguments = argv.iter().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.trim_matches('"').to_ascii_lowercase().as_str() {
            "-file" | "-f" | "/file" => return arguments.next().map(String::as_str),
            "-command" | "-c" | "/command" | "/c" => {
                return arguments.next().and_then(|command| command_token(command));
            }
            "-encodedcommand" | "-enc" | "/encodedcommand" | "/enc" => return None,
            "-configurationname" | "-executionpolicy" | "-outputformat" | "-psconsolefile"
            | "-version" | "-windowstyle" | "-workingdirectory" => {
                let _ = arguments.next();
            }
            value if value.starts_with('-') || value.starts_with('/') => continue,
            _ => return Some(argument),
        }
    }
    None
}

#[cfg(any(windows, test))]
fn split_process_command_line(line: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut argument = String::new();
    let mut quote = None;
    for character in line.chars() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(active), value) if value == active => quote = None,
            (None, value) if value.is_whitespace() => {
                if !argument.is_empty() {
                    arguments.push(std::mem::take(&mut argument));
                }
            }
            _ => argument.push(character),
        }
    }
    if !argument.is_empty() {
        arguments.push(argument);
    }
    arguments
}

fn command_token(mut command: &str) -> Option<&str> {
    loop {
        command = command.trim_start();
        let first = command.chars().next()?;
        let (token, rest) = if matches!(first, '\'' | '"') {
            let start = first.len_utf8();
            let end = command[start..]
                .find(first)
                .map_or(command.len(), |end| start + end);
            (
                &command[start..end],
                &command[end.saturating_add(first.len_utf8()).min(command.len())..],
            )
        } else {
            let end = command.find(char::is_whitespace).unwrap_or(command.len());
            (&command[..end], &command[end..])
        };
        if matches!(
            token.to_ascii_lowercase().as_str(),
            "&" | "." | "call" | "exec"
        ) {
            command = rest;
            continue;
        }
        return Some(token);
    }
}

#[cfg(target_os = "linux")]
fn foreground_processes(_child_pid: u32, group: Option<u32>) -> Vec<ProcessInfo> {
    use std::fs;
    use std::io::Read;
    let Some(group) = group else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for entry in entries.flatten().take(16_384) {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(tail) = stat.rsplit_once(')').map(|(_, tail)| tail) else {
            continue;
        };
        let fields = tail.split_whitespace().collect::<Vec<_>>();
        if fields.get(2).and_then(|value| value.parse::<u32>().ok()) != Some(group) {
            continue;
        }
        let name = fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let mut command_line = Vec::new();
        let _ = fs::File::open(format!("/proc/{pid}/cmdline")).and_then(|file| {
            file.take(MAX_PROCESS_ARGV_BYTES as u64)
                .read_to_end(&mut command_line)
        });
        let argv = command_line
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        processes.push(ProcessInfo { name, argv });
    }
    processes
}

#[cfg(target_os = "macos")]
fn foreground_processes(_child_pid: u32, group: Option<u32>) -> Vec<ProcessInfo> {
    const PROC_PGRP_ONLY: u32 = 2;
    let Some(group) = group else {
        return Vec::new();
    };
    let mut pids = vec![0 as libc::pid_t; 4096];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            group,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }
    let count = bytes as usize / std::mem::size_of::<libc::pid_t>();
    pids.into_iter()
        .take(count)
        .filter_map(|pid| u32::try_from(pid).ok())
        .filter_map(|pid| {
            macos_argv(pid).map(|argv| ProcessInfo {
                name: argv.first().cloned().unwrap_or_default(),
                argv,
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn macos_argv(pid: u32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size = 0;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size == 0
        || size > MAX_PROCESS_ARGV_BYTES
    {
        return None;
    }
    let mut bytes = vec![0_u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    bytes.truncate(size);
    if bytes.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(bytes[..4].try_into().ok()?);
    let rest = &bytes[4..];
    let executable_end = rest.iter().position(|byte| *byte == 0)?;
    let mut offset = executable_end;
    while rest.get(offset) == Some(&0) {
        offset += 1;
    }
    let mut argv = Vec::new();
    for part in rest[offset..]
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .take(argc.max(0) as usize)
    {
        argv.push(String::from_utf8_lossy(part).into_owned());
    }
    (!argv.is_empty()).then_some(argv)
}

#[cfg(windows)]
fn foreground_processes(child_pid: u32, _group: Option<u32>) -> Vec<ProcessInfo> {
    use std::collections::{HashMap, VecDeque};
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut rows = Vec::new();
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok && rows.len() < 16_384 {
        let length = entry
            .szExeFile
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(entry.szExeFile.len());
        rows.push((
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            String::from_utf16_lossy(&entry.szExeFile[..length]),
            windows_command_line(entry.th32ProcessID),
        ));
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    let mut children = HashMap::<u32, Vec<(u32, String)>>::new();
    let mut root = None;
    for (pid, parent, name, command_line) in rows {
        let argv = command_line
            .map(|line| split_process_command_line(&line))
            .filter(|arguments| !arguments.is_empty())
            .unwrap_or_else(|| vec![name.clone()]);
        if pid == child_pid {
            root = Some(ProcessInfo {
                name: name.clone(),
                argv: argv.clone(),
            });
        }
        children.entry(parent).or_default().push((pid, name, argv));
    }
    let mut queue = VecDeque::from([child_pid]);
    let mut output = root.into_iter().collect::<Vec<_>>();
    while let Some(parent) = queue.pop_front() {
        for (pid, name, argv) in children.remove(&parent).unwrap_or_default() {
            queue.push_back(pid);
            output.push(ProcessInfo { argv, name });
        }
    }
    output
}

#[cfg(windows)]
fn windows_command_line(pid: u32) -> Option<String> {
    use std::ffi::c_void;
    use std::mem::{MaybeUninit, size_of};
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows_sys::Win32::Foundation::{
        CloseHandle, HANDLE, NTSTATUS, STATUS_SUCCESS, UNICODE_STRING,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Peb {
        reserved1: [u8; 2],
        being_debugged: u8,
        reserved2: [u8; 1],
        reserved3: [*mut c_void; 2],
        ldr: *mut c_void,
        process_parameters: *mut ProcessParameters,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CurrentDirectory {
        path: UNICODE_STRING,
        handle: HANDLE,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcessParameters {
        maximum_length: u32,
        length: u32,
        flags: u32,
        debug_flags: u32,
        console_handle: HANDLE,
        console_flags: u32,
        standard_input: HANDLE,
        standard_output: HANDLE,
        standard_error: HANDLE,
        current_directory: CurrentDirectory,
        dll_path: UNICODE_STRING,
        image_path_name: UNICODE_STRING,
        command_line: UNICODE_STRING,
        environment: *mut c_void,
    }

    unsafe fn read_value<T: Copy>(process: HANDLE, address: *const c_void) -> Option<T> {
        if address.is_null() {
            return None;
        }
        let mut value = MaybeUninit::<T>::uninit();
        let mut read = 0;
        let ok = unsafe {
            ReadProcessMemory(
                process,
                address,
                value.as_mut_ptr().cast(),
                size_of::<T>(),
                &mut read,
            )
        } != 0;
        (ok && read == size_of::<T>()).then(|| unsafe { value.assume_init() })
    }

    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if process.is_null() {
        return None;
    }
    let result = (|| {
        let mut basic = MaybeUninit::<PROCESS_BASIC_INFORMATION>::uninit();
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessBasicInformation,
                basic.as_mut_ptr().cast(),
                size_of::<PROCESS_BASIC_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if status != STATUS_SUCCESS as NTSTATUS {
            return None;
        }
        let basic = unsafe { basic.assume_init() };
        let peb = unsafe { read_value::<Peb>(process, basic.PebBaseAddress.cast()) }?;
        let parameters =
            unsafe { read_value::<ProcessParameters>(process, peb.process_parameters.cast()) }?;
        let command = parameters.command_line;
        if command.Buffer.is_null() || command.Length == 0 || command.Length % 2 != 0 {
            return None;
        }
        let mut buffer = vec![0_u16; usize::from(command.Length / 2)];
        let mut read = 0;
        let ok = unsafe {
            ReadProcessMemory(
                process,
                command.Buffer.cast(),
                buffer.as_mut_ptr().cast(),
                usize::from(command.Length),
                &mut read,
            )
        } != 0;
        (ok && read == usize::from(command.Length)).then(|| String::from_utf16_lossy(&buffer))
    })();
    unsafe { CloseHandle(process) };
    result
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn foreground_processes(_child_pid: u32, _group: Option<u32>) -> Vec<ProcessInfo> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_wrapped_agents() {
        assert_eq!(
            identify_agent(&ProcessInfo {
                name: "node".into(),
                argv: vec!["node".into(), "/x/@anthropic-ai/claude-code/cli.js".into()]
            }),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            identify_agent(&ProcessInfo {
                name: "python".into(),
                argv: vec!["python".into(), "hermes_agent.py".into()]
            }),
            Some(AgentKind::Hermes)
        );
        assert_eq!(
            identify_agent(&ProcessInfo {
                name: "bash".into(),
                argv: vec!["bash".into(), "-lc".into(), "exec codex --full-auto".into()]
            }),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            identify_agent(&ProcessInfo {
                name: "powershell.exe".into(),
                argv: vec![
                    "powershell.exe".into(),
                    "-NoProfile".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-Command".into(),
                    "& 'C:\\tools\\opencode.cmd' --continue".into(),
                ]
            }),
            Some(AgentKind::Opencode)
        );
        assert_eq!(
            identify_agent(&ProcessInfo {
                name: "node".into(),
                argv: vec![
                    "node".into(),
                    "--eval".into(),
                    "setTimeout(() => {}, 1000)".into(),
                    "/tmp/codex".into(),
                ]
            }),
            None
        );
        assert_eq!(
            split_process_command_line("powershell.exe -Command \"& opencode --continue\""),
            ["powershell.exe", "-Command", "& opencode --continue"]
        );
    }

    #[test]
    fn codex_live_confirmation_is_blocked() {
        assert_eq!(
            detect_state(AgentKind::Codex, "Allow command? esc to interrupt", "", ""),
            AgentState::Blocked
        );
    }

    #[test]
    fn hidden_completion_becomes_done_until_seen() {
        let mut runtime = AgentRuntime::new();
        runtime.kind = Some(AgentKind::Codex);
        runtime.identified_at = Instant::now() - STARTUP_GRACE;
        runtime.commit(AgentState::Working, AgentSource::Screen, false);
        runtime.commit(AgentState::Idle, AgentSource::Screen, false);
        assert_eq!(runtime.snapshot().unwrap().status, AgentStatus::Done);
        runtime.mark_seen();
        assert_eq!(runtime.snapshot().unwrap().status, AgentStatus::Idle);
    }

    #[test]
    fn reports_are_ordered_and_scoped_per_runtime() {
        let mut first = AgentRuntime::new();
        let mut second = AgentRuntime::new();
        first
            .report(
                AgentKind::Opencode,
                AgentState::Working,
                "test".into(),
                2,
                false,
            )
            .unwrap();
        second
            .report(
                AgentKind::Opencode,
                AgentState::Idle,
                "test".into(),
                2,
                false,
            )
            .unwrap();
        assert!(
            first
                .report(
                    AgentKind::Opencode,
                    AgentState::Blocked,
                    "test".into(),
                    1,
                    false
                )
                .is_err()
        );
        first.clear_report("test", 3).unwrap();
        assert_eq!(first.snapshot().unwrap().source, AgentSource::Screen);
        assert_eq!(second.snapshot().unwrap().source, AgentSource::Report);
    }

    #[test]
    fn opencode_progress_and_hermes_prompts_are_detected() {
        assert_eq!(
            detect_state(AgentKind::Opencode, "■■■■", "", ""),
            AgentState::Working
        );
        assert_eq!(
            detect_state(
                AgentKind::Hermes,
                "Dangerous command — enter to confirm",
                "",
                ""
            ),
            AgentState::Blocked
        );
    }

    #[test]
    fn live_regions_do_not_reclassify_misleading_transcript_text() {
        assert_eq!(
            detect_state(
                AgentKind::Codex,
                "Allow command?\n› explain that old output",
                "",
                ""
            ),
            AgentState::Idle
        );
        assert!(
            detect_candidate(
                AgentKind::Claude,
                "showing detailed transcript\nctrl+o to toggle\n? for shortcuts",
                "",
                ""
            )
            .is_none()
        );
    }

    #[test]
    fn visible_prompt_box_idle_is_distinct_from_fallback_idle() {
        let visible =
            detect_candidate(AgentKind::Claude, "────────\n❯ \n────────", "", "").unwrap();
        assert_eq!(visible.state, AgentState::Idle);
        assert!(visible.visible_idle);
        let fallback = detect_candidate(AgentKind::Claude, "ordinary output", "", "").unwrap();
        assert_eq!(fallback.state, AgentState::Idle);
        assert!(!fallback.visible_idle);
    }

    #[test]
    fn foreground_replacement_clears_only_that_panes_report_authority() {
        let mut runtime = AgentRuntime::new();
        runtime
            .report(
                AgentKind::Opencode,
                AgentState::Working,
                "plugin".into(),
                1,
                false,
            )
            .unwrap();
        assert!(!runtime.observe_process(Some(10), Some(AgentKind::Opencode)));
        assert_eq!(runtime.snapshot().unwrap().source, AgentSource::Report);
        assert!(runtime.observe_process(Some(11), Some(AgentKind::Opencode)));
        assert_eq!(runtime.snapshot().unwrap().source, AgentSource::Screen);
        assert!(
            runtime
                .report(
                    AgentKind::Codex,
                    AgentState::Blocked,
                    "plugin".into(),
                    2,
                    false
                )
                .is_err()
        );

        runtime
            .report(
                AgentKind::Opencode,
                AgentState::Working,
                "plugin".into(),
                3,
                false,
            )
            .unwrap();
        assert!(!runtime.observe_process(Some(11), None));
        assert!(runtime.snapshot().is_none());
    }
}
