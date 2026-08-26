//! Bounded multi-step automation plans, run over one connection.
//!
//! A plan exists because a workflow is not one request. Discovering a pane, typing into it, waiting
//! for it, and checking what happened is four processes and four connections when each is its own
//! `msg` invocation — and nothing carries a result from one step into the next, so a caller has to
//! parse and re-pass every ID by hand.
//!
//! The schema is deliberately identical to Vivido's `run-plan` (`vivido/src/cli.rs`), so an agent
//! that learned it there does not relearn it here. What differs is verification: vvmux has no GPU
//! frame, so a step verifies against a pane's screen sequence or the attached client's render
//! acknowledgement rather than against a presented frame.
//!
//! There are no loops, no conditionals beyond a single equality test, and no arithmetic. A plan is
//! a bounded list of steps, which is what makes it safe to accept from an agent and possible to
//! preflight without running it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ipc::{AutomationMethod, AutomationRequest, METHOD_CAPABILITIES};

/// The only plan version this release accepts.
pub const PLAN_VERSION: u16 = 1;
const MAX_PLAN_STEPS: usize = 256;
const MAX_PLAN_NAME_BYTES: usize = 64;
const MAX_PLAN_BYTES: usize = 1024 * 1024;
const MAX_VERIFY_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u16,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    /// Names this step in the output stream and in any error about it.
    pub id: String,
    /// A wire method name, as `capabilities` advertises it.
    pub method: String,
    #[serde(default)]
    pub params: Value,
    /// Alias name to JSON Pointer, read out of this step's result for later steps to reference.
    #[serde(default)]
    pub bind: BTreeMap<String, String>,
    /// Run this step only when a previously bound alias equals this value.
    #[serde(default)]
    pub when: Option<PlanCondition>,
    #[serde(default)]
    pub on_error: PlanErrorPolicy,
    #[serde(default)]
    pub verify: Option<PlanVerification>,
    /// Targeting and preconditions, exactly as the equivalent `msg` flags express them.
    #[serde(default)]
    pub pane_id: Option<Value>,
    #[serde(default)]
    pub pane_name: Option<Value>,
    #[serde(default)]
    pub alias: Option<Value>,
    #[serde(default)]
    pub expect: Option<PlanExpect>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanExpect {
    #[serde(default)]
    pub screen_sequence: Option<Value>,
    #[serde(default)]
    pub session_sequence: Option<Value>,
    #[serde(default)]
    pub layout_sequence: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCondition {
    /// A bound alias name.
    pub reference: String,
    pub equals: Value,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanErrorPolicy {
    #[default]
    Abort,
    Continue,
}

/// What a step must be able to observe before it counts as done.
///
/// Vivido verifies against a presented GPU frame. vvmux has none, so the two things it can prove
/// are that the pane's screen changed and that the attached client acknowledged a render —
/// deliberately different assertions, and neither is GPU presentation. Pair `rendered` with
/// Vivido's own `wait frame` when that is what matters.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanVerification {
    /// Which pane to watch. Defaults to the pane the step itself addressed.
    #[serde(default)]
    pub pane_id: Option<Value>,
    /// Require a newer pane screen sequence than the one read before the step ran.
    #[serde(default = "default_true")]
    pub screen_changed: bool,
    /// Require the attached client to acknowledge a render newer than the one before the step.
    #[serde(default)]
    pub rendered: bool,
    /// Capture the pane's text once the wait resolves.
    #[serde(default)]
    pub capture: bool,
    #[serde(default = "default_verify_timeout")]
    pub timeout_ms: u64,
}

fn default_true() -> bool {
    true
}

fn default_verify_timeout() -> u64 {
    30_000
}

/// How a plan run reports itself.
pub struct PlanOptions {
    pub dry_run: bool,
    /// Run only the steps that observe, skipping every mutation.
    pub preflight: bool,
}

pub fn run(target: &str, file: Option<&std::path::Path>, options: PlanOptions) -> io::Result<()> {
    let plan = read_plan(file)?;
    let (mut reader, writer) = crate::server::connect(target)?;
    let mut request_id = 0_u64;
    let mut next_id = || {
        request_id += 1;
        request_id
    };

    let capabilities = {
        let id = next_id();
        let request = plain_request(id, AutomationMethod::Capabilities);
        crate::automation::send_request(&writer, request)?;
        crate::automation::response_result(crate::automation::receive_response(&mut reader, id)?)?
    };
    validate(&plan, &capabilities)?;

    let mode = if options.dry_run {
        "dry_run"
    } else if options.preflight {
        "preflight"
    } else {
        "execute"
    };
    let mut stdout = io::stdout().lock();
    emit(
        &mut stdout,
        &serde_json::json!({
            "type": "plan_started",
            "version": plan.version,
            "steps": plan.steps.len(),
            "mode": mode,
        }),
    )?;

    let mut aliases: BTreeMap<String, Value> = BTreeMap::new();
    let mut failures = 0_usize;
    for step in &plan.steps {
        let mutating = method_is_mutating(&step.method, &capabilities);
        if options.dry_run {
            emit(
                &mut stdout,
                &serde_json::json!({
                    "type": "step",
                    "id": step.id,
                    "method": step.method,
                    "mutating": mutating,
                    "status": "planned",
                }),
            )?;
            continue;
        }
        if options.preflight && mutating {
            emit(
                &mut stdout,
                &step_skipped(step, mutating, "preflight_mutation"),
            )?;
            continue;
        }
        // A step whose inputs were bound by a step preflight skipped cannot run either. Reported
        // as its own reason rather than as a failure: nothing is wrong, the dependency was skipped.
        if let Some(missing) = missing_reference(step, &aliases) {
            emit(
                &mut stdout,
                &step_skipped(step, mutating, &format!("unresolved_reference:{missing}")),
            )?;
            continue;
        }
        if let Some(when) = &step.when {
            let actual = aliases.get(&when.reference).cloned().unwrap_or(Value::Null);
            if actual != when.equals {
                emit(
                    &mut stdout,
                    &step_skipped(step, mutating, "condition_false"),
                )?;
                continue;
            }
        }

        match execute_step(&mut reader, &writer, &mut next_id, step, &aliases) {
            Ok(result) => {
                if let Err(error) = bind_aliases(step, &result, &mut aliases) {
                    failures += 1;
                    emit(&mut stdout, &step_error(step, mutating, &error))?;
                    if step.on_error == PlanErrorPolicy::Abort {
                        return finish(&mut stdout, failures, true);
                    }
                    continue;
                }
                emit(
                    &mut stdout,
                    &serde_json::json!({
                        "type": "step",
                        "id": step.id,
                        "method": step.method,
                        "mutating": mutating,
                        "status": "ok",
                        "result": result,
                    }),
                )?;
            }
            Err(error) => {
                failures += 1;
                emit(&mut stdout, &step_error(step, mutating, &error.to_string()))?;
                if step.on_error == PlanErrorPolicy::Abort {
                    return finish(&mut stdout, failures, true);
                }
            }
        }
    }
    finish(&mut stdout, failures, false)
}

fn finish(stdout: &mut impl Write, failures: usize, aborted: bool) -> io::Result<()> {
    let status = match (failures, aborted) {
        (0, _) => "ok",
        (_, true) => "failed",
        (_, false) => "completed_with_errors",
    };
    emit(
        stdout,
        &serde_json::json!({
            "type": "plan_completed",
            "status": status,
            "failures": failures,
        }),
    )?;
    if failures == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!("{failures} plan step(s) failed")))
    }
}

fn emit(stdout: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, value).map_err(io::Error::other)?;
    writeln!(stdout)?;
    // A plan runner is usually read by something waiting on it, and a block-buffered stdout would
    // hold every step until the run ended.
    stdout.flush()
}

fn step_skipped(step: &PlanStep, mutating: bool, reason: &str) -> Value {
    serde_json::json!({
        "type": "step",
        "id": step.id,
        "method": step.method,
        "mutating": mutating,
        "status": "skipped",
        "reason": reason,
    })
}

fn step_error(step: &PlanStep, mutating: bool, error: &str) -> Value {
    serde_json::json!({
        "type": "step",
        "id": step.id,
        "method": step.method,
        "mutating": mutating,
        "status": "error",
        "error": error,
    })
}

/// Read and parse a plan, from a file or from stdin.
fn read_plan(file: Option<&std::path::Path>) -> io::Result<Plan> {
    let source = match file {
        Some(path) if path != std::path::Path::new("-") => {
            let metadata = std::fs::metadata(path)?;
            if metadata.len() > MAX_PLAN_BYTES as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("a plan holds at most {MAX_PLAN_BYTES} bytes"),
                ));
            }
            std::fs::read_to_string(path)?
        }
        _ => {
            use std::io::Read as _;
            let mut source = String::new();
            // Bounded even from a pipe, where there is no size to check first.
            io::stdin()
                .lock()
                .take(MAX_PLAN_BYTES as u64 + 1)
                .read_to_string(&mut source)?;
            if source.len() > MAX_PLAN_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("a plan holds at most {MAX_PLAN_BYTES} bytes"),
                ));
            }
            source
        }
    };
    serde_json::from_str(&source).map_err(|error| {
        io::Error::new(io::ErrorKind::InvalidData, format!("invalid plan: {error}"))
    })
}

/// Reject a plan whole, before any of it runs.
///
/// Everything checkable without side effects is checked here: a plan that would fail on its last
/// step because of a typo should not first perform the mutations in the steps before it.
fn validate(plan: &Plan, capabilities: &Value) -> io::Result<()> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
    if plan.version != PLAN_VERSION {
        return Err(invalid(format!(
            "plan version {} is not supported; this release runs version {PLAN_VERSION}",
            plan.version
        )));
    }
    if plan.steps.is_empty() || plan.steps.len() > MAX_PLAN_STEPS {
        return Err(invalid(format!(
            "a plan takes 1 through {MAX_PLAN_STEPS} steps"
        )));
    }
    let advertised = capabilities["methods"]
        .as_array()
        .map(|methods| {
            methods
                .iter()
                .filter_map(|method| method.as_str())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();

    let mut ids = BTreeSet::new();
    // Accumulated in order, which is what makes a forward reference detectable: an alias is only
    // usable by steps after the one that bound it.
    let mut bound = BTreeSet::new();
    for step in &plan.steps {
        if !valid_plan_name(&step.id) {
            return Err(invalid(format!(
                "step id `{}` must be 1..={MAX_PLAN_NAME_BYTES} bytes of letters, digits, '-' or '_'",
                step.id
            )));
        }
        if !ids.insert(step.id.as_str()) {
            return Err(invalid(format!("duplicate step id `{}`", step.id)));
        }
        if step.method == "capabilities" && step.bind.is_empty() {
            // Allowed, just never useful; no need to reject it.
        }
        if !advertised.is_empty() && !advertised.contains(step.method.as_str()) {
            return Err(invalid(format!(
                "step `{}` names `{}`, which this session does not serve",
                step.id, step.method
            )));
        }
        if !step.params.is_null() && !step.params.is_object() {
            return Err(invalid(format!(
                "step `{}` params must be a JSON object",
                step.id
            )));
        }
        if let Some(verify) = &step.verify
            && !(1..=MAX_VERIFY_TIMEOUT_MS).contains(&verify.timeout_ms)
        {
            return Err(invalid(format!(
                "step `{}` verification timeout must be 1..={MAX_VERIFY_TIMEOUT_MS} ms",
                step.id
            )));
        }
        // Every reference this step makes must already be bound. Forward references are rejected
        // rather than deferred: a plan is a list, not a graph, and a step cannot use a value that
        // has not been produced.
        let mut references = BTreeSet::new();
        collect_references(&step.params, &mut references);
        for value in [&step.pane_id, &step.pane_name, &step.alias]
            .into_iter()
            .flatten()
        {
            collect_references(value, &mut references);
        }
        if let Some(expect) = &step.expect {
            for value in [
                &expect.screen_sequence,
                &expect.session_sequence,
                &expect.layout_sequence,
            ]
            .into_iter()
            .flatten()
            {
                collect_references(value, &mut references);
            }
        }
        if let Some(verify) = &step.verify
            && let Some(pane_id) = &verify.pane_id
        {
            collect_references(pane_id, &mut references);
        }
        if let Some(when) = &step.when {
            references.insert(when.reference.clone());
        }
        for reference in &references {
            if !bound.contains(reference.as_str()) {
                return Err(invalid(format!(
                    "step `{}` references `{reference}`, which no earlier step binds",
                    step.id
                )));
            }
        }
        for (alias, pointer) in &step.bind {
            if !valid_plan_name(alias) {
                return Err(invalid(format!(
                    "step `{}` binds an invalid alias `{alias}`",
                    step.id
                )));
            }
            if !bound.insert(alias.clone()) {
                return Err(invalid(format!("alias `{alias}` is bound more than once")));
            }
            if !pointer.is_empty() && !pointer.starts_with('/') {
                return Err(invalid(format!(
                    "step `{}` binds `{alias}` to `{pointer}`, which is not a JSON Pointer",
                    step.id
                )));
            }
        }
    }
    Ok(())
}

fn valid_plan_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PLAN_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Every `{"$ref": "alias"}` in a value.
fn collect_references(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = single_reference(object) {
                into.insert(reference.to_owned());
                return;
            }
            for nested in object.values() {
                collect_references(nested, into);
            }
        }
        Value::Array(array) => {
            for nested in array {
                collect_references(nested, into);
            }
        }
        _ => {}
    }
}

/// An object that is exactly `{"$ref": "name"}` and nothing else.
fn single_reference(object: &serde_json::Map<String, Value>) -> Option<&str> {
    (object.len() == 1)
        .then(|| object.get("$ref"))
        .flatten()
        .and_then(Value::as_str)
}

fn resolve_references(value: &Value, aliases: &BTreeMap<String, Value>) -> Value {
    match value {
        Value::Object(object) => {
            if let Some(reference) = single_reference(object) {
                return aliases.get(reference).cloned().unwrap_or(Value::Null);
            }
            Value::Object(
                object
                    .iter()
                    .map(|(key, nested)| (key.clone(), resolve_references(nested, aliases)))
                    .collect(),
            )
        }
        Value::Array(array) => Value::Array(
            array
                .iter()
                .map(|nested| resolve_references(nested, aliases))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The first reference this step makes that is not bound, if any.
fn missing_reference(step: &PlanStep, aliases: &BTreeMap<String, Value>) -> Option<String> {
    let mut references = BTreeSet::new();
    collect_references(&step.params, &mut references);
    for value in [&step.pane_id, &step.pane_name, &step.alias]
        .into_iter()
        .flatten()
    {
        collect_references(value, &mut references);
    }
    references
        .into_iter()
        .find(|reference| !aliases.contains_key(reference))
}

fn bind_aliases(
    step: &PlanStep,
    result: &Value,
    aliases: &mut BTreeMap<String, Value>,
) -> Result<(), String> {
    for (alias, pointer) in &step.bind {
        // Bound from the method's own reply, not from the verification wrapper around it: a caller
        // binding `/pane_id` means the pane the method returned.
        let action = result.get("action").unwrap_or(result);
        let value = if pointer.is_empty() {
            action.clone()
        } else {
            action.pointer(pointer).cloned().ok_or_else(|| {
                format!("`{pointer}` is not present in step `{}`'s result", step.id)
            })?
        };
        aliases.insert(alias.clone(), value);
    }
    Ok(())
}

fn method_is_mutating(method: &str, capabilities: &Value) -> bool {
    capabilities["method_capabilities"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["name"] == method)
                .and_then(|entry| entry["mutating"].as_bool())
        })
        // Falls back to this build's own table, then to "assume it mutates". An unknown method is
        // never treated as safe: a preflight that ran something because it could not classify it
        // would be worse than one that skipped a harmless read.
        .unwrap_or_else(|| {
            METHOD_CAPABILITIES
                .iter()
                .find(|capability| capability.name == method)
                .is_none_or(|capability| capability.mutating)
        })
}

fn plain_request(id: u64, method: AutomationMethod) -> AutomationRequest {
    AutomationRequest {
        id,
        pane_id: None,
        agent: None,
        pane_name: None,
        lease: None,
        allow_focused: false,
        expect: None,
        idempotency_key: None,
        method,
    }
}

/// Run one step, with its optional verification, and return what it produced.
///
/// Verification is part of the same step rather than a following one on purpose: the "before"
/// sequence has to be read before the action, and a caller that had to do that itself would be
/// back to the race the plan exists to close.
fn execute_step(
    reader: &mut crate::ipc::RecordReader,
    writer: &crate::ipc::SharedWriter,
    next_id: &mut impl FnMut() -> u64,
    step: &PlanStep,
    aliases: &BTreeMap<String, Value>,
) -> io::Result<Value> {
    let params = resolve_references(&step.params, aliases);
    let pane_id = step
        .pane_id
        .as_ref()
        .map(|value| resolve_references(value, aliases))
        .and_then(|value| value.as_u64());
    let pane_name = step
        .pane_name
        .as_ref()
        .map(|value| resolve_references(value, aliases))
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .map(crate::layout::PaneName::new)
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let alias = step
        .alias
        .as_ref()
        .map(|value| resolve_references(value, aliases))
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .map(crate::agent::AgentAlias::new)
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;

    let expect = step.expect.as_ref().map(|expect| {
        let read = |slot: &Option<Value>| {
            slot.as_ref()
                .map(|value| resolve_references(value, aliases))
                .and_then(|value| value.as_u64())
        };
        crate::ipc::ExpectedState {
            screen_sequence: read(&expect.screen_sequence),
            session_sequence: read(&expect.session_sequence),
            layout_sequence: read(&expect.layout_sequence),
        }
    });

    // The pane verification watches, resolved before the action so a "before" reading is possible.
    // A step that targets by name still needs a number here, because the sequences it compares are
    // per pane; the name is resolved once rather than being made the caller's problem.
    let verify_pane = match step.verify.as_ref() {
        None => None,
        Some(verify) => {
            let named = verify
                .pane_id
                .as_ref()
                .map(|value| resolve_references(value, aliases))
                .and_then(|value| value.as_u64())
                .or(pane_id);
            match (named, pane_name.as_ref()) {
                (Some(pane), _) => Some(pane),
                (None, Some(name)) => Some(resolve_named_pane(reader, writer, next_id, name)?),
                (None, None) => None,
            }
        }
    };
    let before = match (&step.verify, verify_pane) {
        (Some(_), Some(pane)) => Some(read_sequences(reader, writer, next_id, pane)?),
        (Some(_), None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "step `{}` verifies but names no pane, and its method addressed none",
                    step.id
                ),
            ));
        }
        (None, _) => None,
    };

    let method = build_method(&step.method, params)?;
    let id = next_id();
    let request = AutomationRequest {
        id,
        pane_id,
        agent: alias,
        pane_name,
        lease: None,
        // A plan is explicit about its targets. Falling back to whatever happens to be focused
        // would make the same plan act on different panes on different runs.
        allow_focused: false,
        expect,
        idempotency_key: step.idempotency_key.clone(),
        method,
    };
    crate::automation::send_request(writer, request)?;
    let action =
        crate::automation::response_result(crate::automation::receive_response(reader, id)?)?;

    let (Some(verify), Some(pane), Some(before)) = (&step.verify, verify_pane, before) else {
        return Ok(action);
    };
    let mut verification = serde_json::Map::new();
    if verify.screen_changed {
        let id = next_id();
        crate::automation::send_request(
            writer,
            AutomationRequest {
                id,
                pane_id: Some(pane),
                method: AutomationMethod::WaitScreenChange {
                    after_screen: Some(before.screen),
                    timeout_ms: verify.timeout_ms,
                },
                ..plain_request(id, AutomationMethod::Capabilities)
            },
        )?;
        verification.insert(
            "screen".into(),
            crate::automation::response_result(crate::automation::receive_response(reader, id)?)?,
        );
    }
    if verify.rendered {
        let id = next_id();
        crate::automation::send_request(
            writer,
            plain_request(
                id,
                AutomationMethod::WaitRendered {
                    after_session: before.session,
                    timeout_ms: verify.timeout_ms,
                },
            ),
        )?;
        verification.insert(
            "rendered".into(),
            crate::automation::response_result(crate::automation::receive_response(reader, id)?)?,
        );
    }
    if verify.capture {
        let id = next_id();
        crate::automation::send_request(
            writer,
            AutomationRequest {
                id,
                pane_id: Some(pane),
                method: AutomationMethod::GetText {
                    rows: None,
                    source: crate::ipc::TextSource::Visible,
                },
                ..plain_request(id, AutomationMethod::Capabilities)
            },
        )?;
        verification.insert(
            "capture".into(),
            crate::automation::response_result(crate::automation::receive_response(reader, id)?)?,
        );
    }
    Ok(serde_json::json!({
        "action": action,
        "verification": Value::Object(verification),
    }))
}

/// The pane a name currently refers to.
fn resolve_named_pane(
    reader: &mut crate::ipc::RecordReader,
    writer: &crate::ipc::SharedWriter,
    next_id: &mut impl FnMut() -> u64,
    name: &crate::layout::PaneName,
) -> io::Result<u64> {
    let id = next_id();
    crate::automation::send_request(
        writer,
        AutomationRequest {
            id,
            pane_name: Some(name.clone()),
            method: AutomationMethod::ResolvePane {
                tab: None,
                path: Vec::new(),
            },
            ..plain_request(id, AutomationMethod::Capabilities)
        },
    )?;
    let resolved =
        crate::automation::response_result(crate::automation::receive_response(reader, id)?)?;
    resolved["target"]["pane_id"].as_u64().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("`{name}` did not resolve to a pane"),
        )
    })
}

/// The sequences a verification compares against, read before the step acts.
struct Sequences {
    screen: u64,
    session: u64,
}

fn read_sequences(
    reader: &mut crate::ipc::RecordReader,
    writer: &crate::ipc::SharedWriter,
    next_id: &mut impl FnMut() -> u64,
    pane_id: u64,
) -> io::Result<Sequences> {
    let id = next_id();
    crate::automation::send_request(
        writer,
        AutomationRequest {
            id,
            pane_id: Some(pane_id),
            ..plain_request(id, AutomationMethod::Inspect)
        },
    )?;
    let inspected =
        crate::automation::response_result(crate::automation::receive_response(reader, id)?)?;
    Ok(Sequences {
        screen: inspected["pane"]["screen_sequence"].as_u64().unwrap_or(0),
        session: inspected["session_sequence"].as_u64().unwrap_or(0),
    })
}

/// Turn a wire method name and its parameters into the typed method.
///
/// Round-tripped through the same tagged representation the wire uses, so a plan cannot construct
/// a method shape the protocol would not accept, and an unknown field is refused here rather than
/// silently ignored.
fn build_method(name: &str, params: Value) -> io::Result<AutomationMethod> {
    let mut object = match params {
        Value::Object(object) => object,
        Value::Null => serde_json::Map::new(),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "step params must be a JSON object",
            ));
        }
    };
    object.insert("method".into(), Value::String(name.to_owned()));
    serde_json::from_value(Value::Object(object)).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{name}` cannot be built from these params: {error}"),
        )
    })
}
