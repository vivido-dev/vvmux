# vvmux architecture

## Session actor and pending work

The session actor is the single writer for terminal, layout, scene-projection, and automation
state. Events enter through a bounded synchronous channel and are admitted in receive order. Code
running on the actor must not perform device I/O, wait for media, join a thread, or wait for a PTY
write.

Media payloads use a dedicated bounded receiver, but that receiver is serviced in finite batches:
a live producer can refill a bounded channel while it is being drained, so exhaustion is not a
fairness boundary. Media/projection wakeups are dirty-bit coalesced, keeping at most one redundant
wake in the general actor queue until the actor observes the pending work. Detach, pane input,
delivery acknowledgements, and projection control therefore retain a scheduling turn under
continuous playback.

Video recovery remains pending from inner keyframe ingest through the foreground bridge's outer
delivery acknowledgement. The level-triggered recovery bit is edge-coalesced before crossing VVMX,
so media-only projection revisions cannot queue duplicate requests that arrive after the good
keyframe and force the producer to skip to the next GOP.

Linked audio is parked only while a replacement video keyframe has not been *accepted* yet.
Accepting one arms the catch-up floor that discards retired-epoch samples, which is what keeps them
out of the new physical audio clock, so the park is released there rather than on the outer
acknowledgement. It has to be: the outer relay publishes the replacement PLAY only once every
linked member has submitted pre-roll, so audio held for that acknowledgement holds the activation
the acknowledgement is waiting for. For the same reason the first record of a replacement
generation on an already-presented audio track — a seek's handoff — passes the gate outright.

Each media track starts with one record of flow allowance so recovery control can overtake at most
one stale packet. Its first completed outer delivery expands only that track to the bounded rolling
window declared by its immutable inflight-byte limit. In particular, linked audio can prebuffer and
absorb acknowledgement jitter without borrowing capacity from video or another producer. A seek's
replacement generation is re-granted that window on its handoff record instead, because the outer
delivery that would earn it cannot complete until the PLAY the prebuffer gates.

A paused timed outer track forwards one bounded pre-roll record at a time before PLAY, so the
writer queue never holds more than one. Slot activation needs decoded output, and a decoder
routinely wants several access units before it emits its first picture, so a blocked activation
attempt is itself the evidence that admits one more record, up to the ceiling the track declared.
Otherwise the allowance and the readiness it feeds wait on each other.

Ordinary timed audio/video packets update delivery and query status without advancing the virtual
scene projection. PLAY, PAUSE, EOS, recovery edges, track lifecycle, and retained image/raster
updates still advance it. This separation prevents a packet burst from placing hundreds of no-op
projection reconciliations ahead of the media records they describe.

An operation that cannot complete promptly follows the pending-work pattern:

1. Validate and admit the operation on the actor in receive order.
2. Reserve a key in the session-wide `pending_actor_work` set. The set is bounded by
   `MAX_PENDING_ACTOR_WORK`; exhaustion rejects new work instead of growing a queue.
3. Give only the blocking or computational portion to a named worker.
4. Return completion through a typed `ActorEvent`.
5. Release the pending key and apply any resulting mutation or reply on the actor.

`automation_input` is the reference implementation: PTY completion waits happen on a worker while
the actor continues servicing media delivery, rendering, client events, and unrelated automation.
Vivid queries, waits, device operations, and status work use the same pattern.
Workers must never mutate `SessionActor` state directly or write session replies themselves.

Agent launch uses the same pattern for fresh process-table probes. The actor admits and scopes the
request, but a bounded probe worker performs `/proc` and platform process inspection; the actor
never performs process I/O. Its typed result is revalidated against pane identity before launch.
Prompt submission uses the bounded PTY input path with a delayed-input queue: prompt bytes and the
later Enter are separate ordered writes, while the actor remains free to process unrelated work.

Alternate-screen reads are a bounded actor-owned state machine rather than a blocking transcript
scrape. Admission requires an idle detected agent, alternate-screen mode, mouse reporting, and no
copy mode. Upward wheel steps, stable-snapshot checks, overlap alignment, fixed-header-safe merges,
and viewport restoration advance on actor deadlines through the PTY queue. A read is capped at
1000 lines, the session admits at most eight concurrently, and each operation uses its frozen
admission snapshot so concurrent terminal changes cannot silently redefine its merge baseline.

Lifecycle state reports and session-only reports share one monotonic source sequence but have
different authority. `report-agent` may supersede screen classification until cleared or process
identity changes. `report-agent-session` stores only resumable native identity and never creates a
reported state, blocked message, or lifecycle transition. Broad inspection surfaces expose only
`session_present`; the session ID/path remain confined to exact-pane inspection.

Session shape is persisted by the same discipline every other actor-owned periodic job follows. A
payload-free dirty bit coalesces changes, a five-second debounce turns a burst into one write, and
the capture runs on the actor — it reads live state, so it must — while the serialization and the
filesystem write run on a named worker that reports back through an actor event. One write is in
flight at a time; a change arriving during one re-arms the debounce rather than being lost. Shutdown
writes inline, because there is no loop left to deliver a completion to.

Restore reuses the startup-layout path rather than reimplementing it: a snapshot embeds the layout
schema, is lowered by the same `lower`, and is applied by the same `apply_layout_plan`, so slot
pre-allocation, failed-leaf pruning, and float placement have one implementation. What a layout file
cannot describe — active tab, zoom, synchronized input, float position — rides alongside as extras
keyed by the pane slots that lowering assigns, and both are produced by a single capture walk so the
two orderings cannot drift. Pane IDs are assigned in layout-tree order and are therefore not stable
across a restart; agent names are the durable target.

A snapshot is untrusted input. It is bounded before it is allocated, its schema is read before its
body, and anything unusable starts a fresh session rather than failing to start one. It is owner-only
and lives outside the config directory, because it records working directories and native agent
session identity.

A request names its pane by ID, by agent name, or not at all, and one function on the actor
reconciles all three: an explicit pane ID wins, then a live agent's name, then the focused pane for
the verbs that permit it. Naming both a pane and an agent is refused rather than resolved. Agent
names are held on the per-pane agent runtime rather than on the pane, so they are released by the
same process-observation paths that release reports, session identity, and metadata — a name cannot
outlive the agent it was given to, and cannot be inherited by whatever runs in that pane next. They
are deliberately outside the lifecycle snapshot: renaming an agent is a display change, and folding
it into the snapshot would advance the per-pane transition counter that `agent-prompt` reads as its
stall baseline.

The config watcher is the third worker that wakes the actor, after the PTY readers and the media
wakeup. It polls the config path, requires one further identical observation before acting so a
half-written file is never read, and treats a missing file as no change rather than a reset to
defaults. Its wake is a payload-free `ConfigChanged` coalesced through a dirty bit, exactly like
the media wakeup; the actor re-reads the file itself, so the watcher, `SIGUSR1`, and
`msg reload-config` converge on one parse-validate-apply path and a dropped wake costs nothing.

Reload adopts only what a live session can change. A parse or validation failure leaves the
running config untouched. `[media]` was moved into the running `VirtualVivid` at startup, so its
values are carried forward rather than swapped under live retained media and in-flight tracks.
`general.prefix` and `[keys.prefix]` belong to the client's prefix parser, so new values are stored
but reported as deferred. `[server]` belongs to a separate `vvmux serve` process, so the session
carries its old values forward and reports an edit as ignored. Changing `general.status_visible`
moves the status row in or out of the pane area, so the stored displays are re-normalized before
anything derives geometry from them, followed by exactly one `relayout` for the whole change.

Startup layouts are parsed and bounded in the foreground process before daemonization, then loaded
again by the server and lowered to pane-slot plans before the session actor starts. The actor
allocates every tab-local slot before spawning, substitutes owner-scoped pane IDs only after that,
and closes failed leaves out of the candidate tree; one owner's failed spawn cannot broaden cleanup
to another session that reused the same numeric IDs. Weighted startup constructors deliberately do
not enforce the placeholder 80x23 display's interactive minimums. The first real attach supplies
authoritative geometry and relayouts once; interactive splits continue to validate the live area.

Synchronized input is tab-owned actor state. Its target snapshot deliberately enumerates the tiled
tree and the complete floating layer rather than the visible projection, so zoomed and hidden live
shells remain members; panes in copy mode are filtered out. PTY writes are attempted across the
owned snapshot first and failures are reported only after iteration. Reporting may close a pane,
remove its tab, refocus, and relayout, so mutating lifecycle state inside the fan-out loop would
invalidate both the target set and its tab. Paste uses the same collect-then-report discipline and
builds bracketed payloads per pane because terminal modes are pane-local.

## Vivid 1.5 nested presenter and revision domains

The pane-facing virtual presenter and the outer presenter are different Vivid sessions. Virtual
scene/surface/track revisions, observation sequences, channel generations, record sequences,
raster bases, media IDs, epochs, flow maxima, and EOS state never cross the boundary as authority.
Every relay key contains the complete inner session, context, surface, and track identity.

What does cross is the *shape* of the media. The outer raster track mirrors the nested track's
compression and delta grant, so the relay re-encodes each frame with the outer track's own identity
and sends it compressed whenever that is the smaller of the two forms. Raster tracks never reach
PLAY, so their writers pace one record at a time only until the outer slot is activated; after that
the outer channel's byte and record flow is the sole bound. Both matter only when the outer
connection is forwarded, where a raw framebuffer per page turn and a presenter round trip per frame
are what a nested document reader actually pays.

The session actor separately
tracks a monotonic outer compatibility revision and apply sequence; the current foreground bridge
reports its own instance ID and local revision. Replacing a bridge cannot move compatibility state
backward or perturb pane-owned virtual revisions.

Private VVMX version 20 is a hard cutover. In addition to complete track identity and binary media,
it retains deterministic outer/rendered completion waits, stable tab-ID selection, correlated
diagnostics, PTY-write reports, and process-anchored recovery traces. Its binary media header carries complete track identity
and bounded binary render/media records,
pane-scoped sanitized media status and waits, bridge-instance correlation, and metadata-only media
traces. Host terminal focus, pixel mouse input, and focused-pane Kitty keyboard flags retain their
coordinate and mode semantics across the nested terminal boundary. Focus reaches only a pane whose
program enabled focus reporting. Media snapshots also preserve each inner track's live/timed mode
and whether the authoritative surface slot map currently activates it, so live raster/audio groups
are reactivated without inventing a PLAY transition. Applied projections report recreated retained
tracks explicitly, allowing the actor to rehydrate a fresh outer image/raster attachment even when
the preceding snapshot still described an older attachment as resident. The inner presenter
terminates raster delta chains into a retained latest framebuffer; restoration re-encodes those
composed pixels as a hop-local full frame, so an interactive canvas does not fall back to its old
pre-drawing full frame. Mixed VVMX versions are rejected with restart guidance.

Version 20 also carries an attachment-scoped Kitty graphics capability. Only a native attachment
whose exact host `TERM` is `xterm-kitty` or `xterm-ghostty` sets it; gateway and Vivido attachments
leave it false. The terminal parser recognizes bounded fragmented APC packets beside the Vivid
marker scanner, accepts only direct quiet uploads, Unicode virtual placements, targeted deletion,
and support queries, and accounts in-progress plus ready transfers against a 64 MiB session bound.
Completed packets are prepended to a forced-full screen render, and the foreground client
reassembles all VVMX render chunks before one queue admission, making upload-before-placeholder
ordering atomic. Placeholder cells remain ordinary grid cells through composition and scrollback,
but are blanked for incapable attachments. Transfer state is cleared whenever the physical client
detaches and is never part of retained session state. On Unix, PTY resize propagation includes
checked pixel dimensions; Windows accepts the same internal call and continues using ConPTY cells.
This is the sole scoped PTY media exception in vvmux and does not change the Vivid wire stack.

```text
Vivi → inner vvmux presenter → VVMX 18 → outer vvmux producer → Vivido
```

## Plugin boundary

The stable extension contract lives in the separate `vvmux-plugin-api` workspace crate and is
independently versioned from private VVMX. Strict manifests and self-contained action schemas are
validated before registry mutation or execution. Native plugins use bounded length-prefixed JSON;
component plugins use the WIT world shipped by that crate. Language SDKs never encode VVMX.

Plugin process and schema work belongs outside the session actor. Only resolved, bounded commands
may mutate session state, and the actor remains its sole writer. Exact argv PTY spawning keeps
manifest commands out of shell parsing. Native plugins are trusted same-user code; only WebAssembly
Components receive a sandbox claim. Plugin media continues through the existing pane-scoped Vivid
capability path and never through terminal bytes.

The session owns its collision-resistant instance identity independently of plugin enablement. When
`plugins.enabled` is false, it starts neither a plugin supervisor nor a registry watcher; disabling
plugins in a live session immediately removes the supervisor from the actor's acceptance path,
cancels its jobs, revokes runtime identities, and stops its watcher. Enabling plugins creates those
workers without replacing the session identity. The disabled and no-package paths therefore add no
plugin-originated actor wakeups.

When enabled, each session owns one bounded plugin supervisor outside the actor. Catalog and
capability discovery come from that supervisor's applied registry snapshot, carry its generation,
and include only actions currently invocable in the exact target session. Native services and
Components receive an exact session/plugin-instance identity and a short-lived broker token through
their scrubbed environment; the token is revoked when that runtime stops and is never an argv value
or protocol payload. Native protocol handlers may issue correlated `host_call` replies. One-shot
actions receive no broker token and expose no typed host-call transport. The supervisor validates a
live service or Component token, and the actor lowers both automation and broker requests through
typed `SessionCommand`/`CallerContext` authorization before serving `session.inspect`,
`pane.get_text`, or `pane.input`. Native code remains trusted because it can bypass this broker as
the same OS user.

The same validated registry snapshot compiles a bounded agent catalog from bundled and installed
manifest-version-2 providers. The supervisor publishes the immutable catalog and its generation to
the actor, which shares it with the process detector and uses it for screen/OSC evaluation. Catalog
replacement invalidates process caches and clears only reports whose provider identity or manifest
fingerprint disappeared. Claude and Codex are default-enabled bundled data providers; OpenCode and
Hermes default off. The global plugin kill switch also removes this catalog.

Manifest PTY panes are resolved by the supervisor, then lowered as exact-argv `pane.create` commands
through the same actor service. The actor attaches complete session/plugin-instance/package
generation ownership before spawn and alone commits split, float, or tab placement. Sync keyboard
and paste share the manifest opt-in predicate. `media.produce` gates issuance of the existing
pane-scoped Vivid capability; held exit revokes it before displaying a core diagnostic. Generation
change, disable, removal, kill switch, and shutdown reuse the ordinary pane close path, so teardown
never matches only a numeric pane ID.

The actor also owns the monotonic plugin-event sequence and a bounded metadata-only replay journal.
Screen, layout, focus, configuration, and media revisions coalesce at the existing render boundary;
pane and plugin lifecycle entries remain individually ordered. Persistent VVMX subscribers use a
dedicated bounded writer and consume neither automation-waiter nor job slots. The actor only uses
`try_send` toward subscribers and the supervisor, so a stalled client or runtime cannot delay PTY,
render, or media work; overflow becomes an explicit sequence gap. The supervisor permission-checks
manifest hooks, activates `session` runtimes, and dispatches native protocol events or Component
`on-event` calls on bounded per-plugin workers. Broker-triggered mutations retain correlation and
causation identity, hooks exclude self-caused events by default, and dispatch stops at depth eight.

The Rust SDK owns the guest-side WIT bindings used to build `wasm32-wasip2` Components. Component
invocations reset a private log accumulator before entering guest code; host log imports sanitize
line breaks, serialize bounded JSON entries, and retain explicit truncation metadata with detached
jobs instead of writing guest-controlled text directly to the host's stderr. Component storage,
cache, config, and data paths use independent plugin-ID-derived components. Cache deserialization
requires the complete engine/host/artifact key and serialized payload digest; a mismatch recompiles
the pinned source artifact.

The inner presenter accepts Control and Track only, verifies root and channel authentication,
consumes marker-v3 anchors, and grants cumulative flow per track. The outer producer allocates all
of its own identities and re-encodes portable media headers. Per-track bridge queues are
independently bounded and scheduled fairly; no media writer runs on the session actor.

Each outer track has its own blocking writer. Before timed PLAY, that writer reports a completed
pre-roll record only after the outer presenter returns the record's ingress capacity; this keeps
ACTIVATE_TRACK behind actual outer processing instead of a kernel socket write. Once atomic
activation and PLAY succeed, the writer stops adding that record-by-record barrier and uses the
outer channel's normal bounded flow window. Linked audio can then stay buffered at device rate
without sharing a blocking write, decoder wait, or acknowledgement round trip with video. EOS is
queued behind all earlier records for the same track.
