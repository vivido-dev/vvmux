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

Each media track starts with one record of flow allowance so recovery control can overtake at most
one stale packet. Its first completed outer delivery expands only that track to the bounded rolling
window declared by its immutable inflight-byte limit. In particular, linked audio can prebuffer and
absorb acknowledgement jitter without borrowing capacity from video or another producer.

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

Private VVMX version 11 is a hard cutover. Its binary media header carries complete track identity
and bounded binary render/media records,
pane-scoped sanitized media status and waits, bridge-instance correlation, and metadata-only media
traces. Host terminal focus is its own client message, so it reaches only a pane whose program
enabled focus reporting. Mixed VVMX versions are rejected with restart guidance.

```text
Vivi → inner vvmux presenter → VVMX 11 → outer vvmux producer → Vivido
```

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
