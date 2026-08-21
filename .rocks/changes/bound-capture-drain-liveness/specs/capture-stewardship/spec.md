# Capture Stewardship Specification

## ADDED Requirements

### Requirement: Bounded response capture liveness

Plant MUST bound how long a single response-capture tee may hold the drain head.
A capture tee that receives no upstream body bytes for a configurable idle
interval (default 300 seconds, chosen above harness streaming keep-alive cadence
and normal thinking pauses) MUST finalize its Envelope as
`response.complete=false` with no synthesized response body, stop reading the
upstream, and durably stage the Envelope in its reserved position. The idle
bound MUST measure the inter-chunk gap and reset on every received chunk, so a
stream that continues to emit events is never reaped. A clean or torn stream end
MUST continue to stage exactly as before.

#### Scenario: Upstream stream hangs without closing

- WHEN a captured response receives no upstream bytes for longer than the idle bound
- THEN the tee finalizes the Envelope with `response.complete=false` and no synthesized body
- AND the Envelope is durably staged in its reserved sequence position
- AND the reserved sequence no longer blocks draining of later sequences

#### Scenario: Slow but live stream is not reaped

- WHEN a captured response keeps emitting events with inter-chunk gaps below the idle bound
- THEN the tee never reaps it and stages the full Envelope at clean stream end

### Requirement: Periodic drain recovery

Plant MUST run its capture persistence recovery transaction periodically while
serving, not only at startup, so a stranded drain backlog on a live Session
Capture drains within a bounded interval without requiring a Plant restart. The
periodic sweep MUST reuse the startup recovery transaction and MUST NOT
synthesize an incomplete Envelope for any reservation younger than the response
capture idle bound, so it never races a still-live tee. The sweep MUST be
read-safe against concurrent capture and MUST NOT alter any sealed Session
Capture.

#### Scenario: Backlog stranded behind a reaped head drains without restart

- WHEN a live Session Capture has completed staged Envelopes blocked behind a reaped or abandoned earlier reservation
- THEN a periodic recovery sweep persists the earlier reservation as an incomplete Envelope and drains the staged completions in preparation order
- AND the drain happens within one sweep interval rather than at the next process restart

#### Scenario: Live reservation is not synthesized early

- WHEN a periodic sweep observes a reservation younger than the response capture idle bound with no completed stage
- THEN the sweep leaves that reservation open and synthesizes no Envelope for it
</content>
