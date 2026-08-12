## 1. Reconstruction

- [ ] 1.1 Retain observed message union during reconstruction.
- [ ] 1.2 Mark final-replay membership during reconstruction.
- [ ] 1.3 Recover usable history after lineage breaks.
- [ ] 1.4 Materialize scrubbed captures from `state.json` during sealing.
- [ ] 1.5 Add reconstruction fixtures for both harnesses.

## 2. Session index

- [ ] 2.1 Add the Tantivy dependency.
- [ ] 2.2 Define versioned session index metadata.
- [ ] 2.3 Build derived turn documents from normalized blocks.
- [ ] 2.4 Store session metadata with turn documents.
- [ ] 2.5 Limit stored tool output.
- [ ] 2.6 Implement fingerprint-based session replacement.
- [ ] 2.7 Implement schema-mismatch rebuilding.
- [ ] 2.8 Parallelize capture decoding with configurable workers.

## 3. Curated index

- [ ] 3.1 Define the curated corpus boundary.
- [ ] 3.2 Index included Markdown files.
- [ ] 3.3 Index orphan prompt sidecars.
- [ ] 3.4 Exclude readable-capture prompt sidecars.

## 4. Query interface

- [ ] 4.1 Add the `session index` command.
- [ ] 4.2 Add the `session search` command.
- [ ] 4.3 Parse literal and field queries.
- [ ] 4.4 Collapse repeated content-hash results.
- [ ] 4.5 Render human-readable search results.
- [ ] 4.6 Emit the JSON search envelope.
- [ ] 4.7 Report unavailable and stale index states.

## 5. Integrations and validation

- [ ] 5.1 Add the shared five-minute Plant index job.
- [ ] 5.2 Add the Herdr search action.
- [ ] 5.3 Update `/Recall` search guidance.
- [ ] 5.4 Add search command tests.
- [ ] 5.5 Run the Vaultr test suite.
