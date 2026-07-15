---
every: 15m
cli: codex
model: gpt-5.6-sol
args: -c model_reasoning_effort=xhigh
close-pane: on-success
---
Read ~/.dotfiles/skills/Vault/Workflows/Learn.md and execute that workflow exactly, with `--learner codex` and these session directories as input: !`plant sessions eligible --learner codex --idle 60m --max 10`
