# Plant Agent Jobs — Delta

## ADDED Requirements

### Requirement: Scheduled cross-session reflection

Plant MUST run a `reflect` agent job every 2 hours that dispatches `/Vault reflect` to
Claude (`opus[1m]`) through the standard Herdr agent lifecycle, and MUST skip
the run when no new learnings have been ledgered since the job's last recorded
attempt.

#### Scenario: New learnings since last reflect

- WHEN the learnings ledger was modified after the reflect job's last recorded attempt
- THEN the job computes the prompt `/Vault reflect` and runs it through the agent lifecycle

#### Scenario: Nothing new to reflect over

- WHEN the learnings ledger is missing or unchanged since the last recorded attempt
- THEN the job records `skipped` and no agent pane is launched
