# Bound capture drain liveness — design

## Why the gap is structural, not incidental

`capture-stewardship` ADR-0001 already accepts that "a slow earlier response may
block later draining." That is correct and intended: preparation order is
required because delta bases advance at reservation time. The unstated
assumption was that a blocked head *eventually* completes or that startup
recovery clears it. Neither holds for a long-lived session whose head sequence
is a permanently hung stream and whose Plant process never restarts.

So the deepening is not "remove ordering" — ordering must stay. It is: **bound
how long the head may block, and drain the backlog without requiring a restart.**

## Change 1 — bounded tee liveness (proxy.rs)

The capture task loop currently selects over `tx.closed()` and
`upstream_stream.next()`. Add a third arm: a reset-on-activity idle timer. On
each received chunk the timer resets; if it fires with no chunk, treat the
stream as torn (`complete=false`), stop pulling upstream, and fall through to the
existing `Stage::publish` path. The forwarded client body simply ends — the
client already abandoned this connection in the retry case.

- The bound is on **idle** (inter-chunk gap), not total duration, so legitimate
  long generations and thinking pauses are unaffected as long as the stream
  emits periodic events (Anthropic and Codex both send periodic SSE pings).
- Default 300s, configurable. This is deliberately generous: the goal is to
  reap dead connections, not to police slow ones.

## Change 2 — periodic drain recovery (main.rs)

Add a `tokio::spawn` loop alongside the otel-flush loop:

```
loop { sleep(interval); capture::recover_all(&vault); }
```

`recover_all` already performs the correct transaction: synthesize any reserved
sequence with no completed stage as an incomplete Envelope and interleave staged
completions in order. The one new constraint for mid-session (vs startup) use:
it must not synthesize a reservation that a live tee task may still complete.
With Change 1 in place every tee self-resolves within the idle bound, so a
staleness guard of "reservation older than the tee idle bound" makes synthesis
safe. Sealing eligibility is unchanged — it already refuses a generation with an
open reservation or completed stage.

Interval: a few minutes. This bounds worst-case stranding from "session
lifetime" to "one sweep interval."

## ADRs

### ADR-0003: Bound head-of-line blocking with an idle tee reap plus periodic recovery

Preparation-ordered persistence is retained (ADR-0001). Head-of-line blocking is
bounded at two layers instead of removed: the response tee reaps an
idle-beyond-threshold upstream stream into an incomplete Envelope so no single
broken connection blocks a session indefinitely, and recovery runs periodically
so a stranded drain backlog on a live Session Capture clears within a bounded
interval rather than only at process restart. Periodic recovery synthesizes only
reservations older than the tee idle bound, so it never races a live tee. This
trades a bounded number of `complete=false` Envelopes (a genuinely dead stream
has no more body to capture anyway) for guaranteed drain progress on long-lived
sessions.
</content>
