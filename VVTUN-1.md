# VVTUN/1: vvmux outbound machine tunnel protocol

VVTUN/1 is the public protocol by which a machine's `vvmux serve --connect` gateway reaches a
vvmux_server deployment and stays reachable for browser attach, without opening any inbound port
on the machine. The gateway dials out; the server never dials in.

## Transport and authentication

VVTUN/1 has two behavior-identical reliable carrier mappings. The preferred mapping is one
WebTransport session at `CONNECT /t/v1/webtransport`, protocol `vvtun.v1`, containing one
server-opened bidirectional control stream and one server-opened independent bidirectional stream
per data leg. The fallback is a control WebSocket at `GET /t/v1/control`, subprotocol `vvtun.v1`,
plus one data-leg WebSocket at `GET /t/v1/leg` offering `vvtun.leg.v1` and
`vvtun.ticket.<BASE64URL_TICKET>`.

`--tunnel-carrier auto` tries WebTransport first and may fall back only before authentication;
`webtransport` and `websocket` force a mapping. An explicit `wss://.../t/v1/control` URL forces
WebSocket. A tunnel must never mix a WebSocket control with WebTransport legs or the reverse.

TLS is mandatory across a host boundary. The canonical CLI input is a bare `https://host[:port]`
deployment base, from which the gateway derives the HTTP/3 and `wss://` endpoints. An exact
`wss://host[:port]/t/v1/control` URL explicitly forces this mapping. Plain `http://` or `ws://` is
accepted for loopback addresses only, for development and testing. Connect URLs reject credentials,
queries, fragments, and unexpected paths. A public connect also requires the explicit
`--acknowledge-content-visible-gateway` flag because the relay can observe terminal and media bytes.
A loopback plaintext tunnel has no TLS exporter and its handshake signature covers an empty
exporter. WebTransport always has TLS and a 32-byte exporter.

The gateway connects with its enrolled identity: an Ed25519 key pair stored in an owner-only file
(`cloud-identity.json`). Only the public key leaves the machine.

### Challenge and proof

After carrier establishment, the server sends the first strict JSON object (a WebSocket text frame
or a length-prefixed object on the WebTransport control stream):

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

Exporter derivation is mandatory on every TLS tunnel. If rustls cannot derive exactly 32 bytes, the
gateway fails the connection; it must never substitute the loopback empty binding on `wss://`.

Because `hostname` and `exporter` are inside the signed payload, a signature captured from one
deployment or one TLS session cannot be replayed to another.

On success the server replies:

```json
{"type":"authed","protocol":1,"server_version":"0.1.0","reconnect_after_seconds":0}
```

`reconnect_after_seconds` is a scheduling hint for a planned drain; it is never an order.

Both the initial `challenge` wait and the subsequent `authed` wait have a 10-second deadline. A peer
cannot retain an unauthenticated gateway task by completing the upgrade and then remaining silent.

### WebTransport control framing

Every control object is a `u32` big-endian byte length followed by exactly that many UTF-8 JSON
bytes. The 64 KiB ceiling is checked before allocation. The control stream has the highest
transport priority. EOF inside a prefix or object, invalid UTF-8/JSON, an unknown field, or an
oversized object closes the tunnel. A WebTransport gateway advertises
`webtransport-streams-v1` and `stream-priority-v1` in `machine_status`.

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

The **socket layer** on the fallback: the server sends WebSocket ping frames every 30 seconds. The gateway answers
pong frames with the same payload. This keeps NAT and proxy mappings warm and proves the socket
lives. On WebTransport, QUIC keepalive and connection-close events provide this layer.

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
`Retry-After`, the gateway parses either delta-seconds or an HTTP date and treats it as its next
delay, capped at 60 seconds, rather than as a generic failure.

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

On the fallback mapping, the gateway connects `GET /t/v1/leg` offering exactly `vvtun.leg.v1` and
`vvtun.ticket.<ticket>`, both non-empty and each occurring once. The server consumes the ticket
atomically before any Vivid or VVWS byte is accepted. Binary frames only after the upgrade; text
frames on a leg are a protocol error. Byte zero after admission is the first byte of the browser
socket's stream.

On WebTransport, the server sends `open_leg` and opens one reliable bidirectional stream. The
stream starts with this fixed 64-byte network-order preface, stripped before endpoint bytes:

```text
0..8    "VVTLEG1\0"
8..16   authenticated tunnel generation, u64
16..24  leg_id, u64
24      kind: 1 VVWS, 2 Vivid
25..29  stream priority, i32
29..32  zero reserved bytes
32..64  raw 32-byte one-use leg ticket
```

The preface and `open_leg` must agree. Mismatch, duplicate, unknown kind, nonzero reserved bytes,
mixed mapping, or an unpaired stream is reset. Pairing is bounded and expires within 30 seconds.

After the preface, Vivid is raw bytes. VVWS uses
`kind:u8 || length:u32be || payload`, with kinds text=1, binary=2, ping=3, pong=4, close=5. Payload
is at most 64 KiB, text is UTF-8, and close has zero length. Unknown kinds or malformed frames close
the leg. Priorities are control/VVWS, realtime/audio, then bulk. Datagrams are forbidden.

A gateway bounds its legs: at most 32 open legs per tunnel. A leg beyond the bound is answered
with `leg_failed{code:"capacity"}`. A `leg_id` is unique for the lifetime of one authenticated tunnel
generation; any reuse is rejected with `leg_failed{code:"invalid_request"}` without replacing or
cancelling the original. The same numeric ID in another generation is independent. The gateway
closes a leg promptly when `close_leg` arrives,
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
reused, or unknown code. Before reading or redeeming a code, the gateway race-safely reserves the
owner-only identity destination. It reads the code from a no-echo prompt or explicit bounded
file/stdin source, never argv or the environment. The gateway generates its key, submits the public
key, verifies that the returned `machine_id` is exactly that public key, and then commits the
reserved identity file. The enrollment code and private key never leave their intended channels or
appear in argv, an environment variable, or a log; transient private-key buffers are zeroized.
