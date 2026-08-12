## ADDED Requirements

### Requirement: Explicit Herdr sidecar read

Vaultr MUST expose `session herdr <id>` as an explicit Herdr topology read. The command MUST prefer local `herdr.jsonl`, then local `herdr.jsonl.zst`. A local hit MUST NOT contact S3. The command MUST stream decoded JSONL to standard output.

#### Scenario: Raw sidecar is local

- WHEN the resolved Session Capture contains regular-file `herdr.jsonl`
- THEN Vaultr streams that file without contacting S3

#### Scenario: Sealed sidecar is local

- WHEN the resolved Session Capture contains regular-file `herdr.jsonl.zst`
- THEN Vaultr streams its decoded JSONL without contacting S3

### Requirement: Herdr sidecar fetch on explicit miss

Vaultr MUST fetch `herdr.jsonl.zst` only when `session herdr` resolves no local sidecar. Vaultr MUST use the Session Index date with neighboring-day probes. Vaultr MUST stage the object under an ignored temporary name. Vaultr MUST verify object size and zstd framing. Vaultr MUST atomically rename the result into a regular file. This path MUST NOT fetch `turns.jsonl.zst`.

#### Scenario: Sidecar exists in S3

- WHEN explicit Herdr inspection misses locally
- AND S3 contains a candidate `herdr.jsonl.zst` key
- THEN Vaultr materializes a byte-identical regular file
- AND Vaultr streams its decoded JSONL

#### Scenario: Fetch is disabled

- WHEN explicit Herdr inspection misses locally
- AND `--no-fetch` is set
- THEN Vaultr fails without contacting S3

#### Scenario: Store access is denied

- WHEN S3 denies a candidate lookup
- THEN Vaultr reports the denied lookup as an operational failure
- AND Vaultr does not report the sidecar as absent

#### Scenario: Sidecar is absent

- WHEN every candidate key returns not-found
- THEN Vaultr fails with every tried key named

### Requirement: Inventory remains local-only

Plant capture, sweep, and Cultivation Job inventory MUST NOT call the Herdr sidecar fetch path.

#### Scenario: Plant inventories Session Captures

- WHEN Plant walks Session Capture directories
- THEN no missing Herdr sidecar is fetched
