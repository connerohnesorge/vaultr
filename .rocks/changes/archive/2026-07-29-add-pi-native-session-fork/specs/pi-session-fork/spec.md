## ADDED Requirements

### Requirement: Pi native writer

`vaultr session fork <id> --into pi` MUST write a fresh Pi version 3 session.
The writer MUST use atomic mode-0600 creation and MUST NOT overwrite a session.
The writer MUST reconstruct exclusively from the Session Capture.

#### Scenario: Session resumes in Pi

- WHEN a Claude Code or Codex Session Capture is forked into Pi
- THEN Pi opens the generated session with the reconstructed conversation
- AND the source Session Capture remains unchanged

### Requirement: Pi session storage

The Pi writer MUST resolve storage from `PI_CODING_AGENT_SESSION_DIR`.
It MUST otherwise use the `sessions` directory under `PI_CODING_AGENT_DIR`.
It MUST otherwise use `~/.pi/agent/sessions`.

#### Scenario: Explicit Pi session directory

- WHEN `PI_CODING_AGENT_SESSION_DIR` names an absolute directory
- THEN the Pi writer creates the native session beneath that directory

### Requirement: Portable Pi history

The Pi writer MUST preserve normalized user and assistant text.
It MUST preserve well-formed tool pairs.
It MUST render malformed or unsupported tool interactions as readable text.
It MUST NOT emit opaque reasoning signatures.

#### Scenario: Unsupported tool interaction

- WHEN a normalized history contains an unsupported tool interaction
- THEN the Pi session contains readable text for that interaction
- AND the fork remains resumable

### Requirement: Optional launch prompt

`vaultr session fork` MUST accept an optional `--prompt` value.
The launch command MUST submit that value as the first new user prompt.
Forks without `--prompt` MUST remain unseeded.

#### Scenario: Prompted fork

- WHEN a fork receives `--prompt "review this"`
- THEN the selected native agent resumes with `review this` as its next user prompt

#### Scenario: Unprompted fork

- WHEN a fork omits `--prompt`
- THEN the selected native agent resumes without a new user prompt

### Requirement: Read-only launch

`vaultr session fork` MUST accept `--read-only`.
The generated launch command MUST use each target's native read-only controls.
The default launch command MUST retain its existing authority.

#### Scenario: Read-only Pi fork

- WHEN a Pi fork receives `--read-only`
- THEN Pi starts with only `read`, `grep`, `find`, and `ls`

#### Scenario: Default fork authority

- WHEN a fork omits `--read-only`
- THEN Vaultr adds no read-only restriction to the target launch
