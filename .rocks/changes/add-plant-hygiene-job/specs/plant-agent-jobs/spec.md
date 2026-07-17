# Plant Agent Jobs Delta

## ADDED Requirements

### Requirement: Vault git hygiene job

Plant MUST run an hourly in-process Cultivation Job that keeps the vault
repository's git state healthy: it pushes committed work that is ahead of
upstream, commits stray Learn-owned output (`learnings/`, `preferences/`,
`digests/`) once it has been idle past a grace window, and reports — without
remediating — uncommitted paths outside that scope and a pending dotfiles
`vault` submodule bump. The job MUST NOT stage paths outside the Learn-owned
trio, MUST NOT commit or push in the dotfiles repository, and MUST NOT
force-push, pull, or rebase.

#### Scenario: Vault ahead of upstream

- WHEN the vault repository has commits not on its upstream branch
- THEN the job pushes
- AND a push failure records a `failed` outcome with the git detail so the next hourly tick retries

#### Scenario: Stray Learn-owned output past the grace window

- WHEN `learnings/`, `preferences/`, or `digests/` contain uncommitted changes
- AND the newest such change is older than the 30-minute grace window
- THEN the job commits exactly those paths with `--no-verify` and pushes

#### Scenario: Learn-owned output inside the grace window

- WHEN Learn-owned uncommitted changes are all newer than the grace window
- THEN the job leaves them untouched so a live learn pane is never raced

#### Scenario: Out-of-scope dirt or pending submodule bump

- WHEN uncommitted vault paths exist outside the Learn-owned trio and `sessions/`
- OR the dotfiles repository shows a modified `vault` submodule pointer
- THEN the job records a `failed` outcome naming the top-level directories and counts
- AND performs no commit for those paths

#### Scenario: Non-fast-forward push

- WHEN the push is rejected as non-fast-forward
- THEN the job records `failed` and does not pull, rebase, or force-push
