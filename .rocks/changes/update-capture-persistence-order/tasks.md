## 1. Ordered capture persistence

- [x] 1.1 Add the private per-session mutex and atomically reserve preparation sequence with delta-base advancement
- [x] 1.2 Atomically stage completed Envelopes outside the Session Capture and Git tree
- [x] 1.3 Drain eligible stages in preparation order using the specified append, journal-retire, and stage-delete commit order
- [x] 1.4 Preserve Session Index and Herdr snapshot side effects at durable stage acceptance

## 2. Recovery and Sealing coordination

- [x] 2.1 Recover every staged session before proxy binding or Sealing
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
