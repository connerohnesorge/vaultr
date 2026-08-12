## ADDED Requirements

### Requirement: Complete searchable session reconstruction

Vaultr MUST reconstruct searchable text from user messages, assistant messages,
tool uses, and tool results. Vaultr MUST exclude system, developer, image, and
unavailable reasoning content. Reconstruction MUST retain the union of observed
messages and mark messages absent from the final replay. A broken delta lineage
MUST recover usable history and mark the result partial.

#### Scenario: A tool result is captured

- WHEN a captured turn contains a tool result
- THEN the turn body contains the tool result text
- AND `session show` preserves its existing rendered transcript contract

#### Scenario: A message is absent from the final replay

- WHEN a message appears in an earlier observed history but not the final replay
- THEN the index includes the message
- AND the result identifies the turn as compacted

#### Scenario: A delta lineage breaks

- WHEN reconstruction cannot apply one history delta
- THEN indexing continues with recoverable observed messages
- AND the indexed session is marked partial

### Requirement: Local session index lifecycle

`vaultr session index --update` MUST create and update a local session index.
The index MUST reside under `~/.local/state/vaultr/`. The index MUST use source
fingerprints for change detection. The index MUST replace every document for a
session when its source changes. `--rebuild` MUST delete and recreate the index.
A schema-version mismatch MUST rebuild the index.

#### Scenario: An index does not exist

- WHEN `vaultr session index --update` runs without a local index
- THEN it builds the session index
- AND it reports the completed build

#### Scenario: A raw capture seals

- WHEN a raw capture becomes a sealed capture
- THEN the next update removes the session's prior documents
- AND the update adds documents from the sealed capture

#### Scenario: The schema changes

- WHEN local index metadata has a different schema version
- THEN the update removes the incompatible index
- AND the update builds a compatible index

### Requirement: Derived turn documents

The session index MUST create one document for each derived turn. A user message
with text MUST start a turn. Each document MUST use the session ID and a SHA-256
of canonical blocks as its identity. Each document MUST store its body, turn
index, session ID, harness, source timestamps, partial markers, final-replay
membership, and available session metadata. Tool output MUST be limited to 4,000
characters with a 3,000-character head and a 1,000-character tail.

#### Scenario: A tool result exceeds the limit

- WHEN a tool result exceeds 4,000 characters
- THEN the indexed body contains its first 3,000 characters
- AND the indexed body contains its last 1,000 characters

#### Scenario: A session field is unavailable

- WHEN a session lacks metadata for a searchable filter
- THEN indexing retains the turn
- AND query output reports the filter coverage

### Requirement: Lexical query behavior

`vaultr session search` MUST search sessions by default. It MUST support phrase
queries and schema-field queries. It MUST interpret an unknown field prefix as
literal text. It MUST require all unqualified query terms by default. It MUST
collapse repeated content hashes by default. `--no-collapse` MUST return every
matching document. `--final-only` MUST exclude compacted turns.

#### Scenario: A pasted colon string is not a field query

- WHEN a query has a colon prefix that is not a schema field
- THEN search treats the colon string as literal query text
- AND search does not report an unknown-field parser error

#### Scenario: Repeated content matches

- WHEN identical content matches in multiple turns
- THEN default output shows one representative hit with its duplicate count
- AND `--no-collapse` shows each matching turn

### Requirement: Search result contract

Search MUST print ten hits by default. Each human-readable hit MUST show an
8-character session ID prefix, timestamp, harness, cwd, branch, turn index,
markers, and a three-line snippet. Missing metadata MUST print `-`. Output MUST
state shown and total match counts. `--json` MUST emit one envelope with the
query, totals, freshness, warnings, and hits. JSON hits MUST include full IDs,
scores, and duplicate members. Snippets MUST NOT include complete bodies.

#### Scenario: A query has no matches

- WHEN a ready index has no matching documents
- THEN search exits 0
- AND output states the indexed denominator and build time
- AND output states sessions captured since the build

#### Scenario: An index is unavailable

- WHEN no readable session index exists
- THEN search fails
- AND the error names `vaultr session index --update`

### Requirement: Curated vault index

`vaultr session index --update` MUST maintain a separate curated Markdown index.
The curated index MUST include `learnings`, `decisions`, `incidents`, `runbooks`,
`systems`, `projects`, `glossary`, `tickets`, `alerts`, `pit`, and `jobs`. It
MUST exclude `conversations`, `teams`, `people`, `digests`, and `preferences`.
It MUST index a prompt sidecar only when its session has no readable capture.
`vaultr session search --curated` MUST show up to three curated results before
session results.

#### Scenario: A curated lookup is requested

- WHEN `--curated` is supplied
- THEN search queries the curated index
- AND output shows no more than three curated hits before session hits

#### Scenario: A readable capture has a prompt sidecar

- WHEN a prompt sidecar belongs to a readable captured session
- THEN the curated index does not add the prompt sidecar
- AND search does not return a duplicate prompt hit

### Requirement: Scheduled index freshness

Plant MUST run `vaultr session index --update` through a five-minute shared job.
The job ledger timestamp MUST be the freshness time reported by search. Search
MUST NOT update an index. `--workers` MUST set the decode worker count.

#### Scenario: The scheduled update misses a run

- WHEN a later scheduled update runs
- THEN it rescans all source fingerprints
- AND it repairs every detected source change

#### Scenario: Search sees stale data

- WHEN the last successful index job is older than recent captures
- THEN search reports the freshness state
- AND search does not run an index update
