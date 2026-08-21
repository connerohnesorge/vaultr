## ADDED Requirements

### Requirement: Native Codex Responses WebSocket capture

Plant MUST accept a valid WebSocket upgrade only for a Codex `GET /responses`
request whose path the configured adapter captures for HTTP `POST`, dial the corresponding upstream
`ws://` or `wss://` endpoint with the request's end-to-end credentials, and
relay data, control, and close frames bidirectionally with bounded
backpressure while forwarding upstream application handshake metadata. Plant
MUST omit `generate=false` prewarm exchanges from durable turn capture and MUST
expand a validated `previous_response_id` request's incremental `input` with
the prior normalized request and response items before using the existing
request-body delta encoder. For every other sequential top-level
`response.create` request, Plant MUST normalize the request JSON and upstream
JSON text events into the existing request-body and SSE response envelope, MUST
prefer fresh frame `client_metadata` over immutable upgrade metadata for turn
identity, and MUST finish that envelope as complete only when the exact top-level
`response.completed` event reaches the downstream client, and MUST finish an
active turn as transport-incomplete when the connection closes, fails, or
receives a new turn before completion. A broken response-id chain or overlapping
turn, malformed client frame, or unrecognized client application frame MUST
disable further capture on that connection rather than persist truncated or
cross-turn evidence. If a turn is active, Plant MUST finish it as
transport-incomplete before disabling capture. Capture or telemetry failure
MUST NOT interrupt otherwise valid proxy traffic. Existing HTTP Responses
proxying and non-Codex upgrade rejection MUST remain unchanged.

#### Scenario: Native WebSocket turn completes

- WHEN Codex upgrades `GET /responses` and sends a `response.create` JSON text frame
- THEN Plant relays the unchanged request and response event frames through a credentialed upstream WebSocket
- AND Plant persists one complete existing-format envelope whose request omits the transport discriminator and whose response is the equivalent SSE event stream

#### Scenario: One connection carries sequential turns

- WHEN Codex sends a second `response.create` only after the first turn's exact `response.completed` event
- THEN Plant persists each turn as a separate envelope in preparation order without closing the WebSocket
- AND each envelope uses that frame's fresh turn identity even when upgrade metadata names the earlier turn

#### Scenario: Prewarm precedes an incremental turn

- WHEN Codex completes a `generate=false` prewarm and sends a turn with the matching `previous_response_id` and only incremental `input`
- THEN Plant does not persist the prewarm as a user turn
- AND the captured turn contains the complete logical request history including prior response items

#### Scenario: Response-id chain is not trustworthy

- WHEN an incremental request does not match the response id and serialized context observed on the connection
- THEN Plant relays the frames but disables capture on that connection without persisting truncated history

#### Scenario: Turn is interrupted

- WHEN either WebSocket closes or relay fails before the active turn receives exact `response.completed`
- THEN Plant persists the active turn as transport-incomplete with every response event received before interruption

#### Scenario: Active turn receives ambiguous client data

- WHEN a malformed, unrecognized, or binary client application frame arrives before the active turn completes
- THEN Plant relays the frame unchanged, finishes the active turn as transport-incomplete, and disables capture on that connection
- AND later upstream events cannot be mixed into that turn's evidence

#### Scenario: Control and close lifecycle

- WHEN either peer sends ping, pong, or close control frames
- THEN Plant services or relays the protocol lifecycle without unbounded buffering
- AND Plant's shutdown drain owns the upgraded connection task

#### Scenario: Upstream handshake metadata

- WHEN the upstream upgrade returns Codex model, capability, catalog, or turn-state application headers
- THEN Plant forwards those headers in the downstream 101 response without copying hop-by-hop or extension-negotiation headers

#### Scenario: Upstream handshake is rejected

- WHEN the upstream rejects the WebSocket handshake with an HTTP response such as 426 or 401
- THEN Plant preserves that response's status and safe end-to-end headers so Codex can perform HTTP fallback or credential recovery
- AND Plant omits the potentially partial or transfer-encoded rejection body
- AND Plant returns 502 only for dial or protocol failures without an upstream HTTP response

#### Scenario: Capture preparation fails

- WHEN valid WebSocket traffic cannot be parsed or prepared for capture
- THEN Plant still relays that traffic without mixing its evidence into another turn

#### Scenario: Existing transport behavior remains stable

- WHEN a Codex request uses HTTP SSE or a non-Codex request attempts an upgrade
- THEN Plant preserves the existing HTTP SSE behavior and rejects the unsupported upgrade
