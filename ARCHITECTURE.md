# vvmux architecture

## Session actor and pending work

The session actor is the single writer for terminal, layout, scene-projection, and automation
state. Events enter through a bounded synchronous channel and are admitted in receive order. Code
running on the actor must not perform device I/O, wait for media, join a thread, or wait for a PTY
write.

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

## Vivid 1.1 and revision domains

The pane-facing virtual presenter and the outer presenter are different Vivid sessions. Virtual
scene/source revisions, observation sequences, attachment generations, record sequences, raster
frame bases, and EOS barriers never cross the boundary as identities. The session actor separately
tracks a monotonic outer compatibility revision and apply sequence; the current foreground bridge
reports its own instance ID and local revision. Replacing a bridge cannot move compatibility state
backward or perturb pane-owned virtual revisions.

Private VVMX version 9 is a hard cutover. It carries bounded binary render/media records,
pane-scoped sanitized media status and waits, bridge-instance correlation, and metadata-only media
traces. Mixed VVMX versions are rejected with restart guidance.
