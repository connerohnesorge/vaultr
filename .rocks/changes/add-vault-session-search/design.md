## Context

The vault stores wire captures as raw `turns.jsonl` files or sealed `.zst`
files. Request bodies use deltas. Responses use SSE. `vaultr session show`
reconstructs the final replay and intentionally omits `ToolResult` blocks.

A search index needs the union of observed messages. A final replay can omit
messages that existed earlier. The index also needs current unsealed captures.
A capture becomes scrubbed when it seals.

## Goals / Non-Goals

- Goals: local lexical search, tool I/O, phrase queries, literal fragment
  search, curated vault search, incremental updates, and machine-local state.
- Non-goals: embeddings, score fusion, committed indexes, search-triggered
  writes, and a new Plant subcommand.

## Architecture

`recon::reconstruct` will retain every observed message and identify the final
replay membership. Recovery will retain usable history when a delta lineage
breaks. Plant will materialize `state.json` captures into scrubbed sealed
captures before search consumes them.

The indexer will create derived turns. A turn starts at a user message with a
text block. Its stable identity is the session ID and SHA-256 of its canonical
blocks. Each document keeps its body and metadata. Tool output is capped at
4,000 characters. The body keeps the first 3,000 and last 1,000 characters.

Tantivy will use separate session and curated indexes in
`~/.local/state/vaultr/`. A metadata file stores the schema version and source
fingerprints. A mismatch deletes and rebuilds the affected index. Nothing
under this directory is committed or synchronized.

The session schema uses the default tokenizer for prose and a `(3,4)` ngram
field for paths, commands, and identifiers. Tool output is excluded from the
ngram field. Search parses phrases and schema fields through Tantivy. Unknown
field prefixes are literal query text. Conjunction is the default.

The curated index stores selected whole Markdown files. It includes
`learnings`, `decisions`, `incidents`, `runbooks`, `systems`, `projects`,
`glossary`, `tickets`, `alerts`, `pit`, and `jobs`. It excludes conversational,
chat, people, digest, and preference directories. Prompt sidecars fill only
sessions without a readable capture.

A Plant job runs `vaultr session index --update` every five minutes. `--update`
performs a cold build when no index exists. It scans sources every run. It
replaces a session after sealing. Search never invokes the writer.

## ADRs

### ADR-0001: Use two local Tantivy indexes

Sessions and curated records have different document frequencies. A pooled
index dilutes curated terms. Two indexes avoid score fusion and preserve a
separate curated result section.

### ADR-0002: Treat the index as a rebuildable cache

Tantivy does not guarantee index-format compatibility. A version mismatch must
fail closed into a rebuild. This also handles downgrades and removes migration
code.

### ADR-0003: Ship lexical search before semantic search

Tantivy has no vector field. Semantic search needs another index and a fusion
layer. No observed lexical failure justifies that surface today.

## Risks / Trade-offs

A cold build is decode-bound and can take minutes. The Plant job owns it.
Stored bodies increase local disk use, but they provide snippets without a
decode pass. The index can briefly contain unsealed text before sealing. The
next update replaces it with the scrubbed sealed text.
