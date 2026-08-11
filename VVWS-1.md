# VVWS/1: vvmux WebSocket session protocol

VVWS/1 is the public, renderer-neutral protocol for attaching a browser terminal to sessions owned
by vvmux. It is distinct from the private, same-user VVMX IPC protocol. VVWS/1 carries terminal
text and session control; its `vivid-bridge-v1` capability brokers Vivid 1.5 Control and Track
connections over separate binary WebSockets. Vivid records are not translated into VVWS JSON.

## Transport and authentication

The endpoint is `GET /v1/ws` over WebSocket with the `vvmux.v1` subprotocol. The server accepts
only exact configured `Origin` values. Missing origins, `null`, wildcard origins, and a missing
subprotocol are rejected during the HTTP upgrade.

The first WebSocket message must arrive within five seconds and must be a UTF-8 text frame:

```json
{"type":"hello","protocol":1,"token":"BASE64URL_TOKEN"}
```

No session information is available before this succeeds. The token is 32 random bytes encoded as
unpadded base64url. Create or rotate it with:

```sh
vvmux token create
vvmux token create --rotate
```

The raw token is printed once. vvmux stores only a domain-separated SHA-256 hash in an owner-only
record and compares hashes in constant time. A token grants shell access to every vvmux session
owned by the gateway's OS user.

### Tunnel-asserted authentication

A `vvmux serve --connect` gateway serves its sessions through outbound VVTUN/1 tunnel legs
([VVTUN-1.md](VVTUN-1.md)) instead of the loopback listener. On such a leg there is no bearer token,
and inventing one would recreate the "one token equals shell access to everything" property the
tunnel exists to avoid. The first message on a tunnel leg is therefore:

```json
{"type":"hello","protocol":1,"auth":"tunnel"}
```

`auth` is `"tunnel"` and the `token` field must be absent. The loopback listener rejects this form
and continues to require the bearer token; the two authentication modes are mutually exclusive and
neither is accepted on the other's transport. The connection identity is asserted by the tunnel
handshake itself and by the account the server presented in the leg's `open_leg` frame, never by
anything the browser sends.

A tunnel leg's `hello` reply advertises the additional capability `tunnel-attached-v1`. When the
gateway was started with `--allow-kill`, it also advertises `session-kill-v1` and accepts the
`kill_session` control; the default is off, matching the loopback gateway's refusal to expose
session kill. VVWS/1 remains protocol version 1 for both transports.

On success, the server replies:

```json
{
  "type":"hello",
  "protocol":1,
  "server_version":"0.1.2",
  "capabilities":[
    "terminal-v1",
    "session-list-v1",
    "session-create-v1",
    "vivid-bridge-v1"
  ],
  "vivid": {
    "endpoint":"/v1/vivid",
    "subprotocol":"vvmux.vivid.v1",
    "wire_version":"1.5",
    "connection":"EPHEMERAL_CONNECTION_ID",
    "token":"EPHEMERAL_BASE64URL_TOKEN"
  }
}
```

The Vivid access values are scoped to this authenticated VVWS connection, disappear when it
closes, and must not be logged or placed in a URL. The decoded ephemeral 32-byte route token is
also the Vivid 1.5 root secret. A browser opens the advertised endpoint with
four WebSocket subprotocol values: `vvmux.vivid.v1`, `vvmux.connection.<connection>`,
`vvmux.auth.<token>`, and `vvmux.kind.<kind>`. Kind is Control (`0`) or Track (`2`); Lane (`1`) and
all other values are rejected. The gateway selects only `vvmux.vivid.v1` in the upgrade response.
Each resulting WebSocket is one
ordinary binary Vivid connection; the producer-side preface is the first bytes sent by vvmux.

VVWS/1 has no direct-network TLS mode. `vvmux serve` binds only an IPv4 or IPv6 loopback address;
remote deployments use an SSH tunnel or a trusted TLS reverse proxy.

### Private IPC compatibility

VVWS/1 remains protocol version 1. Its server-side session adapter uses private VVMX version 12,
which is a hard cutover from every earlier VVMX version. VVMX 7 moved render and media byte
payloads out of JSON into bounded binary records. VVMX 8 added transport-loss recovery without
inventing a new producer epoch. VVMX 9 added bridge-instance correlation and bounded,
metadata-only media recovery traces. VVMX 10 is the hard Vivid 1.5 surface/track/channel cutover.
VVMX 11 reports host terminal focus as its own client message instead of pane input.
VVMX 12 distinguishes cell and pixel mouse reports and mirrors the focused pane's Kitty keyboard
mode flags into native host terminals. Media snapshots preserve live/timed plus active-slot state
for relayed tracks. It remains a hard cutover: mixed client/server versions are rejected.

Pane media inspection reports separate `virtual_projection_revision`,
`virtual_scene_revision`, `outer_projection_revision` (the monotonic compatibility sequence),
`outer_apply_sequence`, `bridge_instance_id`, and `bridge_local_revision`. `surfaces` and `tracks`
are reported separately. Track entries include complete session/context/surface/track identity,
kind/lifecycle, revision, epoch, explicitly independent inner/outer channel generations,
milestones, queue utilization, and cumulative flow availability. Relay/bridge counters are
diagnostic only; no inner identity or counter is outer-hop authority.

The binary forms carry terminal frame bytes and Vivid media bodies only. They never enter VVWS
JSON control messages and never contain root secrets, channel keys, authenticators, payload
hashes, or derived capability material. Gateway/session processes from different VVMX versions
must reject one another and the
older session must be restarted.

## Connection states and terminal data

After authentication a connection is detached. It may list or create sessions and attach to one
session. While attached, management requests are rejected until it detaches.

Client-to-server binary frames are terminal input. Server-to-client binary frames are ordered ANSI
terminal output for direct delivery to an xterm.js terminal. Binary input is accepted only while
attached. vvmux sends its terminal-mode initialization immediately after `attached` and its
restoration sequence before `detached`.

The gateway must not drop ANSI output frames: later frames may be diffs against earlier output. If
the bounded outbound queue fills, it cancels only that VVMX attachment and closes the WebSocket
with code 1013. A later attachment receives a new full render.

JSON control frames and terminal input are limited to 64 KiB. A WebSocket frame or assembled
message is limited to 1 MiB.

## Client controls

Every JSON object is strict: unknown fields and unknown enum values are errors.

List live sessions:

```json
{"type":"list_sessions","request_id":1}
```

Create a detached session using the gateway's vvmux configuration:

```json
{"type":"create_session","request_id":2,"name":"work"}
```

Terminate a session and all of its panes (tunnel gateways with `--allow-kill` only, and never the
loopback listener):

```json
{"type":"kill_session","request_id":3,"name":"work"}
```

Attach to an existing session:

```json
{
  "type":"attach",
  "request_id":3,
  "name":"work",
  "display":{"columns":120,"rows":40,"cell_width":9,"cell_height":18},
  "takeover":false,
  "vivid":true
}
```

`columns` must be 10–1000 and `rows` 4–500. Cell dimensions may be zero when unknown and otherwise
must not exceed 4096. Only one vvmux client controls a session. An occupied session returns
`session_occupied`; setting `takeover` to true cleanly detaches the old client before admission.
`vivid` defaults to false for generic xterm.js clients. When true, the client must concurrently
open the kind-0 Vivid WebSocket so the normal Vivid HELLO/WELCOME handshake can finish. Media
connections use the same endpoint and credentials with the connection kind requested by Vivid.

Resize and detach:

```json
{"type":"resize","display":{"columns":100,"rows":30,"cell_width":9,"cell_height":18}}
{"type":"detach"}
```

Mux actions use a nested tagged object:

```json
{"type":"action","action":{"name":"split","axis":"vertical"}}
{"type":"action","action":{"name":"focus","direction":"left"}}
{"type":"action","action":{"name":"select_tab","index":2}}
```

Action names are `split`, `focus`, `resize`, `new_tab`, `next_tab`, `previous_tab`, `select_tab`,
`close_pane`, `toggle_zoom`, `toggle_sync_input`, `enter_copy_mode`, `copy_input`, `paste`, `new_floating_pane`,
`toggle_floating_panes`, `toggle_pane_pinned`, `enter_floating_move_mode`, and
`enter_floating_resize_mode`. Axes are `horizontal` or `vertical`; directions are `left`, `right`,
`up`, or `down`. `copy_input` carries a JSON byte array named `bytes`.

Raw terminal input is passed through the same prefix, mouse, close-confirmation, and floating-edit
state machine as the native vvmux client, so normal `Ctrl-b` bindings work without action frames.

## Server controls

Successful correlated replies are:

```json
{"type":"sessions","request_id":1,"sessions":[{"name":"work","pid":1234}]}
{"type":"created","request_id":2,"name":"work"}
{"type":"killed","request_id":3,"name":"work"}
{"type":"attached","request_id":4,"name":"work","text_only":false}
```

Uncorrelated terminal events are:

```json
{"type":"title","title":"editor — vvmux"}
{"type":"bell"}
{"type":"clipboard","text":"copied text"}
{"type":"status","message":"status text"}
{"type":"floating_edit_state","mode_id":4,"active":true,"pane":2,"kind":"move"}
{"type":"detached","reason":"detached"}
```

Clipboard events do not write the browser clipboard automatically; the client decides whether and
when to request browser permission.

Errors have a stable machine code and an optional correlated request ID:

```json
{"type":"error","request_id":3,"code":"session_occupied","message":"..."}
```

VVWS/1 codes are `invalid_request`, `invalid_state`, `invalid_session`, `invalid_display`,
`invalid_action`, `input_too_large`, `session_exists`, `session_not_found`, `session_occupied`,
`session_unavailable`, and `session_error`. `vivid_unavailable` reports a failed Vivid transport or
handshake for an otherwise valid attach request. Authentication and upgrade failures close the
connection without exposing session state.

WebSocket ping frames receive pong frames with the same payload. Application-level ping messages
are not part of VVWS/1.

## Minimal xterm.js integration

The browser application opens a WebSocket with `vvmux.v1`, sends `hello`, then `attach`. Set
`binaryType = "arraybuffer"`; pass received binary data to `terminal.write`, encode `terminal.onData`
strings with `TextEncoder`, and send `resize` from the fit/resize handler. Title, bell, clipboard,
status, errors, and lifecycle events are handled from JSON text frames. vvmux does not serve HTML,
JavaScript, or other client assets.

The vvmux_server web frontend (`vvmux_server/web`) is the maintained browser coordinator: it owns
the VVWS hello, the advertised Vivid WebSockets or WebTransport session, and attach/reattach. Over
a public tunnel leg it uses the `auth: "tunnel"` form above; the loopback development path may use
the bearer token form. Text-only clients send the same JSON hello and ignore the `vivid` block.
The retired vivido.js `connectVvmux` API is gone with its submodule; no client asset is served by
this gateway.
