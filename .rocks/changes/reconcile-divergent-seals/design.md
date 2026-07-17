# Design — Reconcile Divergent Seals

Two layers because neither suffices alone: reopen-on-resume stops new
divergence at the source but loses the race when a seal lands mid-resume (and
does nothing for the 25 dirs already forked); the doctor pass repairs any
double-file dir regardless of how it got there. Both converge on the same
invariant: a session dir holds exactly one capture file per stream, raw while
open, sealed once learned and idle.

Repair targets the raw side, not the seal: the doctor merges into
`turns.jsonl` and deletes the stale `.zst`, then lets the ordinary
scrub+seal+commit path re-seal. This reuses the only code trusted to write
seals, keeps the doctor idempotent (a crash mid-repair leaves the dir in the
double-file state it already handles), and re-scrubs the merged whole — the
post-resume epoch never went through scrub.

## ADRs

### ADR-0001: Merge strategy for divergent seal epochs

Divergent dirs are merged by rule, in order: byte-identical → drop the raw
duplicate; one side a line-prefix of the other → keep the superset; otherwise
concatenate seal-content then raw-content. The fresh-epoch capture model
(append to a file that no longer exists) means the raw is the *later* epoch,
proven live by 14 dirs where raw < seal. Concatenation preserves chronology;
prefix handling covers unseal-then-append and partial-seal races. Because a
wrong merge is unrecoverable, the stale seal is removed only after verifying
the merged file's line count covers both parts — and the old seal's bytes stay
recoverable from the vault repo's seal commit either way.

### ADR-0002: Reopen-on-resume is best-effort, never fatal

Unsealing in the capture path (proxy stream end) must not block or fail the
envelope write: on any unseal error Plant writes the fresh epoch exactly as
today and the doctor reconciles later. Project constraint "keep Plant failure
paths non-fatal where capture uptime is at stake" — a dropped envelope is
permanent, a forked epoch is now repairable.
