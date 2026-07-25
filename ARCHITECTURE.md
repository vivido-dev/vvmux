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
New Vivid queries, waits, device operations, and Stage 2 status work must use the same pattern.
Workers must never mutate `SessionActor` state directly or write session replies themselves.
