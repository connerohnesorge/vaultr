## ADDED Requirements

### Requirement: Pi Codex route compatibility

Plant MUST capture Pi OpenAI Codex requests through the existing Codex adapter.
It MUST forward exact path `/codex/responses` as `/responses`.
It MUST leave native Codex path `/responses` unchanged.
It MUST retain the observed downstream path in the Envelope.

#### Scenario: Pi HTTP request

- WHEN Pi sends an HTTP request to `/codex/responses`
- THEN Plant forwards one request to upstream path `/responses`
- AND Plant records the Pi session identity

#### Scenario: Pi WebSocket request

- WHEN Pi upgrades a WebSocket at `/codex/responses`
- THEN Plant connects once to upstream path `/responses`
- AND Plant records the Pi session identity

#### Scenario: Native Codex request

- WHEN Codex sends a request to `/responses`
- THEN Plant forwards the unchanged path
