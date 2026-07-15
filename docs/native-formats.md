# Native session-storage formats — Claude Code 2.1.210 & Codex CLI 0.144.4

Documented from real on-disk data on this Mac (2026-07-15). Goal: enable a Rust writer to emit
synthetic-but-valid session files that `claude --resume <id>` and `codex resume <id>` natively resume.

---

## 1. Claude Code

### 1.1 Path & project-dir encoding

```
~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl        (root overridable via CLAUDE_CONFIG_DIR)
~/.claude/projects/<encoded-cwd>/<sessionId>/             (optional sibling dir: subagents/, tool-results/)
```

Encoding, verified against real dirs: **every `/` and every `.` in the absolute cwd becomes `-`**
(in general, non-alphanumeric chars map to `-`; existing `-` stays `-`). Leading `/` also becomes `-`,
so paths start with a dash. The encoding is lossy/non-reversible.

| Real cwd | Encoded dir |
|---|---|
| `/Users/cohnesor/.dotfiles` | `-Users-cohnesor--dotfiles` |
| `/Users/cohnesor/.dotfiles/.config/plant` | `-Users-cohnesor--dotfiles--config-plant` |
| `/Users/cohnesor/Documents/001Repos/das-pantheon/cnb` | `-Users-cohnesor-Documents-001Repos-das-pantheon-cnb` |
| `/private/tmp/claude-503/-Users-cohnesor--dotfiles/<uuid>/scratchpad` | `-private-tmp-claude-503--Users-cohnesor--dotfiles-<uuid>-scratchpad` |

`sessionId` is a lowercase UUIDv4; the filename stem must equal the `sessionId` field inside records.

### 1.2 Resume discovery

- **No index database exists.** There is no `~/.claude/__store.db` and no sessions-index file on this
  install. `~/.claude/sessions/` contains only per-PID runtime state (`22747.json` etc.), not an index.
- The `--resume` picker **scans the project directory** (`~/.claude/projects/<encoded-cwd>/*.jsonl`)
  for the current cwd's encoded dir. Dropping a well-formed `<uuid>.jsonl` there is sufficient for it
  to be listed and resumed. `~/.claude/history.jsonl` (prompt history: `{display, pastedContents,
  timestamp, project, sessionId}`) feeds prompt recall, not resume discovery. `.claude.json` holds
  per-project settings, not session lists.
- Picker titles come from the first real user message / `last-prompt` content in the file.

### 1.3 Record types (JSONL, one object per line)

Version string in all recent records: `"version":"2.1.210"` (older files: 2.1.207/2.1.209).

**Envelope fields present on every conversational record** (user/assistant/system/attachment):

```json
{
  "parentUuid": "<uuid-of-previous-record or null>",   // linked-list chain; first record: null
  "isSidechain": false,
  "type": "user" | "assistant" | "system" | "attachment",
  "uuid": "<uuid>",                                    // this record's id
  "timestamp": "2026-07-15T14:36:01.334Z",             // ISO-8601 UTC, ms precision
  "userType": "external",
  "entrypoint": "cli",
  "cwd": "/Users/cohnesor/.dotfiles",                  // may change mid-session
  "sessionId": "<must match filename>",
  "version": "2.1.210",
  "gitBranch": "main"                                  // present when cwd is a git repo
}
```

Additional optional envelope fields observed: `promptId` (uuid, groups records belonging to one user
turn — on user records), `requestId` (`req_...`, on assistant records), `session_id` (duplicate
snake_case of sessionId, on assistant + tool_result records), `attributionSkill`, `isMeta` (true for
injected/skill content the user didn't type), `slug` (session nickname e.g.
`"cozy-wiggling-jellyfish"`, appears on later records once assigned), `logicalParentUuid` (on
compact_boundary), `sourceToolAssistantUUID` (on tool_result user records), `toolUseResult` (rich
structured result mirror on tool_result user records).

#### Bookkeeping records (no envelope; just `sessionId` + type-specific fields)

```json
{"type":"mode","mode":"normal","sessionId":"..."}
{"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"..."}
{"type":"last-prompt","lastPrompt":"<text…>","leafUuid":"<uuid of last record>","sessionId":"..."}
{"type":"queue-operation","operation":"enqueue","timestamp":"...","sessionId":"...","content":"..."}
```

`last-prompt` appears both at file start (rewritten on resume; may omit `lastPrompt` and carry only
`leafUuid`) and at file end after each turn. `leafUuid` tells resume which record is the tip of the
active branch. These are appended, not updated in place.

#### User message (plain text)

```json
{"parentUuid":"<prev>","isSidechain":false,"promptId":"<uuid>","type":"user",
 "message":{"role":"user","content":"the text the user typed"},
 "uuid":"...","timestamp":"...", ...envelope...}
```

`message.content` is a plain string for typed text, or an array of blocks
(`[{"type":"text","text":"..."}]`) for injected content (then usually `"isMeta":true`).

#### Assistant message

One JSONL record **per content block** — a single API response with thinking + text + tool_use is
written as three consecutive records sharing the same `message.id` and `requestId`, chained by
`parentUuid`. `message` is a near-verbatim Anthropic API message:

```json
{"parentUuid":"<prev>","isSidechain":false,
 "message":{
   "model":"claude-opus-4-8","id":"msg_011Cd41b4Q2pXFASEfz1UXQH","type":"message","role":"assistant",
   "content":[{"type":"text","text":"..."}],
   "stop_reason":"tool_use","stop_sequence":null,"stop_details":null,
   "usage":{"input_tokens":9501,"cache_creation_input_tokens":7727,"cache_read_input_tokens":38391,
            "output_tokens":123, "...":"(server_tool_use, cache_creation, iterations, speed — copied from API)"}},
 "requestId":"req_011Cd41b31SYYai2innxqHXR",
 "type":"assistant","uuid":"...","timestamp":"...","session_id":"...", ...envelope...}
```

Content block variants: `{"type":"text","text":...}`,
`{"type":"thinking","thinking":"...","signature":"<base64>"}`,
`{"type":"tool_use","id":"toolu_...","name":"Read","input":{...},"caller":{"type":"direct"}}`.

#### Tool result (a user record)

```json
{"parentUuid":"<uuid of the tool_use assistant record>","isSidechain":false,"promptId":"<uuid>",
 "type":"user",
 "message":{"role":"user","content":[
   {"tool_use_id":"toolu_01Xh...","type":"tool_result","content":"<string or blocks>"}]},
 "uuid":"...","timestamp":"...",
 "toolUseResult":{"type":"text","file":{"filePath":"...","content":"...","numLines":122,"startLine":1,"totalLines":122}},
 "sourceToolAssistantUUID":"<same as parentUuid>","session_id":"...", ...envelope...}
```

`toolUseResult` shape is tool-specific (mirror of the harness's structured result); resume replays
`message.content` — `toolUseResult` appears to be UI/bookkeeping enrichment.

#### System records

```json
{"type":"system","subtype":"local_command","content":"<local-command-stdout></local-command-stdout>",
 "level":"info","isMeta":false, ...envelope...}
{"type":"system","subtype":"turn_duration","durationMs":530474,"messageCount":181, ...envelope...}
```

Observed subtypes: `local_command`, `turn_duration`, `bridge_status`, `compact_boundary`.

#### Attachment records

Hook output etc.: `{"type":"attachment","attachment":{"type":"hook_success","hookName":"SessionStart:startup","toolUseID":"<uuid>","hookEvent":"SessionStart","content":"...","stdout":"...","stderr":"","exitCode":0,"command":"...","durationMs":210}, ...envelope...}`.
Often the first envelope record of a session (`parentUuid:null`).

#### Compaction

Two records: a `compact_boundary` system record then the summary injected as a user message.

```json
{"parentUuid":null,"logicalParentUuid":"<pre-compact tail uuid>","isSidechain":false,
 "type":"system","subtype":"compact_boundary","content":"Conversation compacted","level":"info",
 "compactMetadata":{"trigger":"manual","preTokens":106939,"postTokens":9987,
   "cumulativeDroppedTokens":96952,"durationMs":64577,
   "preservedSegment":{"headUuid":"...","anchorUuid":"...","tailUuid":"..."},
   "preservedMessages":{"anchorUuid":"...","uuids":["..."]}}, ...envelope...}

{"parentUuid":"<compact_boundary uuid>","type":"user",
 "message":{"role":"user","content":"This session is being continued from a previous conversation... Summary: ..."},
 "isVisibleInTranscriptOnly":true,"isCompactSummary":true, ...envelope with slug...}
```

No standalone `{"type":"summary"}` records exist in any project dir on this install (older-format
summary-first lines are gone in 2.1.x).

#### First record of a session

Not a meta/summary record. Real files start with either bookkeeping (`last-prompt`/`mode`/
`permission-mode`) or directly with the first envelope record (`parentUuid:null`, typically an
`attachment` from a SessionStart hook, or a plain user message when no hooks fire).

### 1.4 Required vs optional (conservative inference)

Every conversational record always has: `parentUuid`, `isSidechain`, `type`, `uuid`, `timestamp`,
`userType`, `entrypoint` (recent versions), `cwd`, `sessionId`, `version`, plus `message` for
user/assistant. Plausibly optional: `gitBranch`, `promptId`, `requestId`, `session_id`, `slug`,
`isMeta`, `attributionSkill`, `toolUseResult`, `usage` internals (keep at least
`input_tokens`/`output_tokens`), bookkeeping records other than a trailing `last-prompt`
(recommended so the picker shows a title and resume finds the leaf). `mode`/`permission-mode`
records are written by the real CLI but a resumable file works from the chain alone; include them
for safety.

---

## 2. Codex CLI

### 2.1 Path & filename

```
~/.codex/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl   (root overridable via CODEX_HOME)
```

- Date path components and the filename timestamp are **local time** (verified: filename
  `rollout-2026-07-15T03-52-38-...` vs session_meta `"timestamp":"2026-07-15T08:52:38.481Z"` —
  America/Chicago, UTC-5). Colons in the time are replaced with `-`.
- `<uuid>` is a **UUIDv7** (e.g. `019f64fa-5f28-7772-9949-d2ddd2292625`) and equals
  `payload.session_id`/`payload.id` in `session_meta` and the `threads.id` DB row. Its embedded
  timestamp should be consistent with the filename time.

### 2.2 Record envelope

Every line: `{"timestamp":"<ISO-8601 UTC ms Z>","type":"<record type>","payload":{...}}`.

Top-level `type` values observed: `session_meta`, `response_item`, `event_msg`, `turn_context`,
`world_state`, `compacted`.

### 2.3 Record shapes (from real 0.144.4 rollouts)

**session_meta — always line 1:**

```json
{"timestamp":"2026-07-15T08:53:00.995Z","type":"session_meta","payload":{
  "session_id":"019f64fa-...","id":"019f64fa-...",          // duplicated
  "timestamp":"2026-07-15T08:52:38.481Z",                    // session start, UTC
  "cwd":"/Users/cohnesor/pantheon-collective/proppy",
  "originator":"codex-tui","cli_version":"0.144.4",
  "source":"cli","thread_source":"user","model_provider":"openai",
  "base_instructions":{"text":"You are Codex, an agent based on GPT-5. ..."},
  "history_mode":"legacy",
  "context_window":{"window_id":"<uuidv7>"},
  "git":{"commit_hash":"...","branch":"main","repository_url":"https://gitlab.com/..."}}}
```

**turn_context — once per turn:**

```json
{"type":"turn_context","payload":{"turn_id":"<uuidv4>","cwd":"...","workspace_roots":["..."],
 "current_date":"2026-07-15","timezone":"America/Chicago","approval_policy":"never",
 "approvals_reviewer":"user","sandbox_policy":{"type":"danger-full-access"},
 "permission_profile":{"type":"disabled"},"model":"gpt-5.6-sol","comp_hash":"3000",
 "personality":"pragmatic","collaboration_mode":{"mode":"default","settings":{...}},
 "multi_agent_version":"v2","multi_agent_mode":"explicitRequestOnly","realtime_active":false,
 "effort":"high","summary":"auto"}}
```

**response_item — the actual model history** (payload types = OpenAI Responses items):

- message: `{"type":"message","role":"user"|"developer"|"assistant","content":[{"type":"input_text","text":"..."}], "internal_chat_message_metadata_passthrough":{"turn_id":"..."}}`
  (assistant messages use `output_text` blocks; user/developer use `input_text`).
- reasoning: `{"type":"reasoning","id":"rs_...","summary":[],"encrypted_content":"<opaque>", ...}`
- function_call: `{"type":"function_call","id":"fc_...","name":"wait","arguments":"<json string>","call_id":"call_..."}`
- function_call_output: `{"type":"function_call_output","call_id":"call_...","output":[{"type":"input_text","text":"..."}]}`
- custom_tool_call / custom_tool_call_output: same idea with `input` string and `status:"completed"`.

**event_msg — UI/event stream** (not model history). Payload types observed:
`thread_goal_updated`, `task_started`, `agent_message`, `token_count`, `patch_apply_end`,
`mcp_tool_call_end`, `context_compacted`, `task_complete`, `thread_settings_applied`, and
**`user_message`**: `{"type":"user_message","message":"continue","images":[],"local_images":[],"text_elements":[]}`.
`user_message` is the event the resume picker/DB backfill uses for previews (`threads.title` /
`first_user_message` / `preview` and `has_user_event` derive from it). A synthetic writer should emit
one `event_msg/user_message` per user turn alongside the `response_item` user message.

**world_state:** snapshot of AGENTS.md, environments, skills flags:
`{"type":"world_state","payload":{"full":true,"state":{...}}}` — emitted around turn starts; likely
optional for resume but present in every real rollout.

**compacted:** `{"type":"compacted","payload":{"message":"","replacement_history":[<response_item payloads>],
"window_number":1,"first_window_id":"...","previous_window_id":"...","window_id":"<new uuidv7>"}}`
followed by `event_msg/context_compacted`. Resume rebuilds history from `replacement_history` forward.

### 2.4 State DB & resume discovery

`~/.codex/state_5.sqlite` (sqlx-managed; also `codex-dev.db` under `~/.codex/sqlite/` for dev builds).
Key tables:

- **threads**: `id` (uuid), `rollout_path` (absolute path), `created_at/updated_at` (+`_ms`),
  `source`, `model_provider`, `cwd`, `title`, `sandbox_policy`, `approval_mode`, `tokens_used`,
  `has_user_event`, `archived`, `git_sha/git_branch/git_origin_url`, `cli_version`,
  `first_user_message`, `model`, `reasoning_effort`, `preview`, `recency_at(_ms)`,
  `history_mode` (`legacy` on all current rows).
- **backfill_state**: `{status:"complete", last_watermark:"sessions/2026/02/26/rollout-...jsonl", ...}` —
  direct evidence of the **read-repair/backfill scanner**: Codex walks `sessions/YYYY/MM/DD/rollout-*.jsonl`
  in filename order past a watermark and inserts missing `threads` rows from rollout content. So a
  rollout file with no DB row is discovered by filename (this is why the filename timestamp/uuid format
  must be exact); the DB is a cache, not the source of truth.
- `thread_dynamic_tools`, `agent_jobs` — not needed for resume.

`codex resume <uuid>` resolves the id → rollout path (DB row or filename scan fallback) and replays
`session_meta` + `response_item`/`compacted` records. Synthetic files therefore work without touching
the DB, provided filename and `session_meta.id` agree. Recent rollouts all report
`"cli_version":"0.144.4"`, `originator":"codex-tui"`.

---

## 3. Synthetic-writer checklist

### Claude Code — minimal viable resumable session

1. Choose UUIDv4 `S`; write `~/.claude/projects/<encode(cwd)>/S.jsonl`.
2. Records, chained by `parentUuid` (first = null), all carrying the full envelope
   (`uuid`, `timestamp` monotonic ISO-UTC, `sessionId:S`, `cwd`, `version:"2.1.210"`,
   `userType:"external"`, `entrypoint:"cli"`, `isSidechain:false`, `gitBranch` if repo).
3. (a) **Plain turns**: user record (`message.content` string, fresh `promptId`) → assistant record(s)
   (`message` with `model`, `id:"msg_<24 alnum>"`, `role:"assistant"`, `content:[{"type":"text",...}]`,
   `stop_reason:"end_turn"`, minimal `usage`).
4. (b) **Tool use**: assistant record with `content:[{"type":"tool_use","id":"toolu_...","name":...,"input":{...},"caller":{"type":"direct"}}]`,
   `stop_reason:"tool_use"` → user record with `content:[{"tool_use_id":..., "type":"tool_result","content":"..."}]`
   and `sourceToolAssistantUUID` = the tool_use record's uuid → assistant text record. One record per
   content block, same `message.id` across blocks of one response.
5. (c) End with `{"type":"last-prompt","lastPrompt":"<first user text>","leafUuid":"<last uuid>","sessionId":S}`.
   Optionally prepend `mode`/`permission-mode` records.
6. No index to update — directory scan suffices.

### Codex — minimal viable resumable session

1. Choose UUIDv7 `U` whose timestamp ≈ session start; write
   `~/.codex/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDTHH-MM-SS-U.jsonl` using **local time** for path
   and filename.
2. Line 1: `session_meta` with `session_id`=`id`=`U`, `cwd`, `originator:"codex-tui"`,
   `cli_version:"0.144.4"`, `source:"cli"`, `thread_source:"user"`, `model_provider:"openai"`,
   `base_instructions.text`, `history_mode:"legacy"`, `context_window.window_id` (fresh UUIDv7),
   `git{}` if repo.
3. Per turn: `event_msg/user_message` (preview text) + `turn_context` +
   `response_item/message role:user` with `input_text` blocks.
4. (a) Plain reply: `response_item/message role:assistant` with `output_text` blocks
   (+ optional `event_msg/agent_message`, `event_msg/task_complete`).
5. (b) Tools: `response_item/function_call` (`call_id:"call_..."`, `arguments` as JSON string) then
   `response_item/function_call_output` (same `call_id`, `output` array of `input_text` blocks).
6. Every record wrapped `{"timestamp","type","payload"}`.
7. DB row optional — backfill/read-repair creates it from the filename; do not write the sqlite
   yourself.

### Risks

- **Claude**: broken `parentUuid` chain or a `last-prompt.leafUuid` pointing at a missing uuid may
  make resume pick a wrong/empty branch; `thinking` blocks carry cryptographic `signature`s — omit
  thinking blocks entirely in synthetic files rather than forging signatures; version drift (2.1.x
  format changed recently: `entrypoint`, per-block assistant records) — pin to observed 2.1.210 shapes;
  `message.id`/`requestId` formats are unvalidated but keep plausible (`msg_`/`req_` + base62).
- **Codex**: filename timestamp is local-time while record timestamps are UTC — mixing these up breaks
  backfill ordering; UUID must be v7 and consistent between filename and session_meta; `reasoning`
  items carry `encrypted_content` the server may validate — omit reasoning items; if a stale DB row
  already claims the uuid with a different `rollout_path`, resume may follow the DB; missing
  `event_msg/user_message` yields an empty preview/title (`has_user_event:0`) but sessions still
  appear (real rows with 0 exist).
- Both CLIs move fast; re-verify against the exact installed version before shipping the writer.
