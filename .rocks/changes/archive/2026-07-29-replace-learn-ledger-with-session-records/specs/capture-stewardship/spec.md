## ADDED Requirements

### Requirement: Immutable per-pass learn records

Each completed learn pass MUST be recorded as its own file inside that session's own
capture directory, named for the learner, the writing host, and the pass timestamp. The
learner and writing host MUST be derivable from the filename, and a record MUST NOT
restate them in its content, so a record cannot contradict its own location. A learn
record MUST be created only when absent and MUST never be modified, replaced, or
truncated afterward. Recording a further pass for a session and learner MUST be
permitted without exception, so a resumed capture records a new pass rather than
overwriting a prior one. A filename that does not name a known learner MUST be rejected
rather than counted.

#### Scenario: A resumed capture records another pass

- WHEN a learner processes a session it has already processed
- THEN a new learn record is created for that pass
- AND every earlier record for that session remains byte-for-byte unchanged

#### Scenario: A record cannot contradict its location

- WHEN a learn record is read
- THEN its learner and writing host come from its filename
- AND no content field can disagree with them

#### Scenario: Two hosts record the same session

- WHEN two hosts each record a pass for the same session and learner
- THEN each writes a separate file named for its own host
- AND merging the two histories produces no conflict

#### Scenario: An unknown learner is rejected

- WHEN a file in a session directory is named for a learner that is not recognised
- THEN it is reported as invalid
- AND it is not counted as a pass for any learner

### Requirement: Unified learn-state reader

Vaultr MUST expose one reader that folds per-pass learn records together with the frozen
legacy ledger into per-session, per-learner state, the latest pass winning for each
learner. Plant and Vaultr validation MUST both consume that reader instead of parsing
learn state independently. Learn records MUST be read during the session-directory walk
already performed for capture-generation inventory rather than as a separate traversal.
Legacy rows MUST keep counting without migration, and a legacy row carrying no learner
MUST count as the Claude learner.

#### Scenario: Legacy rows keep counting without migration

- WHEN a session's only learn state is legacy ledger rows
- THEN the reader folds them in
- AND no migration is required for those rows to count

#### Scenario: A newer pass supersedes a legacy row

- WHEN a session has both a legacy row and a newer per-pass record for one learner
- THEN the later pass wins for that learner

#### Scenario: Classification is unchanged by the new storage

- WHEN the same learn state is stored as per-pass records instead of ledger rows
- THEN stuck classification reports the same `seal-blocked`, `half-learned`,
  `unlearned`, and `sub-threshold` sets as the legacy reader over the same content
- AND sealing eligibility is unchanged

#### Scenario: A malformed record is reported against its path

- WHEN a learn record is not JSON carrying a readable outcome and timestamp
- THEN vault validation raises an error naming that record's path

## MODIFIED Requirements

### Requirement: Stuck Session Capture detection

Plant MUST classify every pending unsealed Session Capture generation idle for
at least a configurable age (default 24 hours) by its ownership and
learn state: `job-capture` when Plant registered it as a Cultivation Job
self-capture, `seal-blocked` when every learner has recorded a pass over the selected
generation yet it remains unsealed, `half-learned` naming the missing learner
when only a strict subset of learners recorded the selected generation,
`unlearned` when no learner recorded it and it passes the learn substance gate,
and `sub-threshold` when no learner recorded it and it falls below the substance
gate. A resumed raw generation beside older sealed or detached evidence MUST
count a learner only when its latest learn record postdates that prior
generation boundary. Detection MUST be read-only against Session Captures and
MUST propagate maintenance inventory failures rather than returning a partial
classification.

#### Scenario: Sealing is failing on a fully learned capture

- WHEN a pending Session Capture generation idle beyond the age threshold is recorded by every learner
- THEN it is reported as `seal-blocked`

#### Scenario: One learner never processed a capture

- WHEN a pending Session Capture generation idle beyond the age threshold is recorded by only one learner
- THEN it is reported as `half-learned` naming the missing learner

#### Scenario: Learn never picked up a substantive capture

- WHEN a pending Session Capture generation idle beyond the age threshold has no learn record and passes the substance gate
- THEN it is reported as `unlearned`

#### Scenario: Capture below the substance gate

- WHEN a pending Session Capture generation idle beyond the age threshold has no learn record and falls below the substance gate
- THEN it is reported as `sub-threshold`

#### Scenario: Cultivation Job self-capture is informational

- WHEN an idle pending Session Capture is registered as a Plant Cultivation Job self-capture
- THEN it is reported as `job-capture`
- AND it is not actionable for learn dispatch or watchdog failure
