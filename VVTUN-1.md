# VVTUN/1: vvmux outbound machine tunnel protocol

VVTUN/1 is the public protocol by which a machine's `vvmux serve --connect` gateway reaches a
vvmux_server deployment and stays reachable for browser attach, without opening any inbound port
on the machine. The gateway dials out; the server never dials in.

## Transport and authentication

The control tunnel is `GET /t/v1/control` over WebSocket with the `vvtun.v1` subprotocol. A data
leg is `GET /t/v1/leg` with the subprotocols `vvtun.leg.v1` and `vvtun.ticket.<BASE64URL_TICKET>`.

TLS is mandatory across a host boundary. `wss://` is the only scheme accepted for a non-loopback
host. `ws://` is accepted for loopback addresses only, for development and testing; a `ws://`
tunnel has no TLS exporter and the handshake signature covers an empty exporter.

The gateway connects with its enrolled identity: an Ed25519 key pair stored in an owner-only file
(`cloud-identity.json`). Only the public key leaves the machine.

### Challenge and proof

After the WebSocket upgrade, the server sends the first message, a strict JSON text frame:

```json
{"type":"challenge","protocol":1,"nonce":"BASE64URL_32_BYTES","hostname":"vvmux.example"}
```

`nonce` is fresh per authentication attempt and must not be reused within a deployment's replay
window. `hostname` is the deployment's public hostname.

The gateway replies:

```json
{"type":"auth","machine_id":"BASE64URL_PUBLIC_KEY","signature":"BASE64URL_ED25519"}
```

`signature` covers, in order, the ASCII bytes:

```text
"vvmux tunnel auth v1\0" || nonce || hostname || exporter || machine_id
```

`exporter` is 32 bytes of TLS keying material exported from the tunnel's own TLS session, or the
empty byte string when the tunnel is a loopback `ws://` development connection. Both peers derive it
from their side of the same session, so it never appears on the wire. The exporter label is the
ASCII `EXPORTER-VVTUN-1` with a null context; it is distinct from the Vivid carrier's exporter label
so the two bindings can never be confused. The server verifies the
signature against the enrolled public key for `machine_id`, in constant time, and rejects the
connection on any mismatch, indistinguishably from an unknown machine.

Because `hostname` and `exporter` are inside the signed payload, a signature captured from one
deployment or one TLS session cannot be replayed to another.

On success the server replies:

```json
{"type":"authed","protocol":1,"server_version":"0.1.0","reconnect_after_seconds":0}
```

`reconnect_after_seconds` is a scheduling hint for a planned drain; it is never an order.

The gateway then sends `machine_status`:

```json
{
  "type":"machine_status",
  "vvmux_version":"0.4.0",
  "vvws_protocol":1,
  "capabilities":["terminal-v1","session-list-v1","session-create-v1","vivid-bridge-v1","tunnel-attached-v1"]
}
```

All strings in this message are bounded: version and protocol fields are ASCII alphanumeric,
dots, dashes, and underscores at most 64 bytes; capabilities are from the fixed VVWS list and at
most 16 entries. The server must treat every value as untrusted.

## Liveness

Both layers of D2 apply on the control tunnel.

The **socket layer**: the server sends WebSocket ping frames every 30 seconds. The gateway answers
pong frames with the same payload. This keeps NAT and proxy mappings warm and proves the socket
lives.

The **protocol layer**: the gateway sends

```json
{"type":"ping","nonce":1}
```

every 30 seconds and the server replies

```json
{"type":"pong","nonce":1}
```

with the same nonce. This proves the VVTUN state machine is running, not merely the transport.

A peer that misses three consecutive intervals is dead at 90 seconds: the gateway closes the
tunnel and reconnects; the server marks the machine offline. The gateway reconnects with
full-jittered exponential backoff, a uniform random delay in `[0, cap]` with the cap doubling from
1 second to 60 seconds. When the server answers a connection attempt with `503` and
`Retry-After`, the gateway treats the header as its next delay, capped at 60 seconds, rather than
as a generic failure.

## Legs

The server opens a data leg for each browser socket, and only for a socket it has already
authenticated and paired. One leg is byte-transparent to exactly one browser socket.

Control frames, all strict JSON of at most 64 KiB. Unknown fields and unknown enum values are
errors; a persistent protocol violation closes the tunnel:

| Direction | Message |
|---|---|
| server → gateway | `open_leg{leg_id, kind, route, account, ticket, subprotocols}` |
| server → gateway | `close_leg{leg_id, reason}` |
| server → gateway | `going_away{reason, reconnect_after_seconds}` |
| gateway → server | `leg_failed{leg_id, code}` |
| both | `ping{nonce}` / `pong{nonce}` |

`kind` is `vvws` or `vivid`. `route` is a non-secret server correlation identifier and is never
authority. `account` is `"<issuer>#<subject>"`, the authenticated account the browser belongs to,
and is matched against the gateway's `--allow-account` list when one is configured. `ticket` is a
one-use 32-byte base64url value minted by the server for this leg, and is presented on the leg
upgrade; it expires 30 seconds after minting and binds to the control tunnel that carried
`open_leg`.

For a `vivid` leg, `subprotocols` is the browser's requested Vivid WebSocket subprotocol tuple,
relayed as opaque strings in the order `vvmux.vivid.v1`, `vvmux.connection.<id>`,
`vvmux.auth.<token>`, `vvmux.kind.<n>`. The gateway validates them with the identical
exactly-once, non-empty checks it applies to the loopback listener, and rejects the leg otherwise.
Vivid bytes are never interpreted by the server.

`leg_failed` codes are `invalid_request`, `invalid_subprotocols`, `unknown_connection`,
`authentication_failed`, `capacity`, `ticket_rejected`, `upgrade_rejected`, `transport_error`, and
`closed`.

To dial a leg, the gateway connects `GET /t/v1/leg` offering exactly `vvtun.leg.v1` and
`vvtun.ticket.<ticket>`, both non-empty and each occurring once. The server consumes the ticket
atomically before any Vivid or VVWS byte is accepted. Binary frames only after the upgrade; text
frames on a leg are a protocol error. Byte zero after admission is the first byte of the browser
socket's stream.

A gateway bounds its legs: at most 32 open legs per tunnel. A leg beyond the bound is answered
with `leg_failed{code:"capacity"}`. The gateway closes a leg promptly when `close_leg` arrives,
when its browser peer closes, or when its side of the VVWS or Vivid loop ends, and it must tear
down the VVMX attachment the leg carried so that a later attach is not refused as occupied.

## Gateway behavior on tunnel loss

The gateway is a client of the same owner-only VVMX IPC as `vvmux attach`, and the hidden session
daemon is fully detached from it. Closing the tunnel, killing the gateway, or restarting the server
does not disturb a running session. A closing leg tears down its VVMX attachment before the close
completes.

## Connection states and operations

A `vvws` leg runs the full VVWS/1 state machine (list, create, attach, detach, actions). Its
`hello` uses `{"type":"hello","protocol":1,"auth":"tunnel"}` with no token; the loopback listener
rejects this form and still requires the bearer token. VVWS/1 remains protocol version 1.

The tunnel gateway advertises `tunnel-attached-v1` on its VVWS legs. It advertises
`session-kill-v1` only when started with `--allow-kill`, which additionally enables the
`kill_session` VVWS control; the default is off, matching the loopback gateway's refusal to expose
session kill.

## Enrollment API

Enrollment is a short control-plane HTTP contract, not part of VVTUN:

```text
POST /api/v1/machines/enroll
{"code":"ONE_TIME_CODE","public_key":"BASE64URL_32_BYTES"}
→ 200 {"machine_id":"BASE64URL_PUBLIC_KEY"} | 4xx {"error":"..."}
```

The server mints one-time codes and stores only hashes. Enrollment is refused for an expired,
reused, or unknown code. The gateway generates its key, submits the public key, and on success
writes the identity file; the private key never leaves the machine and never appears in argv, an
environment variable, or a log.
