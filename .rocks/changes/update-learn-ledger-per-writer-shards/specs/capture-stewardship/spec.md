## ADDED Requirements

### Requirement: Single-writer learn-ledger shards

The learn ledger MUST be stored as one shard file per writer, addressed by the
writing host and learner, under `learnings/.ledger/`. Each shard MUST have exactly
one writer so that no two concurrent learners share a file. Shards from different
writers MUST occupy disjoint paths so independent hosts merge without conflict. A
writer MUST NOT address the ledger by a single fixed path. Newly written rows MUST
omit the `learnings` array, which no consumer reads.

#### Scenario: Two learners on one host never share a shard

- WHEN the Claude learner and the Codex learner both complete a pass on the same host
- THEN each appends to its own shard addressed by its learner
- AND neither writer can observe or replace the other's rows

#### Scenario: Two hosts merge without conflict

- WHEN two hosts each append rows to their own shards and both are committed
- THEN merging the two histories produces no conflict
- AND every row from both hosts is present after the merge

#### Scenario: A writer may not address the ledger by a fixed path

- WHEN a writer attempts to append to a single fixed ledger path
- THEN that path is not a valid shard for the writer
- AND the append is addressed to the writer's own shard instead

### Requirement: Unified learn-ledger reader

Vaultr MUST expose one learn-ledger reader that folds every shard plus the legacy
`learnings/.ledger.jsonl` into per-session, per-learner state. Plant and Vaultr
validation MUST both consume that reader rather than parsing the ledger
independently. Reading MUST be defined over an arbitrary number of shards, so that
adding or removing a writer changes no reader. Where one session has rows from
several shards for the same learner, the latest processed timestamp MUST win.

#### Scenario: Legacy rows keep counting without migration

- WHEN rows exist only in the legacy `learnings/.ledger.jsonl`
- THEN the reader folds them in alongside any shards
- AND no migration step is required for those rows to count

#### Scenario: Classification is unchanged by sharding

- WHEN the same rows are stored as shards instead of one file
- THEN stuck classification reports the same `seal-blocked`, `half-learned`,
  `unlearned`, and `sub-threshold` sets as before
- AND sealing eligibility is unchanged

#### Scenario: The latest pass wins across shards

- WHEN one session has rows for the same learner in more than one shard
- THEN the reader keeps the row with the latest processed timestamp

#### Scenario: A malformed row is reported against its shard

- WHEN any shard contains a non-empty line that is not JSON carrying a `session_id`
- THEN vault validation raises an error naming the shard that contains it
