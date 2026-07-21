## Implementation Details

Plant will keep its existing HTTP proxy path intact and add one concrete
WebSocket path for Codex Responses requests. Hyper performs the downstream
upgrade, `tokio-tungstenite` performs the upstream handshake, and one async
relay loop owns both streams so `send().await` naturally applies backpressure.
The relay task is registered with Plant's existing capture task tracker, making
graceful shutdown wait for it and forced shutdown cancel it.

The upstream URL is derived from the configured adapter endpoint by changing
`http` to `ws` or `https` to `wss`, then appending the downstream path and
query. End-to-end request headers, including authorization and Codex metadata,
are forwarded. Hop-by-hop and `Sec-WebSocket-*` handshake headers are generated
for the new upstream connection rather than copied. Application response
headers from the upstream handshake are forwarded to Codex while hop-by-hop and
extension-negotiation headers remain scoped to each independently terminated
connection. If the upstream rejects the handshake, Plant preserves its HTTP
status and safe headers so Codex can still select HTTP fallback or credential
recovery; only dial and protocol failures become a 502 response. Rejection
bodies are omitted because the WebSocket handshake API exposes only bytes read
past the headers, not a complete HTTP message body; forwarding those bytes could
emit a truncated body or raw chunk framing.

Each client text frame after the `GET /responses` upgrade whose top-level
`type` is `response.create` starts one
turn. Plant removes that transport discriminator before passing the remaining
JSON object to the existing Codex identity and capture preparation code. Each
upstream JSON text frame is represented as an SSE `data:` record in the capture
response body while the original WebSocket frame remains unchanged on the
wire. Exact top-level `response.completed` finishes the turn as a successful
HTTP-equivalent response; connection close or relay failure finishes an active
turn as transport-incomplete. A second request before completion or any
unrecognized application data disables capture for the desynchronized
connection and closes an older active capture as incomplete, preventing
ambiguous or late events from mixing across turns.

Codex WebSocket reuse first sends a `generate=false` prewarm and may then send
only incremental `input` with `previous_response_id`. Plant does not persist
prewarm as a user turn. It retains the last normalized request and response
items as compact serialized JSON, validates the response-id chain, and expands
the next incremental request to the complete logical request before calling the
existing delta encoder. A missing or mismatched chain disables capture for the
connection instead of persisting truncated history; relay remains unaffected.
Turn identity is resolved from each frame's fresh `client_metadata` before the
immutable upgrade headers, because Codex can reuse one connection across
turn-scoped clients.

## Context

The Codex Responses WebSocket protocol keeps one connection alive across
sequential `response.create` turns and emits one response event per JSON text
frame. Plant's durable schema already models one request and one SSE event
stream, so transport normalization avoids a second capture schema and keeps
reconstruction and telemetry shared with HTTP.

## Goals / Non-Goals

- Goals: native Codex Responses upgrades, transparent relay, existing-envelope
  capture parity, sequential turns, complete shutdown ownership.
- Non-Goals: a generic WebSocket reverse proxy, multiplexed overlapping turns,
  a new envelope schema, or changing Codex's HTTP fallback behavior.

## Decisions

- Use one owned select loop instead of split forwarding tasks so turn state and
  close ordering have one owner.
- Preserve all data frames on the wire; only the tee is normalized.
- Treat malformed or non-`response.create` client data as uncaptured protocol
  traffic rather than blocking Codex, preserving Plant's capture-failure-is-
  non-fatal rule.
- Keep only compact serialized request and response-item context between turns;
  JSON DOM lifetime remains bounded by the shared parse gate.

## ADRs

### ADR-0001: Normalize WebSocket turns into existing capture envelopes

Adding a transport-specific envelope would permanently split reconstruction,
telemetry, scrubbing, and coverage semantics. Plant will instead represent a
WebSocket `response.create` turn as the same request JSON and SSE response body
used by HTTP, while recording the semantic response status as 200 and expanding
response-id deltas into complete logical history. This loses frame-boundary
metadata in durable evidence but preserves all event JSON and keeps every
downstream consumer transport-independent.

## Risks / Trade-offs

- Protocol drift could introduce a different turn discriminator or terminal
  event; focused tests pin the current Codex protocol and unknown frames still
  pass through unchanged.
- A long-lived connection participates in Plant shutdown; the existing drain
  timeout bounds how long graceful shutdown waits before cancellation.
- WebSocket compression extensions are not copied between independent
  handshakes, avoiding negotiated-extension mismatch at the cost of no
  per-message compression in the proxy leg.

## Migration Plan

No stored data migration is required. Deploying the new Plant binary makes
native WebSocket capture available; rollback restores the prior HTTP fallback.

## Open Questions

- None.
