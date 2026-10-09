# vvmux guideline remediation — handoff for Linux/Windows verification

Written 2026-10-09, after the macOS-side remediation of
[ms-rust-gaps.md](ms-rust-gaps.md) (see its *Remediation record* for what changed and why).
Everything is **uncommitted working-tree state**: in the `vvmux` submodule on top of `1bc5506`
(77 modified, `src/bridge.rs` deleted, `src/session.rs` renamed to `src/session/mod.rs`, 24 new
files), plus two root-repo changes: the untracked `.github/workflows/vvmux-ci.yml` and the
code-map edit in `AI/vvmux-AGENTS.md`.

macOS passed every gate in the record's validation table. Linux verification is recorded in
§2a and Windows verification in §3a. Both platform runs are now complete; the first green
cross-platform CI run and landing the Windows changes remain.

## 1. Prerequisites (both OSes)

```sh
rustup toolchain install stable --profile minimal --component rustfmt,clippy
rustup toolchain install 1.95.0 1.96.0 --profile minimal
rustup toolchain install nightly --profile minimal --component miri
rustup target add wasm32-wasip2            # only needed for the guidelines pass
cargo install cargo-audit cargo-hack cargo-udeps --locked   # only needed for the guidelines pass
```

All commands run from `vvmux/` and use `--locked`; a stale-lockfile error is a finding, not
something to delete the flag over. The one intentional relock (`examples/plugins/rust-component/
Cargo.lock`, needed by the SDK dependency reshape) is already in the tree.

## 2. Linux

The three AGENTS.md commands, plus the deeper gates if you want full parity with the macOS run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --no-default-features --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Optional extras (already green on macOS; platform-independent in theory, cheap to confirm):

```sh
cargo test --workspace --doc --locked
RUSTDOCFLAGS="-D warnings -D missing_docs" cargo doc --workspace --lib --no-deps --locked
cargo hack check --workspace --feature-powerset --all-targets --locked
cargo audit
cargo +nightly udeps --workspace --all-targets --locked
cargo +1.96.0 check --workspace --all-targets --locked
cargo +1.95.0 check -p vvmux-plugin-api -p vvmux-plugin-sdk --all-targets --locked
cargo clippy -p vvmux-plugin-sdk --target wasm32-wasip2 --locked -- -D warnings
cargo check --manifest-path tests/fixtures/component_guest/Cargo.toml --target wasm32-wasip2 --locked
cargo check --manifest-path examples/plugins/rust-component/Cargo.toml --target wasm32-wasip2 --locked
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p vvmux --lib --locked -- --skip pty::
```

**Expected:** the macOS run was 662 passed / 0 failed / 3 ignored across 8 suites; the Linux count
can differ slightly where tests are platform-gated. What to actually watch:

- **Agent process inspection.** `foreground_processes` (`src/agent.rs:2035`) reads `/proc`; the
  macOS sibling uses `libc::proc_listpids(PROC_PGRP_ONLY, …)` and **returns bytes, not a pid
  count**. An earlier draft confused the two and exactly three tests caught it: the
  `agent_start…`, `a_detected_agent…`, and `a_restored_agent_pane…` families. If those fail on
  Linux, look at the `/proc` parsing first.
- **`peer_uid`** (`src/platform/unix.rs:808`) — Linux-only `SO_PEERCRED` with `&raw mut` casts;
  auth/identity/runtime/update call it. Its tests are the `gateway_*`/auth ones.
- **Unix socket tests** must run where socket creation is allowed (no sandbox `PermissionDenied`
  skip path); AGENTS.md requires rerunning them un-sandboxed if they skip.
- `tests/integration/overlay_python.rs` needs a Python on PATH; it should skip cleanly otherwise.

## 2a. Linux record (2026-10-09)

Linux ran every gate in §2 including all the optional extras. Results:

- fmt, clippy default, clippy no-default, tests: **662 passed / 0 failed / 3 ignored** across 8
  suites, identical to macOS; the only ignored tests are the three intentional
  `*_producer_child` re-execution harness tests, so no socket test skipped. The
  `agent_start…`/`a_detected_agent…`/`a_restored_agent_pane…` families and the
  `gateway_*`/auth (`peer_uid`) tests all pass unmodified — the `/proc` and `SO_PEERCRED`
  implementations were correct as written.
- Doctests, rustdoc `-D warnings -D missing_docs`, feature powerset, audit, udeps, both MSRV
  pins, the three wasm32-wasip2 checks, and Miri (lib suite, 50 passed / 6 `pty::` filtered):
  all green.

Two fixes fell out, both from **stable clippy 1.99.0**, which is newer than the toolchain the
macOS run used (`assert_is_empty` and the Linux-only `semicolon_outside_block` site were
invisible there):

1. `Cargo.toml` — the workspace lint table now allows `clippy::assert_is_empty` with a reason
   (pedantic, new in 1.99; rewriting every `assert!(x.is_empty())` would need a typed empty
   literal per site). If your Windows clippy is ≥ 1.99 you inherit this automatically via the
   shared table.
2. `tests/integration/tunnel_connect.rs` — the Linux-only `#[cfg]` `/proc` block moved its `;`
   outside the brace (`semicolon_outside_block`); macOS never compiles that block, which is why
   macOS clippy passed. First confirmation of the handoff's cross-target lint warning, in
   reverse: this one only fires on Linux.

Also fixed for CI: `.github/workflows/vvmux-ci.yml` now installs `rust-src` alongside `miri` —
`cargo miri test` otherwise prompts interactively for `rust-src` on first use, which fails a
non-TTY runner. All changes are uncommitted, per §5.

## 3. Windows

This is the real debt: `src/platform/windows.rs`, `vvmux-terminal/src/pty/windows.rs`, and the
ConPTY paths have not been compiled since they were edited. Beyond `rustup`, the build needs
**NASM and CMake on PATH** for `aws-lc-sys` (MSVC toolchain assumed).

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --no-default-features --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

What changed in the never-compiled code, in rough order of "would be my fault first":

- **G01 fix** — `PendingPipe::drop` now calls `CancelIoEx` *then a blocking*
  `GetOverlappedResult` before the boxed fields free (mirrors the existing read path). New test:
  `dropping_a_listener_with_a_pending_connect_releases_the_pipe` (bind → pending connect → drop →
  rebind the same pipe name).
- **Pointer-aligned token buffers** — `ProcessToken` buffers are `Vec<usize>` (was `Vec<u8>`) for
  `TOKEN_USER`/SIDs; the ACE SID offset uses `std::mem::offset_of!`. Misalignment here is an
  easy-to-introduce bug; if `GetTokenInformation` fails oddly, check these first.
- **Removed `unsafe impl Send/Sync for ControlInner`** in `pty/windows.rs` — sound because
  `HPCON` is an `isize` and the inner fields are plain handles; if the compiler disagrees, restore
  them *with* SAFETY reasons rather than reverting the removal blind.
- **Split transmutes** (ConPTY structs) each carry their own `SAFETY` comment; `INFINITE` comes
  from `windows-sys`; inline magic numbers became `MAX_WIDE_PATH_UNITS`, `CP_UTF8`,
  `STARTUP_TIMEOUT`, `READINESS_LIMIT`, `READINESS_PIPE_BYTES`, `PIPE_BUFFER_BYTES`,
  `EXIT_GRACE_MS`, `CLOSE_GRACE_MS`, `KILL_WAIT_MS`, `MAX_ENVIRONMENT_ENTRY_BYTES`.
- **~120 SAFETY comments** were added across the file. If any clippy/rustc error points at one,
  the comment may sit in the wrong block — a few were line-number-applied and hand-corrected, but
  Windows never compiled to confirm placement.

Two Windows-specific expectations:

- The new **workspace lint table has never run on Windows-only code**. Pedantic lints that
  macOS never sees (e.g. in `windows_*` modules) may fire. Fix the code, or add a reasoned
  `#[expect]` — use `cfg_attr` when the lint only fires on some targets, or the expectation goes
  unfulfilled on the others.
- The `windows_*` integration suite (`ctrl_c`, `resize`, `reattach`, `conpty_daemon`,
  `automation`) is the behavioral coverage for the PTY changes; the `windows/*.ps1` soak scripts
  stay manual and are unchanged.

Miri, udeps, audit, powerset, wasm32, and the MSRV pins do not need a Windows rerun; the CI job
only asks Windows for fmt/clippy/tests too.

## 3a. Windows record (2026-10-09)

Verified locally on x86_64 Windows/MSVC with rustc/cargo 1.98.0. All commands ran from
vvmux/; both Clippy configurations and the tests kept --locked, and no lockfile changed.

| Command | Result |
| --- | --- |
| cargo fmt --all --check | Pass; stable rustfmt reports ignored nightly-only settings. |
| cargo clippy --workspace --all-targets --locked -- -D warnings | Pass. |
| cargo clippy --workspace --all-targets --no-default-features --locked -- -D warnings | Pass. |
| cargo test --workspace --all-targets --locked | Pass: **565 passed / 0 failed / 3 ignored**, across 8 suites (two have no Windows tests). |

Clippy 1.98 reports the shared clippy::assert_is_empty opt-out as an unknown lint because that
lint was introduced in 1.99 (§2a). This toolchain warning does not fail either command; the
workspace opt-out remains for newer Clippy. No Windows code lint failures remain.

Windows-only fallout fixed:

- Explicit raw-pointer borrows at Win32 FFI calls, statement semicolons, documentation markup,
  redundant imports/casts/closures, struct field order, and explicit Arc::clone.
- PendingPipe retains its event as _event, with an ownership comment: it must stay alive
  through cancellation completion even though Rust never reads that field directly.
- split_pipe now returns its infallible transport directly; callers wrap it at their fallible
  boundary. Hex encoding uses the existing hex dependency.
- Windows signal conversion, signal installation, and directory-sync stubs carry reasoned
  item-level lint expectations that preserve their shared platform signatures.
- The Windows Python overlay test converts coordinates through checked i32 conversion
  before lossless f64 conversion.
- image_probe::probe_first_image_after_cls is now an explicitly ignored manual diagnostic:
  it requires a separately built sibling vivi.exe and machine-specific native libraries,
  prints observations rather than asserting image acceptance, and cannot run on a clean vvmux
  CI checkout. Its quiet-output wait now has a ten-second ceiling so continuous redraws cannot
  keep it waiting indefinitely. The original run completed this diagnostic successfully
  (566 passed / 0 failed / 2 ignored); the final run verifies the new opt-in configuration.

All named Windows behavioral checks passed: pending-connect drop/rebind (32 iterations),
owner-only pipe round-trip/cancellation, runtime owner/DACL validation, Ctrl+C directly and through
an attached session, automatic split resize, detach/immediate reattach, detached daemon readiness,
real ConPTY anchor wrapping, structured automation, and concurrent clients. The ConPTY control
type compiled without restoring the removed unsafe impl Send/Sync.

The final three ignores are the re-executed producer child helper, the existing opt-in Python
overlay acceptance test, and the manual image diagnostic. No Windows regression was skipped.

CI inspection found that [run 37891049531](https://github.com/wensheng/vivido-private/actions/runs/37891049531)
failed on every host during checkout, before Rust ran: recursive checkout tried to clone the
unrelated private wensheng/vvmux.com submodule. Both workflow jobs now initialize only vvmux,
vivid_protocol, vivid_sdk, vivid_gateway, and vvte, using HTTPS for their GitHub URLs.
cargo metadata --locked confirms these are all the external local path dependencies.
This workflow correction and the Windows fixes still need to land and receive a green CI run;
local success does not close that CI gate.

## 4. First CI run

Once committed and pushed to `dev` (or PR'd), `.github/workflows/vvmux-ci.yml` runs:

- `native` on ubuntu-24.04 / macos-15 / windows-2025 — fmt, clippy default + no-default, tests.
- `guidelines` on ubuntu — rustdoc `-D warnings -D missing_docs`, doctests, cargo-hack powerset,
  audit, udeps, both MSRV pins, wasm32-wasip2 (SDK + both guest crates), Miri.

It initializes only the required submodules listed in §3a and path-filters on vvmux,
vivid_*, and vvte.
Per the audit's own rule (G04: "a configured gate is unverified until it has passed on its host"),
the finding is only closed once this workflow is green — treat its first Windows results with the
same priority as a local Windows run.

## 5. Wrap-up checklist

1. Land Linux + Windows results; fix fallout per the notes above.
2. Confirm CI is green on all three OSes.
3. Commit. In the submodule, stage everything this work produced — the split `src/session/`
   modules, `vvmux-terminal/src/{cell,dcs,event,kitty,marker,osc,terminal}.rs`, `benches/`,
   `docs/`, the two plugin-crate READMEs, `tests/integration/common/tunnel.rs`, the lockfile
   changes (rustls bump, mimalloc, example relock — they belong with the dependency changes that
   caused them), and the `session.rs → session/mod.rs` rename. In the root repo: the workflow
   file, the `AI/vvmux-AGENTS.md` edit, and the submodule pointer. **No Claude/Anthropic
   attribution lines** in commit messages or PR bodies (repo rule).
4. Leave alone: `research/competitors/…`, `vivid_sdk`, `vivida`, `vivido`, `vivi`,
   `vivido.js.old/` — pre-existing or parallel work, not part of this remediation.

## 6. Known follow-ups deliberately deferred (breaking API changes)

- **G09**: restructuring `ManifestError`/`FrameError` into opaque canonical structs with private
  kinds, and replacing `Result<(), Vec<String>>` schema validation with a real error type. The
  additive half (forwarded `source()`, `From` conversions, `Display`/`Error` on `PluginError`,
  typed `ServeError`) is done.
- **G15**: extracting `vvmux_terminal` into its own package and moving workspace members to
  sibling directories. Workspace metadata/dependency inheritance is done.
- **G20 leftovers**: `src/client.rs` (~6.2k lines), `src/plugin.rs` (~4.7k), `src/agent.rs`
  (~3.9k) are still above a reviewable size; `session/` and `vvmux-terminal` were split.
- **Compliance markers**: none were added anywhere; declaring files
  `// Rust guideline compliant 2026-10-07` is a separate decision from this remediation.

## 7. One trap, written down so nobody "cleans it up"

The session split modules carry:

```rust
#![cfg_attr(
    not(test),
    expect(
        clippy::wildcard_imports,
        reason = "a part of the session actor, sharing its module's private vocabulary"
    )
)]
```

This shape is load-bearing: `clippy::wildcard_imports` fires on `use super::*` in normal builds
but exempts test builds, so a bare `#![expect]` is *unfulfilled* under `--cfg test` (an error
under `-D warnings`), and removing it entirely lets the wildcard warning through on every
non-test compile. The same `cfg_attr` pattern is needed for any future expectation that only
fires on some targets or configurations.
