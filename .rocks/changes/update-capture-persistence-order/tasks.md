## 1. Ordered capture persistence

- [x] 1.1 Add the private per-session mutex and atomically reserve preparation sequence with delta-base advancement
- [x] 1.2 Atomically stage completed Envelopes outside the Session Capture and Git tree
- [x] 1.3 Drain eligible stages in preparation order using the specified append, journal-retire, and stage-delete commit order
- [x] 1.4 Preserve Session Index and Herdr snapshot side effects at durable stage acceptance

## 2. Recovery and Sealing coordination

- [x] 2.1 Recover every staged session after complete listener ownership and before proxy traffic or Sealing
- [x] 2.2 Materialize abandoned reservations as incomplete Envelopes and reconcile append/delete crash windows
- [x] 2.3 Repair only exact staged-prefix live tails and fail safely on journal, identity, or persisted-tail conflicts
- [x] 2.4 Prevent Sealing while a session has open reservations or staged Envelopes

## 3. Reconstruction compatibility

- [x] 3.1 Recover every complete concatenated Envelope from a terminated record
- [x] 3.2 Ignore whitespace-only records and only one unterminated final live-raw fragment
- [x] 3.3 Return location-only errors for unrecoverable terminated records and any malformed sealed tail

## 4. Verification

- [x] 4.1 Add deterministic reverse-completion coverage that asserts persisted and reconstructed preparation order
- [x] 4.2 Cover restart recovery, exact-tail idempotency, prefix repair, conflicts, legacy state, and Sealing exclusion
- [x] 4.3 Reproduce the historical `JSONJSON\n\n` shape and assert that both Envelopes reconstruct
- [x] 4.4 Run formatting, the capture-specific Plant self-test, and relevant workspace tests
- [x] 4.5 Reinspect the cited real Session Captures read-only and confirm all recoverable Envelopes are counted

## 5. Capture integrity follow-ups

- [x] 5.1 Detach immutable raw generations under the session mutex and commit each generation idempotently across every Sealing crash boundary
- [x] 5.2 Acquire and verify complete two-listener daemon ownership before recovery or scheduler startup
- [x] 5.3 Make recovery inventory current-root-only, path-exact, strict about journal/stage identity, and explicit about cleanup failures
- [x] 5.4 Reconcile abandoned incomplete Envelope appends through the existing exact-tail seam
- [x] 5.5 Cover concurrent detachment, Sealing retry, two-process ownership, strict evidence fixtures, and incomplete append retries

## 6. Acceptance-critical review fixes

- [x] 6.1 Move Journal, stage, byte-exact commit, and retained recovery state into one private persistence module
- [x] 6.2 Validate legacy and ordered journal shapes through one loader used by preparation, draining, recovery, and detachment
- [x] 6.3 Reconcile live and recovery commits through one exact-byte transaction and cover UTF-8-split prefixes, journal failure, and cleanup failure
- [x] 6.4 Centralize sealed, raw, and detached generation validation and require digest proof before omitting detached evidence
- [x] 6.5 Reject symlinked or escaping Session Capture traversal before recovery mutation
- [x] 6.6 Run scheduled compression in the listener owner, gate manual compression on both listeners, and propagate Sealing failures
- [x] 6.7 Prove graceful-drain append exclusion and scheduled failure recording with independent processes

## 7. External review blockers

- [x] 7.1 Replace bounded tail reconciliation with a typed backward classifier and cover large-prior and terminated-malformed evidence
- [x] 7.2 Carry validated generation inventories and explicit kinds through learning, coverage, and pending Sealing
- [x] 7.3 Assign compression a typed in-process action during job discovery and keep the wrapper manual-only
- [x] 7.4 Correct graceful-drain ownership documentation and rerun the full two-repository verification matrix

## 8. Crash-edge exact-tip review

- [x] 8.1 Remove only exact atomic stage-write temp debris during exclusive recovery and materialize pending reservations once
- [x] 8.2 Make tail reconciliation whitespace-aware, concatenated-value compatible, UUID-strict, chunkwise, and bounded-memory
- [x] 8.3 Use one directory-anchored no-follow raw descriptor through classification, comparison, truncation, and append
- [x] 8.4 Generalize detached exact-once Sealing to Herdr and coordinate snapshot appends with detachment
- [x] 8.5 Run focused crash and security fixtures, formatting, strict Clippy, workspace tests, self-test, and Rocks validation

## 9. Detach and Sealing boundary hardening

- [x] 9.1 Anchor Capture and Herdr scrub, detach, compression, comparison, rename, and cleanup to retained no-follow descriptors under a cooperative session-directory flock
- [x] 9.2 Reject static or pre-operation source, destination, temporary, and cleanup entry substitutions without following symlinks or mutating their targets
- [x] 9.3 Sync source or merged data and directory renames before detached cleanup, then sync the cleanup removal
- [x] 9.4 Cover pre-operation substitutions, symlinked generation paths, and durable post-rename retry while preserving legacy frame compatibility
- [x] 9.5 Rerun formatting, strict Clippy, workspace tests, self-test, Rocks validation, and final clean-tip review

## 10. Final immutable-Sealing audit

- [x] 10.1 Recover current exact UUID temps and only the five enumerated previous-version deterministic temp names under the session lock
- [x] 10.2 Prove fresh and retried committed suffixes through the canonical decoded digest, accepting alternate valid frame representations and retaining corrupt output evidence
- [x] 10.3 Kill and reap a timed-out compressor before descriptor-owned temp cleanup
- [x] 10.4 Cover exact and near-miss temp migration, corrupt-success output, alternate valid frames, and timeout reaping
