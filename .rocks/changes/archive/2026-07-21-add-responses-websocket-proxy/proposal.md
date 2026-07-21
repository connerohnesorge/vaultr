# Change: Add native Responses WebSocket proxying

## Why

Codex can use a persistent WebSocket transport for the Responses API, but
Plant currently rejects every WebSocket upgrade with HTTP 426. That forces a
less efficient HTTP fallback and leaves native WebSocket turns outside Plant's
capture and telemetry guarantees.

## What Changes

- Accept Codex `/responses` WebSocket upgrades and establish the corresponding
  upstream `ws://` or `wss://` connection with the request's credentials.
- Relay data, control, and close frames with bounded backpressure for the life
  of the upgraded connection.
- Normalize each sequential `response.create` request and its response event
  frames into the existing request JSON plus SSE response capture envelope,
  suppressing prewarm traffic and expanding response-id input deltas.
- Finish complete turns on `response.completed`, and retain interrupted turns
  as incomplete captures through the existing persistence and telemetry path.
- Extend Plant's self-test coverage to prove WebSocket forwarding, sequential
  turn capture, and interrupted-turn handling without changing HTTP capture.

## Impact

- Affected specs: `capture-stewardship`
- Affected code: Plant proxy, self-test, Cargo dependencies
