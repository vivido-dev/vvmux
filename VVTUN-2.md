# VVTUN/2 hosted WebSocket contract

This version trusts HAProxy as the public TLS endpoint. The edge-to-relay hop is protected by
local isolation or mutually authenticated TLS. It does not offer end-to-end TLS-exporter binding.
An active trusted edge can forward challenges: protecting the edge is part of the deployment
trust boundary. Do not weaken VVTUN/1 or silently negotiate down to it.

Control endpoint `/t/v2/control`, subprotocol `vvtun.v2`. Data endpoint `/t/v2/leg`, subprotocol
`vvtun.leg.v2`. JSON control messages are bounded to 64 KiB. Reject unknown fields/types/versions.
Challenge and initial status each have a five-second relay deadline (client wait ten seconds).
At most 128 unauthenticated connections. Existing VVTUN/1 open_leg, close_leg, ping/pong,
machine_status, going_away and leg ticket semantics apply with version 2 names; tickets are
random 32-byte one-use values bound to owner, control generation and a ten-second expiry.
Tickets must be presented in-band on v2 data legs, not URLs, headers, or subprotocols.

## Authentication

Relay sends `{type:"challenge",protocol:2,nonce,hostname}`. `hostname` now contains the canonical
public origin (retained field name for the control codec). Canonical origin: lowercase ASCII/IDNA
host, https scheme, no path/query/fragment/credentials, omit default 443; explicit nondefault port
is retained. Loopback development may use http and omit default 80. Client derives the expected
origin from its configured deployment and compares exactly before signing; no redirects allowed.

nonce is canonical unpadded base64url of 32 cryptographically random bytes. It also identifies
this authentication connection. The exact signed transcript is:

```
ASCII("vvmux tunnel auth v2\0") || nonce[32] ||
uint16_be(origin_utf8_length) || origin_utf8 || machine_public_key[32]
```

Machine ID is canonical unpadded base64url of the Ed25519 public key. Client sends
`{type:"auth",machine_id,signature}` with the 64-byte signature in canonical base64url. Relay
verifies against that public key before asking control for enrollment/tenant/lease authorization.
Challenge accepted once on its issuing connection. Replay on another connection has a different
nonce and fails. Relay returns `{type:"authed",protocol:2,server_version,reconnect_after_seconds:0}`;
client then sends bounded machine_status. Errors close the connection, without authentication
fallback. Existing enrolled identities are unchanged.

## Metadata commands

Relay sends `{type:"session_request",request_id,operation_id,owner,account,action,name?}`; account is the immutable
issuer#subject identity, and gateway applies its existing allow-account restrictions. Actions:
list, create, result. Request and operation IDs are 32 lowercase hexadecimal characters. The request ID correlates
one transport exchange; operation_id identifies a retained mutation across exchanges. owner is
the trusted tenant/user context, scoped separately from the allow-account identity. Name is validated by
existing session APIs, at most 128 bytes. At most four requests concurrently, each result at most
64 KiB. Gateway replies `{type:"session_result",request_id,result}` where result is either the
session directory response or `{error:"..."}` with bounded nonsecret error codes. Creation is
idempotent only within the documented process lifetime and retention window; unknown outcomes
require reconciliation. Duplicate IDs with different account/action/name are rejected.

Heartbeat interval 30 seconds, three misses close the connection. Reconnection uses existing
bounded jitter and never kills local session daemons. Saturated data legs cannot block metadata,
authentication renewal, cancellation, or heartbeats. See PRIVATE-API.md for authorization expiry.

## Test vectors

Use deterministic Ed25519 test seed bytes 0..31, nonce bytes 32..63, origin
`https://vvmux.example`, and its derived public key. Generate and store transcript hex, public key,
and signature with the shared test fixture. Both implementations verify the vector. Negative
vectors change nonce, audience, public key, version domain, or origin length; all must fail.
