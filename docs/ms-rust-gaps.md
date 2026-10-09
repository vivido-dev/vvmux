# vvmux Microsoft Rust guideline gaps

Audit date: **2026-10-08**. Guideline baseline: the repository's
[ms-rust skill](../../.agents/skills/ms-rust/SKILL.md), compliance date **2026-10-07**.

This is an audit baseline; the [remediation record](#remediation-record-2026-10-08) at the end
tracks what was fixed against it afterwards. At audit time no remediation had been applied. vvmux
is not declared guideline compliant, and no compliance markers were added.

## Scope and method

- Reviewed the working tree of vvmux 0.5.3 at vvmux commit
  `1bc5506c7d6a07a74beb7962b1afc9d005e853d5`. The uncommitted `Cargo.toml`/`Cargo.lock` edits
  (relaxed `vivid_*` version requirements) were preserved; the lockfile was verified byte-identical
  after all checks.
- Inventoried 103 tracked Rust files, totaling 99,062 lines, including tests, examples, and fixtures.
  Used repository-wide searches, compiler/Clippy/rustdoc measurements, and focused source review.
  This is not a line-by-line soundness certification of every file.
- Crates in scope, each classified for which guidance applies:

  | Crate | Location | Role | Guidance applied |
  | --- | --- | --- | --- |
  | `vvmux` (bin) | `src/` | Application: client, daemon, gateway | Application + universal |
  | `vvmux_terminal` (lib of the `vvmux` package) | `vvmux-terminal/src/` | Library used only by the vvmux binary in this repo, but published with the package | Library rules for its public surface; M-APP-ERROR relaxation for errors |
  | `vvmux-plugin-api` 0.5.5 | `vvmux-plugin-api/` | Published third-party plugin contract | Full library guidance |
  | `vvmux-plugin-sdk` 0.1.0 | `vvmux-plugin-sdk/` | Published third-party plugin SDK | Full library guidance |
  | Component guest fixtures | `tests/fixtures/component_guest/`, `examples/plugins/rust-component/` | Standalone wasm32 `cdylib` crates with their own empty `[workspace]`, built by integration tests | Treated as test/example fixtures |

- Read the root [AGENTS.md](../../AGENTS.md), [vvmux AGENTS.md](../AGENTS.md), and
  [ARCHITECTURE.md](../ARCHITECTURE.md). Read guideline files 01–15. Files 06 and 10 contain headings
  only; substantive library and unsafe rules come from files 12–15 and 03. No C ABI or Rust DLL is
  exported, so M-FFI-NAMING, M-FFI-TRANSLATES, and M-ISOLATE-DLL-STATE are not applicable; the
  plugin boundary is a JSON frame protocol or a WIT component world, not a Rust ABI. vvmux defines
  no macros, so the macro guidance applies only to its use of `wit_bindgen::generate!`.
- P1 means a safety fix to prioritize; P2 means a material contract, resilience, or verification
  gap; P3 means lower-risk maintenance work or a recommendation requiring design/performance
  judgment. A policy deviation is not, by itself, proof of a runtime defect.

## Findings at a glance

| ID | Priority | Gap | Guideline IDs |
| --- | --- | --- | --- |
| G01 | P1 | Windows pipe listener frees an in-flight `OVERLAPPED` | M-UNSOUND, M-UNSAFE |
| G02 | P2 | PTY `pre_exec` hook allocates after `fork` | M-UNSAFE |
| G03 | P2 | Unsafe code carries almost no safety reasoning and no Miri | M-UNSAFE, M-STATIC-VERIFICATION |
| G04 | P2 | No check-in verification gates; Windows code is never compiled in CI | M-STATIC-VERIFICATION |
| G05 | P2 | Locked `rustls` has an open security advisory | M-STATIC-VERIFICATION |
| G06 | P2 | Declared MSRVs do not build | M-MSRV |
| G07 | P2 | `--no-default-features` breaks the test build and warns in the binary | M-FEATURES-ADDITIVE, M-STATIC-VERIFICATION |
| G08 | P2 | Plugin libraries lack docs, examples, and nameable public types | M-CANONICAL-DOCS, M-MODULE-DOCS, M-DESIGN-FOR-AI |
| G09 | P2 | Plugin library errors leak foreign types and drop cause chains | M-ERRORS-CANONICAL-STRUCTS, M-PUBLIC-DISPLAY, M-DONT-LEAK-TYPES, M-FROM-ERROR |
| G10 | P2 | Plugin SDK surface is a glob re-export and cannot be tested by plugin authors | M-NO-GLOB-REEXPORTS, M-IMPL-IO, M-MOCKABLE-SYSCALLS, M-PARAMETER-CONSISTENCY |
| G11 | P2 | Daemon diagnostics go to a stderr that is `/dev/null` | M-LOG-NOT-PRINT |
| G12 | P3 | Logging is unstructured and interpolates user paths | M-LOG-STRUCTURED |
| G13 | P3 | Session automation dispatch relies on 43 unchecked `unwrap()`s | M-PANIC-ON-BUG, M-PANIC-MESSAGE |
| G14 | P3 | No lint configuration; overrides are unreasoned `allow`s | M-LINT-OVERRIDE-EXPECT, M-STATIC-VERIFICATION |
| G15 | P3 | Workspace layout and shared metadata are not unified | M-CARGO-WORKSPACE, M-CRATES-IN-WORKSPACE, M-CRATES-FLAT-FOLDER, M-SMALLER-CRATES |
| G16 | P3 | Public library types lack Debug | M-PUBLIC-DEBUG |
| G17 | P3 | Stale re-export shims mask dead aliases | M-FOREIGN-REEXPORTS, M-SINGLE-ITEM-PATH |
| G18 | P3 | Operational constants lack selection rationale | M-DOCUMENTED-MAGIC |
| G19 | P3 | Default allocator and no hot-path benchmarks | M-MIMALLOC-APPS, M-HOTPATH |
| G20 | P3 | Very large modules | M-BALANCED-MODULES, M-SMALLER-CRATES |

## Safety

### G01 — `PendingPipe::drop` can free memory the kernel still owns

**Evidence:** [src/platform/windows.rs](../src/platform/windows.rs), lines 924–967; the correct
pattern in the same file at lines 1066–1069.

`PendingPipe` owns an `OVERLAPPED` used by an overlapped `ConnectNamedPipe`. When a listener is
dropped while a connect is pending (`connecting == true`), `Drop` calls `CancelIoEx` and returns.
`CancelIoEx` only *requests* cancellation: the I/O manager completes the request afterwards and
writes its status into the `OVERLAPPED`. Immediately after `drop`, the `Box<PendingPipe>` and the
event handle are freed, so a late completion writes into freed heap memory. Windows requires
waiting for the cancelled operation (for example `GetOverlappedResult(..., TRUE)`) before freeing
or reusing the `OVERLAPPED`; the pipe-read path at lines 1066–1069 already does exactly that.

`unsafe impl Send for PendingPipe` (line 931) has no justification. It is sound only because the
pending connect is always boxed before `poll_connect` runs; that address-stability invariant is
implicit.

**Close:** in `Drop`, when `connecting` is set, wait for the cancelled operation to complete before
the fields drop. Document the boxing/address-stability invariant at the type, and the `Send`
reasoning. Add a Windows regression that drops a listener with a pending connect, ideally under
Application Verifier or a page-heap run. This follows from the source and the Win32 contract; no
corruption was reproduced, and Windows was not available to this audit.

### G02 — The PTY child hook allocates between `fork` and `exec`

**Evidence:** [vvmux-terminal/src/pty/unix.rs](../vvmux-terminal/src/pty/unix.rs), lines 270–285.

The `pre_exec` closure builds its error with `io::Error::other(format!(...))`, which allocates. The
closure runs in the forked child of a multithreaded process; `CommandExt::pre_exec` requires callers
to avoid operations, such as allocation, that may take locks held by other parent threads at fork
time. On the failure path, the child can deadlock inside the allocator instead of reporting
`setsid`/`TIOCSCTTY` failure. The other three `pre_exec` hooks
([src/platform/unix.rs](../src/platform/unix.rs) lines 47 and 254, [src/plugin.rs](../src/plugin.rs)
line 3607) correctly return `io::Error::last_os_error()` and use only async-signal-safe calls.

**Close:** return `io::Error::last_os_error()` directly, as the other hooks do, and add a comment
stating the async-signal-safety contract at the `unsafe` block.

### G03 — Unsafe code is not reviewable as written

**Evidence:** 264 `unsafe` blocks/functions/impls in production code, plus 2 in tests (platform Windows
code 119, Windows ConPTY 46, Unix platform 39, agent process inspection 17, Unix PTY 14,
runtime 14, plugin 9, others 6). One `SAFETY` comment exists in the whole tree.
`clippy::undocumented_unsafe_blocks` reports 68 sites on macOS alone; Windows sites were not linted.

- Ten `unsafe impl Send`/`Sync` declarations carry no reasoning:
  [vvmux-terminal/src/pty/windows.rs](../vvmux-terminal/src/pty/windows.rs) lines 92–93 and
  426–427; [src/platform/windows.rs](../src/platform/windows.rs) lines 931, 1165–1166, and
  1200–1201; [src/plugin.rs](../src/plugin.rs) line 3669. M-UNSAFE forbids ad-hoc `Send`
  implementations except as part of a properly reasoned abstraction.
- [src/agent.rs](../src/agent.rs) line 2236 declares `unsafe fn read_value<T: Copy>` without a
  `# Safety` section. It reads another process's bytes into `MaybeUninit<T>` and calls
  `assume_init`; `Copy` does not guarantee that every bit pattern is a valid `T`, so the real
  precondition is unstated.
- The Unix PTY wrappers call `libc::kill(-group, ...)` from `Drop`, `terminate`, and a detached
  thread after a 250 ms sleep. That is not memory unsafety, but the group-ID reuse window after
  the child is reaped should be part of the documented invariant.

No Miri job, sanitizer job, or `unsafe_op_in_unsafe_fn`/`undocumented_unsafe_blocks` lint exists.

**Close:** document each unsafe block and every `Send`/`Sync` impl, give `read_value` a
`# Safety` section or restrict `T` to plain-old-data, and enable `undocumented_unsafe_blocks`
with a ratchet. Most unsafe is FFI or platform calls that Miri cannot execute; run Miri on the pure
Rust parts of `vvmux_terminal` and rely on native integration tests for the platform layer.

## Verification and dependencies

### G04 — No check-in gates exist for vvmux

**Evidence:** [.github/workflows/unix-release.yml](../.github/workflows/unix-release.yml) is the
only vvmux workflow; it runs `cargo build --release` on tag push or manual dispatch for four Unix
targets. The root repository's workflows cover Vivido, vvdesk, and vvweb, but not vvmux.

Nothing runs `cargo fmt`, `cargo test`, Clippy, rustdoc, cargo-audit, cargo-hack, or Miri on push
or pull request. The Windows implementation (`src/platform/windows.rs`, 2,036 lines;
`vvmux-terminal/src/pty/windows.rs`, 800 lines; and the `windows_*` integration tests) is not
compiled by any workflow. The `windows/*.ps1` soak and multi-user scripts are manual.

**Close:** add a push/PR workflow that runs the three AGENTS.md commands on Linux, macOS, and
Windows, plus `cargo doc` with `-D warnings`, `cargo test --doc`, `cargo audit`,
`cargo hack check --feature-powerset --all-targets`, and Miri for the pure Rust library parts.
Build the two wasm32 guest crates explicitly. Treat a configured gate as unverified until it has
passed on its host.

### G05 — `rustls` 0.23.43 is affected by RUSTSEC-2026-0285

**Evidence:** `cargo audit` reports RUSTSEC-2026-0285 ("TLS 1.3 handshake messages incorrectly
accepted across encryption level boundaries", severity 5.3) for `rustls` 0.23.43. The same version
is in the committed lockfile. The fix is `>= 0.23.45`. vvmux uses rustls for the gateway tunnel,
`tokio-tungstenite` WebSocket clients, and the `ureq` update fetcher.

**Close:** update `rustls` within 0.23 and keep the lockfile change with it; add `cargo audit` to
CI (G04).

### G06 — Neither declared MSRV builds

**Evidence:** `cargo +1.95.0 check --workspace --all-targets --locked` fails: the locked
`wasmtime` 49.0.2 and `cranelift-*` 0.136.2 require rustc 1.96. The comment above `wasmtime` in
[Cargo.toml](../Cargo.toml) still states that wasmtime 49 requires 1.95.
`cargo +1.88.0 check -p vvmux-plugin-sdk` fails because its dependency `vvmux-plugin-api` requires
1.95; both guest fixture crates also declare 1.88 while depending on the SDK.

**Close:** set each `rust-version` to what the locked graph actually needs (or pin dependencies
to honor the declared value), make the SDK's MSRV at least its API dependency's, and check the
MSRV with a pinned toolchain in CI.

### G07 — Feature combinations are not verified

**Evidence:** `cargo hack check --workspace --feature-powerset --all-targets` fails on the first
configuration, `--no-default-features`:

- The integration-test binary does not compile. `tests/integration/common/mod.rs` and
  `tests/integration/tunnel_connect.rs` import `axum`, `tokio`, and `futures_util`, which are
  optional dependencies enabled only by `server-capability`; 69 errors result.
  [tests/integration/main.rs](../tests/integration/main.rs) states that each module keeps the
  feature gate it needs, but these two do not.
- The binary compiles with two warnings that `-D warnings` would reject: an unused re-export at
  [src/bridge.rs](../src/bridge.rs) line 3 and the unused `BridgeWorker::spawn_with_sender` at
  [src/client.rs](../src/client.rs) line 1021.

Library, binary, and example targets compile in all three configurations; the remaining two
library crates have no features.

**Close:** gate those test modules (or give the test target `required-features`), remove or gate
the dead items, and run cargo-hack in CI.

## Library contracts

### G08 — Plugin libraries are under-documented and partly unnameable

**Evidence:** `cargo rustdoc -- -W missing_docs` reports **240** missing items in
`vvmux-plugin-api`, **1** in `vvmux-plugin-sdk`, and **115** in `vvmux_terminal`. All three
libraries have **zero** doctests, and no public function has an `# Errors` section
(Clippy `missing_errors_doc`: 21).

- [vvmux-plugin-api/src/protocol.rs](../vvmux-plugin-api/src/protocol.rs) documents almost none of
  its public message types, fields, or `read_frame`/`write_frame` failure modes. It is the wire
  contract third-party plugins implement.
- `Agent::launch` is `Option<AgentLaunch>`, but `AgentLaunch`
  ([manifest.rs](../vvmux-plugin-api/src/manifest.rs) line 447) is not re-exported from the crate
  root, so downstream code cannot name it to construct an `Agent`. Several public bound constants,
  such as `MAX_STARTUP_TIMEOUT_MS` and `MAX_MANIFEST_BYTES`, are likewise unreachable.
- `cargo doc` reports two unresolved links: `vvmux_plugin_api::MAX_STARTUP_TIMEOUT_MS` (a
  consequence of the previous point) and `:port`.
- Public plugin-contract structs have public fields, are not `#[non_exhaustive]`, and use
  `deny_unknown_fields`. Adding a field is therefore a breaking change for both Rust callers and
  older peers. That may be intended, but it is undocumented.

**Close:** document the protocol and manifest contracts with summaries, `# Errors`, and runnable
examples (a framed hello/invoke round trip and a manifest load). Export every type reachable from
public fields. Fix the two links, gate rustdoc warnings and `missing_docs` in CI, and state the
compatibility policy for adding fields.

### G09 — Plugin error types need canonical contracts

**Evidence:** [manifest.rs](../vvmux-plugin-api/src/manifest.rs) lines 36–70 and
[protocol.rs](../vvmux-plugin-api/src/protocol.rs) `FrameError`.

- `ManifestError` and `FrameError` are public enums whose variants expose `toml::de::Error` and
  `serde_json::Error` (M-DONT-LEAK-TYPES). They implement `std::error::Error` with an empty body,
  so `source()` returns `None` and the wrapped cause is lost. They capture no backtrace.
- `Manifest::validate_input`/`validate_output` and `validate_schema_instance` return
  `Result<(), Vec<String>>`, which is not an error type.
- The SDK returns `Result<_, PluginError>` from its public functions, but `PluginError` implements
  neither `Display` nor `std::error::Error` (M-PUBLIC-DISPLAY), so `?` cannot convert it into
  `Box<dyn Error>`.
- The SDK's `serve*` functions return `Box<dyn std::error::Error>`, which is neither `Send` nor
  `Sync`; `read_frame`/`write_frame` repeat `.map_err(FrameError::Io)` and `FrameError::Json`
  instead of `From` conversions (M-FROM-ERROR).

**Close:** introduce situation-specific error structs, such as `ManifestError` and `FrameError`,
with private kinds, `is_*` predicates, forwarded `source()`, and captured backtraces. Implement
`Display` and `Error` for the wire `PluginError` without changing its serialized shape. Use a
typed SDK error.

### G10 — The plugin SDK surface needs deliberate exports and testability

**Evidence:** [vvmux-plugin-sdk/src/lib.rs](../vvmux-plugin-sdk/src/lib.rs).

- `pub use vvmux_plugin_api::*;` glob re-exports a sibling crate, making the SDK a de facto
  prelude (M-NO-GLOB-REEXPORTS, M-NO-PRELUDE). On wasm32, `vvmux_plugin_sdk::PluginError` (native
  wire struct) and `vvmux_plugin_sdk::component::PluginError` (WIT type) are two different types
  with the same name.
- `serve_with_host_and_events` hard-wires the process's stdin and stdout. `NativeHost` has private
  fields and no constructor. Plugin authors therefore cannot unit-test a handler that takes
  `&mut NativeHost`, or drive the serve loop over in-memory streams, without a real process
  (M-IMPL-IO, M-MOCKABLE-SYSCALLS, and the testability point of M-DESIGN-FOR-AI). The crate's
  own test constructs `NativeHost` privately to do exactly that.
- `serve_with_events` and `serve_with_host_and_events` each take two closures; the guideline
  limits functions to one closure, placed last (M-PARAMETER-CONSISTENCY).

**Close:** export the API items explicitly, or document the SDK as an umbrella and say which items
come from where. Add a `serve_on(reader: impl Read, writer: impl Write, ...)` form with the stdio
version forwarding to it, plus a `test-util` host double. Group action and event handlers into one
handler value (for example, a small trait with a default `event` method).

## Operations and diagnostics

### G11 — Daemon errors are printed to a closed stderr

**Evidence:** the detached server is spawned with `stderr(Stdio::null())`
([src/platform/unix.rs](../src/platform/unix.rs) lines 249–251;
[src/server.rs](../src/server.rs) lines 221–223), and [src/logging.rs](../src/logging.rs) says
panics "would otherwise vanish with its stderr". Server and session code still reports failures
with `eprintln!`: for example, session snapshot write failure
([src/session.rs](../src/session.rs) lines 2601 and 13220), pane-history discard failure
([src/server.rs](../src/server.rs) lines 99, 114, and 276), and further sites in `session.rs`.
These diagnostics are lost even when `--log-file` is enabled.

**Close:** route daemon-side failures through `log` at an appropriate level. Reserve `eprintln!`
for foreground CLI output. Add a regression that checks a session-snapshot write failure in the
configured log file.

### G12 — Logs are free-form and include user paths

**Evidence:** the 28 `log` macro calls format messages eagerly with interpolated values, have no
event names or named properties, and include user-identifying paths: for example,
`server starting: session={name} config={config_path:?} layout={layout_path:?}`
([src/server.rs](../src/server.rs) line 42). [src/logging.rs](../src/logging.rs) renders each record
as a single text line. No secrets were found in log calls; the gap is structure and path redaction.

**Close:** adopt named events with structured fields and redact or hash home-relative paths. Keep
volume low on render and IPC paths. This is P3 because logging volume is small and opt-in.

## Correctness and maintenance

### G13 — Automation dispatch depends on an unchecked pairing

**Evidence:** [src/session.rs](../src/session.rs) resolves `pane_id: Option<_>` when
`method_needs_pane(&request.method)` is true (around line 3866). It then calls
`pane_id.unwrap()` in 43 match arms, the first at line 4106. Correctness depends on
`method_needs_pane` and the match arms staying in sync. A mismatch panics the session actor, which
is the sole mutator of every tab and pane, so one automation request would end the whole session.
[src/automation.rs](../src/automation.rs) lines 1116, 1217, and 1228 use message-less
`unreachable!()`.

**Close:** make the pairing correct by construction. For example, have each method variant that
needs a pane resolve to a typed target before dispatch, or let `method_needs_pane` return the
resolved pane alongside the method. Give remaining invariant panics messages. No instance of the
mismatch was found; this is about blast radius, not a demonstrated bug.

### G14 — Lint policy is implicit

**Evidence:** no `[lints]` or `[workspace.lints]` table exists in any vvmux manifest. The source
contains 39 `#[allow]` attributes (17 `dead_code`, 13 `clippy::too_many_arguments`,
4 `unused_imports`, and others), none with a `reason`, and no `#[expect]`. Running the guideline's
recommended compiler and Clippy set over the workspace on macOS yields **1,158 warnings**. Leading
categories: `cast_possible_truncation` 120, `assertions_on_result_states` 114 (mostly tests),
`too_many_lines` 92, `clone_on_ref_ptr` 86, `undocumented_unsafe_blocks` 68,
`cast_possible_wrap` 62, `map_err_ignore` 51, and `allow_attributes_without_reason` 38. By location:
`src` 915, tests 98, `vvmux_terminal` 90, examples 16, plugin API 30, and SDK 9.

**Close:** add a workspace lint table with the guideline set and documented opt-outs, ratchet the
existing debt, and convert item-level `allow`s to `#[expect(..., reason = "...")]`. Platform-specific
exceptions need `cfg_attr` so they do not become unfulfilled expectations on other targets.

### G15 — Workspace structure duplicates configuration

**Evidence:** [Cargo.toml](../Cargo.toml) and the member manifests.

- `vvmux_terminal` is not its own package. It is the `[lib]` target of the `vvmux` package, with
  sources in `vvmux-terminal/`, a directory shaped like a crate that has no manifest. A dependent
  writes `vvmux = ...` but imports `vvmux_terminal`. The terminal emulator is independently useful
  (M-SMALLER-CRATES), and today it compiles in the same package as wasmtime, axum, and rustls.
- Members live inside the root package directory rather than as siblings (M-CRATES-FLAT-FOLDER).
- There is no `[workspace.package]` or `[workspace.dependencies]`. Authors, license, repository,
  edition, and `rust-version` are repeated, and shared dependencies (`regex` with identical
  features, `serde`, `serde_json`, `semver`, `toml`) are declared per crate.
- `vvmux-plugin-sdk` depends on `vvmux-plugin-api` by bare `path`, without a version, so it
  cannot be published as declared. It also lacks `authors`/`repository`
  (Clippy `cargo_common_metadata`: 8 across the workspace). `vvmux` requires API `0.5.4` while the
  member is `0.5.5`.

The repository-level instruction to keep separate projects overrides any suggestion to merge vvmux
into a repository-wide workspace. The two wasm guest crates are fixture crates and fit the
guideline's dummy-crate exception.

**Close:** move shared metadata and dependency versions into the workspace, version the intra-
workspace dependencies there, and consider extracting `vvmux-terminal` into a real member crate.

### G16 — Debug coverage is incomplete

**Evidence:** `-W missing_debug_implementations` reports six `vvmux_terminal` types: `Terminal`
([lib.rs](../vvmux-terminal/src/lib.rs) line 533), `PtyProcess`, `PtyParts`, and `PtyInput`
([pty/mod.rs](../vvmux-terminal/src/pty/mod.rs) lines 21, 23, and 36), plus `PtyControl` and
`PtyWaiter` ([pty/unix.rs](../vvmux-terminal/src/pty/unix.rs) lines 15 and 38). The SDK's
`NativeHost` lacks Debug. `vvmux-plugin-api` is fully covered, including a manual
`SchemaDocument` implementation.

**Close:** add bounded Debug implementations; terminal contents and child environments should be
summarized, not dumped.

### G17 — Re-export shims outlived the gateway reshape

**Evidence:** [src/bridge.rs](../src/bridge.rs) (7 lines) and [src/media.rs](../src/media.rs)
(62 lines) now mostly re-export `vivid_gateway` and `vivid_sdk::presenter` items under
`#[allow(unused_imports)]`. The [vvmux AGENTS.md](../AGENTS.md) code map still describes them
as owning the bridge and the virtual presenter. Some aliases are unused, as G07's warning shows.

**Close:** import those items from their original crates where they are used, delete the dead
aliases, and update the code map. This is internal to the binary, so it is not a public-path
defect; it is aliasing and stale documentation that the root AGENTS.md asks not to preserve.

### G18 — Timeouts and limits lack rationale

**Evidence:** 186 of 280 numeric constants in production code have no comment, and 146 inline
`Duration` literals exist outside constants. Examples: `IDLE_CONFIRMATION_LIMIT` (700 ms),
`STARTUP_GRACE` (3 s), and `FULL_PROCESS_RECHECK` (5 s) in [src/agent.rs](../src/agent.rs)
lines 47–51; `STEP_SETTLE`, `MIN_ALIGNMENT_RATIO_PERCENT`, and related values in
[src/alt_read.rs](../src/alt_read.rs) lines 13–22. The PTY's 250 ms SIGHUP-to-SIGKILL grace
period is an inline literal repeated in two functions of
[pty/unix.rs](../vvmux-terminal/src/pty/unix.rs). By contrast, `vvmux-plugin-api` bounds and several
`vvmux_terminal` limits are well explained.

**Close:** name and justify each timing and size choice: why this value, what changing it
affects, and which external contract constrains it. Prioritize lifecycle, termination, and
protocol limits.

### G19 — Measure the allocator and hot paths

**Evidence:** no `mimalloc` or global allocator, no benchmarks, and no profiling documentation.
The daemon parses all pane output, diffs render state, and forwards media records continuously.

**Close:** add benchmarks for terminal ingest, render diffing, and VVMX framing. Then evaluate
mimalloc for the binary only (never from `vvmux_terminal`), adopting it or recording an
evidence-based exception. No improvement is claimed without measurement.

### G20 — Modules are too large to navigate

**Evidence:** [src/session.rs](../src/session.rs) is 22,314 lines (about 19,800 production);
`src/client.rs` is 6,228, `src/plugin.rs` 4,656, `src/agent.rs` 3,869, and
`vvmux-terminal/src/lib.rs` 3,655 in a single module. Clippy's `too_many_lines` fires 92 times.

**Close:** split the session actor by concern (automation dispatch, render scheduling, projection,
persistence) without weakening the single-mutator invariant, and give `vvmux_terminal` modules for
parsing, grid, and graphics. Line counts identify review candidates; they are not violations by
themselves.

## Existing strengths, exceptions, and audit limits

- All crates use edition 2024. `cargo fmt --check` and default Clippy with `-D warnings` pass, the
  full test suite passes, and `cargo udeps` finds no unused dependencies.
- Integration tests live under `tests/` and share one binary, matching M-INTEGRATION-TESTS.
  Owner-scoped media and lifecycle rules have dedicated two-producer regressions, as AGENTS.md
  requires.
- `read_frame`/`write_frame` accept `impl Read`/`impl Write` and reject oversized frames before
  allocating. The PTY input queue is bounded by count and bytes, the plugin manifest enforces
  explicit bounds, and lock poisoning is handled explicitly.
- Child environments strip Vivid endpoints, tokens, and root secrets. Log files are created with
  mode 0600. Three of four `pre_exec` hooks use only async-signal-safe calls.
- Statics in the binary are application state and are outside M-AVOID-STATICS.
  `HYPERLINK_SEQUENCE` in `vvmux_terminal` is a documented, deliberate process-wide counter;
  uniqueness would weaken only if two copies of the library were linked into one daemon. That is
  recorded here as an accepted exception.
- `serde_json::Value` in the plugin API is the protocol's payload type. It is an intentional
  interoperability exposure under M-DONT-LEAK-TYPES, unlike the error-type leaks in G09.
- M-TARGET-CPU is server guidance; vvmux ships generic desktop and server binaries, so a raised
  target CPU is not prescribed.
- **Not verified:** Windows and Linux builds, tests, and Clippy (only macOS ran locally); Miri;
  release-mode performance; the wasm32 guest crates other than through the conformance test that
  builds the fixture. All counts are specific to the macOS default configuration.

## Audit validation

Commands ran from `vvmux/` on macOS with rustc/cargo 1.98.1, using `--locked --offline`
except where noted. The lockfile was byte-identical before and after.

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | Pass; stable rustfmt reports ignored nightly-only settings. |
| `cargo clippy --workspace --all-targets -- -D warnings` | Pass. |
| `cargo test --workspace --all-targets` | Pass: 654 tests, 3 ignored (two are re-executed child helpers). Socket tests ran without sandbox denial. |
| `cargo test --doc --workspace` | Pass, but zero doctests in all three libraries. |
| `cargo doc --workspace --no-deps` | Two unresolved-link warnings (G08). |
| `cargo rustdoc -- -W missing_docs` per library | 115 / 240 / 1 missing (terminal / API / SDK). |
| `cargo rustc --lib -- -W missing_debug_implementations` | 6 in `vvmux_terminal`; none in the API; 1 in the SDK. |
| Guideline lint set via Clippy CLI flags | 1,158 warnings (G14). |
| `cargo hack check --workspace --feature-powerset --all-targets` | **Fail** at `--no-default-features` (G07). Non-test targets pass in all configurations. |
| `cargo audit` (fetched the advisory database) | **Fail**: RUSTSEC-2026-0285 in `rustls` 0.23.43 (G05). |
| `cargo +nightly udeps --workspace --all-targets` | Pass. |
| `cargo +1.95.0 check --workspace --all-targets` | **Fail**: dependencies require 1.96 (G06). |
| `cargo +1.88.0 check -p vvmux-plugin-sdk` | **Fail**: `vvmux-plugin-api` requires 1.95 (G06). |

## Suggested remediation order

1. Fix G01 and G02; both are small, local changes with clear regressions.
2. Update `rustls` (G05), correct the MSRVs (G06), and repair the feature build (G07).
3. Stand up check-in CI across Linux, macOS, and Windows (G04), then ratchet the unsafe
   documentation (G03) and the lint policy (G14) through it.
4. Stabilize the published plugin contract: docs and nameable types (G08), errors (G09), and
   SDK surface and testability (G10), with Debug (G16) alongside.
5. Route daemon diagnostics through logging (G11, then G12) and remove the G13 panic coupling.
6. Treat G15 and G17–G20 as structural and measured follow-up work.

## Remediation record (2026-10-08)

Direction: fix every finding except whose remediation is a breaking API change. Everything below is
uncommitted working-tree state on top of the audited commit; the lockfile changes (rustls bump,
example-crate relock) stay with their dependency changes, and the audit's byte-identical-lockfile
statement applied to the audit only.

| ID | Status | What changed |
| --- | --- | --- |
| G01 | Fixed | `PendingPipe::drop` waits for a cancelled connect (`CancelIoEx` then a blocking `GetOverlappedResult`) before the boxed fields free; the boxing/address-stability invariant and `Send` reasoning are documented at the type; token/ACE buffers are pointer-aligned (`Vec<usize>`) and the SID offset uses `offset_of!`; Windows regression `dropping_a_listener_with_a_pending_connect_releases_the_pipe`. |
| G02 | Fixed | The PTY `pre_exec` hook returns `io::Error::last_os_error()` with the async-signal-safety contract stated at the `unsafe` block; the SIGHUP→SIGKILL grace is the named, reasoned `TERMINATE_GRACE`. |
| G03 | Fixed | Every production `unsafe` block, function, and impl carries `SAFETY`/`# Safety` reasoning; `agent::read_value` states its plain-old-data precondition; unjustified `unsafe impl Send/Sync` were removed or justified; `undocumented_unsafe_blocks`, `unnecessary_safety_comment`, and `unnecessary_safety_doc` warn workspace-wide; Miri runs the pure-Rust library tests in CI (`--skip pty::`). |
| G04 | Fixed | Root-repo `.github/workflows/vvmux-ci.yml`: a native job on ubuntu-24.04/macos-15/windows-2025 (fmt; clippy default and `--no-default-features`; tests) and a guidelines job (rustdoc `-D warnings -D missing_docs`; doctests; cargo-hack powerset; cargo-audit; cargo-udeps; pinned-toolchain MSRV checks; wasm32-wasip2 clippy/checks for the SDK and both guest crates; Miri). The Windows sources now compile in CI; they were not compiled locally. |
| G05 | Fixed | `rustls` updated within 0.23 past the advisory; `cargo audit` clean and gated in CI. |
| G06 | Fixed | `rust-version` is 1.96 for the workspace (wasmtime 49's line) and 1.95 for the plugin crates and guest fixtures; the stale wasmtime comment corrected; both MSRVs verified with pinned toolchains. |
| G07 | Fixed | The ungated integration-test modules are feature-gated; the dead `bridge` re-export and `BridgeWorker::spawn_with_sender` removed; the powerset is clean in all five configurations. |
| G08 | Fixed | All three libraries are `-D missing_docs` clean; runnable doctests cover a manifest load+validate, a framed hello/invoke round trip, the SDK service loop, and `Terminal`; public functions carry `# Errors`/`# Panics`; `AgentLaunch` and every `MAX_*` bound constant are exported from the crate root; both unresolved links fixed; the exact-shape compatibility policy is stated in the plugin API crate docs. |
| G09 | Partial | Non-breaking half done: `ManifestError`/`FrameError` forward `source()` and gain `From` conversions; `PluginError` implements `Display` + `Error`; `read_frame`/`write_frame` use `?`; the SDK returns a typed `ServeError` with a captured backtrace and `is_closed`/`is_io`/`is_protocol`. Skipped as breaking: restructuring the public error enums into opaque canonical structs with private kinds, and replacing `Result<(), Vec<String>>` schema validation with an error type. |
| G10 | Fixed | An explicit re-export list replaces the glob; a `Service` trait (with a closure blanket impl) plus `serve_service`/`serve_service_on(reader, writer, …)`; `NativeHost::new` is public so handlers are testable against scripted streams; bounded `Debug` on `NativeHost`. |
| G11 | Fixed | Daemon-side `eprintln!` diagnostics (snapshot/history write and load, agent resume, plugin stderr truncation) route through `log::warn!`; `eprintln!` remains foreground-CLI only. |
| G12 | Fixed | Named structured events (`event = "dotted.name"` with typed key-value fields) render as one JSON object per line; home-relative paths are redacted (`~`, tested); `log.start` and `process.panic` events exist; argv logging records only the subcommand. |
| G13 | Fixed | `method_needs_pane` is an exhaustive match over `AutomationMethod`; dispatch resolves panes through a `#[track_caller] required_pane` whose panic names the broken pairing; the 43 `unwrap()`s and message-less `unreachable!()`s are gone. |
| G14 | Fixed | `[workspace.lints]` carries the guideline set with per-opt-out reasons; item overrides are reasoned `#[expect]`s; platform/test interactions use `cfg_attr` (for example `cfg_attr(not(test), expect(...))`) so expectations stay fulfilled on every target; both feature configurations lint clean. |
| G15 | Partial | `[workspace.package]`/`[workspace.dependencies]` added and inherited by members; the SDK depends on the API through a versioned workspace dependency; READMEs added (`cargo_common_metadata` clean); fixture `rust-version`s aligned. Skipped as breaking/packaging: extracting `vvmux_terminal` into its own package and moving members to sibling directories. |
| G16 | Fixed | Bounded `Debug` for `Terminal`, `PtyProcess`, `PtyParts`, `PtyInput`, `PtyControl`, `PtyWaiter`, and `NativeHost` — shape and counters, never contents. |
| G17 | Fixed | `src/bridge.rs` deleted; call sites use `vivid_gateway` and `vivid_sdk::presenter` items directly; `src/media.rs` reduced to the platform↔presenter adapter; the vvmux AGENTS.md code map updated. |
| G18 | Fixed | Operational constants named with rationale (termination graces, pipe buffers, readiness limits, reconnect caps, environment bounds); each documented value was checked against behavior and several draft comments corrected during verification. |
| G19 | Fixed | `benches/terminal_ingest.rs` (plain/SGR/redraw/wide workloads, MiB/s, `VVMUX_BENCH_SECONDS`, `harness = false`) added; mimalloc measured A/B on macOS arm64 — three alternating 3 s runs per allocator: plain 46→50, colored 57→60, wide 58→64 MiB/s, redraws unchanged — and adopted in the binary only, with the measurement recorded at the allocator; libraries set no allocator. |
| G20 | Partial | `src/session.rs` (22,314 lines) split into `src/session/` (mod plus agents, automation, clients, commands, input, overlays, pane_ops, panes, persistence, render, waiters, tests) behind one private vocabulary; `vvmux-terminal/src/lib.rs` (3,655 lines) split into cell/dcs/event/kitty/marker/osc/terminal modules. `client.rs`, `plugin.rs`, and `agent.rs` remain large; the single-mutator invariant is untouched. |

Deliberately skipped as breaking API changes: G09's opaque error-struct redesign and
`Result<(), Vec<String>>`-to-error-type change, and G15's `vvmux-terminal` extraction and
sibling-directory layout. No `// Rust guideline compliant` markers were added; declaring compliance
is a separate decision from this remediation.

Remediation validation, from `vvmux/` on macOS (rustc/cargo 1.98.1 unless pinned):

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | Pass. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` (and `--no-default-features`) | Pass. |
| `cargo test --workspace --all-targets --locked` | Pass: 662 tests, 3 ignored. |
| `cargo test --workspace --doc --locked` | Pass: 4 doctests (1 terminal, 2 plugin API, 1 SDK). |
| `RUSTDOCFLAGS="-D warnings -D missing_docs" cargo doc --workspace --lib --no-deps --locked`; plain full `cargo doc` | 0 warnings in both. |
| `cargo hack check --workspace --feature-powerset --all-targets --locked` | Pass, 5 configurations. |
| `cargo audit` | Pass. |
| `cargo +nightly udeps --workspace --all-targets --locked` | Pass. |
| `cargo +1.96.0 check --workspace --all-targets --locked` | Pass. |
| `cargo +1.95.0 check -p vvmux-plugin-api -p vvmux-plugin-sdk --all-targets --locked` | Pass. |
| `cargo clippy -p vvmux-plugin-sdk --target wasm32-wasip2 --locked -- -D warnings` + both guest-crate checks | Pass; the example crate's `Cargo.lock` was relocked with the SDK dependency reshape so `--locked` holds. |
| `cargo +nightly miri test -p vvmux --lib --locked -- --skip pty::` | Pass: 50 tests, 6 filtered. |

Subsequent platform verification (2026-10-09): Linux passed the required and optional gates
(662 passed / 0 failed / 3 ignored); Windows passed fmt, both Clippy feature configurations, and
all-target workspace tests (565 passed / 0 failed / 3 intentional ignores), including the G01
pending-connect cancellation regression and the real ConPTY integration tests.
[ms-rust-handoff.md](ms-rust-handoff.md) §2a and §3a record the fixes, exact coverage, and
toolchain caveats. G04 still awaits a green cross-platform CI run: the existing workflow failed
during checkout of an unrelated private submodule, and its checkout is now scoped to vvmux and
the required shared crates.
