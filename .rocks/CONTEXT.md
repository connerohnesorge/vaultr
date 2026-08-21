# Session Capture and Cultivation

Vaultr preserves agent sessions as reconstructable evidence; Plant captures that evidence and runs the opinionated automation that cultivates durable knowledge from it.

## Language

**Vaultr**:
The deterministic, read-only core that discovers, reconstructs, renders, validates, and forks captured sessions.
_Avoid_: Plant, wireproxy

**Plant**:
The resident runtime that writes Session Captures, seals them, and runs Cultivation Jobs.
_Avoid_: Vaultr, core

**Session Capture**:
The dated record of one Claude Code or Codex session, indexed by its Session Index entry.
_Avoid_: transcript, native session file

**Session Index**:
The `.meta/<session-id>.json` entry authoritative for Session Capture identity and discovery.
_Avoid_: cache, derived index

**Envelope**:
One observed harness request and response stored in a Session Capture, with request history delta-encoded.
_Avoid_: turn, message

**Reconstruction**:
The deterministic recovery of current native wire history from a Session Capture.
_Avoid_: rendering, normalization

**Normalized Transcript**:
The portable, intentionally lossy conversation model used for display and cross-harness translation.
_Avoid_: Reconstruction, raw history

**Fork**:
A fresh resumable native harness session written exclusively from a Session Capture.
_Avoid_: copy, seeded resume

**Vault Content**:
The durable Markdown knowledge collections validated by Vaultr and cultivated by Plant jobs.
_Avoid_: Session Capture, session store

**Cultivation Job**:
A Plant-scheduled agent run that learns, reconciles, or repairs Vault Content.
_Avoid_: capture, validation

**Sealing**:
Plant's security scrub followed by replacement of an inactive raw Session Capture with its compressed representation.
_Avoid_: lossless compression

## Relationships

- **Plant** appends **Envelopes** to a live **Session Capture**
- A **Session Index** identifies exactly one **Session Capture**
- **Reconstruction** reads a **Session Capture** and may produce a **Normalized Transcript** or **Fork**
- **Plant** may security-scrub a **Session Capture** during **Sealing**; the sealed capture is stable afterward
- A **Cultivation Job** may change **Vault Content** but never a sealed **Session Capture**
- **Vaultr** validates **Vault Content**; **Plant** decides how cultivation agents repair it

## Example dialogue

> **Dev:** "Can a **Cultivation Job** clean up this captured conversation?"
> **Domain expert:** "No. It may repair **Vault Content**, but only **Plant** may alter a live **Session Capture**, and only security scrubbing during **Sealing** may rewrite it."

## Flagged ambiguities

- "vault" was used for both session evidence and durable knowledge — resolved: use **Session Capture** for evidence and **Vault Content** for knowledge.
- "immutable capture" hid the pre-seal security rewrite — resolved: captures are appendable and scrub-mutable until **Sealing**, then stable.
- `.meta` was described as a replaceable cache — resolved: the **Session Index** is authoritative for identity and discovery; only dated directory lookup has a stale-date fallback.
