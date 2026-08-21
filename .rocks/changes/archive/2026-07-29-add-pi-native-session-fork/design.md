## Implementation Details

- Write Pi version 3 JSONL through the existing atomic no-overwrite writer boundary.
- Resolve Pi storage from `PI_CODING_AGENT_SESSION_DIR`, `PI_CODING_AGENT_DIR`, then the default agent directory.
- Translate normalized messages into a linear Pi parent chain.
- Preserve valid tool pairs and render malformed pairs as readable text.
- Add prompt and read-only flags only to the generated launch command.
- Rewrite exact Codex path `/codex/responses` to `/responses` only for upstream transport.

## Context

Pi uses the OpenAI Codex wire API and sends its Pi session ID through provider request headers.
Plant can therefore capture Pi traffic through the existing Codex adapter.

## Goals

- Produce resumable Pi sessions from Claude Code or Codex Session Captures.
- Resolve the active Pi session by its exact session ID.
- Preserve existing Claude Code and Codex fork behavior by default.

## Non-Goals

- Add Pi as a Session Capture source harness.
- Read Pi's native session store during reconstruction.
- Select a Session Capture by working directory.

## Decisions

- Keep Pi as a fork target instead of adding a third capture harness.
- Keep the observed request path in each Envelope.
- Use the native CLI launch arguments to deliver an optional initial prompt.
- Enforce read-only operation with target-specific native controls.

## Risks

- Pi may change its native format. A real Pi loader smoke check detects drift.
- Path rewriting may regress Codex. Exact-route tests preserve native Codex behavior.

## Migration Plan

Existing forks retain their current behavior because the new launch flags are opt-in.
