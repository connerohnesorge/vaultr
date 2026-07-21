## 1. Implementation

- [x] 1.1 Add the WebSocket transport dependency and an upgrade path limited to captured Codex `/responses` requests.
- [x] 1.2 Dial the corresponding upstream WebSocket with forwarded application credentials and relay frames bidirectionally with lifecycle-safe backpressure.
- [x] 1.3 Normalize sequential `response.create` turns, prewarms, and response-id input deltas into the existing request JSON and SSE response capture envelope, including complete and interrupted outcomes.
- [x] 1.4 Extend the end-to-end self-test and focused proxy tests for forwarding, sequential turns, control frames, and incomplete capture.
- [x] 1.5 Run the complete workspace test suite and autonomous review loop.
