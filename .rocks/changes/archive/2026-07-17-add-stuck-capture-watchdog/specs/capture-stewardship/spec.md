# Capture Stewardship — Delta

## ADDED Requirements

### Requirement: Stuck Session Capture detection

Plant MUST classify every raw (unsealed) Session Capture idle for at least a
configurable age (default 24 hours) by its learn-ledger state: `seal-blocked`
when every learner has ledgered it yet it remains unsealed, `half-learned`
naming the missing learner when only a strict subset of learners ledgered it,
`unlearned` when no learner ledgered it and it passes the learn substance
gate, and `sub-threshold` when no learner ledgered it and it falls below the
substance gate. Detection MUST be read-only against Session Captures.

#### Scenario: Sealing is failing on a fully learned capture

- WHEN a raw Session Capture idle beyond the age threshold is ledgered by every learner
- THEN it is reported as `seal-blocked`

#### Scenario: One learner never processed a capture

- WHEN a raw Session Capture idle beyond the age threshold is ledgered by only one learner
- THEN it is reported as `half-learned` naming the missing learner

#### Scenario: Learn never picked up a substantive capture

- WHEN a raw Session Capture idle beyond the age threshold has no ledger entry and passes the substance gate
- THEN it is reported as `unlearned`

#### Scenario: Capture below the substance gate

- WHEN a raw Session Capture idle beyond the age threshold has no ledger entry and falls below the substance gate
- THEN it is reported as `sub-threshold`

#### Scenario: Active and sealed captures are exempt

- WHEN a raw Session Capture was modified within the age threshold, or a Session Capture is already sealed
- THEN it is not reported

### Requirement: Watchdog Cultivation Job

Plant MUST run an in-process `watchdog` job every 6 hours that performs stuck
detection and records the outcome: `failed` with per-state counts when any
`seal-blocked`, `half-learned`, or `unlearned` capture exists, otherwise
`success`. Sub-threshold captures MUST be counted in the recorded detail but
MUST NOT fail the job. The job launches no agent pane and writes nothing to
Session Captures.

#### Scenario: Actionable stuck captures exist

- WHEN detection finds at least one seal-blocked, half-learned, or unlearned capture
- THEN the job logs one line per stuck capture and records `failed` with per-state counts

#### Scenario: Only sub-threshold captures exist

- WHEN detection finds only sub-threshold captures
- THEN the job records `success` with the sub-threshold count in the detail

#### Scenario: Pipeline is healthy

- WHEN detection finds nothing stuck
- THEN the job records `success`

### Requirement: Manual stuck inspection subcommand

`plant sessions stuck [--age <duration>]` MUST print one line per stuck
capture (state, session id, idle time) using the same classification as the
watchdog job, and MUST exit 0 when no actionable stuck captures exist and 1
when any actionable capture exists.

#### Scenario: Operator inspects a stuck pipeline

- WHEN `plant sessions stuck` runs against a vault with actionable stuck captures
- THEN each stuck capture prints with its state and idle time and the process exits 1

#### Scenario: Operator inspects a healthy pipeline

- WHEN `plant sessions stuck` runs against a vault with no actionable stuck captures
- THEN the process exits 0
