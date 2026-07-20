//! Streaming reconstruction of the final message history from a turns.jsonl
//! (raw or zstd) capture, plus the `body_delta` encoder (`encode_delta`) that
//! plant uses at capture time — encode and apply live side by side so they
//! can't drift. Memory is bounded by the largest single envelope plus the
//! final history — the archive is never loaded whole.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Take};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::vault::{parse_capture_generation_name, CaptureGenerationName};

/// Harness identity of a capture, derived once during reconstruction.
///
/// Precedence: the envelope `harness` field is ground truth — envelopes are
/// the captured wire truth, whereas `.meta/<id>.json` is a mutable
/// hook-merged sidecar that can go stale or be wrong. When envelopes lack
/// the field, a history key of "input" resolves to Codex. Only when neither
/// resolves (`Recon::harness` is `None`) may callers fall back to
/// meta.harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    /// Map a recorded harness label to an identity. Accepts the "claude"
    /// alias alongside the canonical "claude-code"; unknown labels resolve
    /// nothing so callers fall through to their next source.
    pub fn from_label(label: &str) -> Option<Harness> {
        match label {
            "codex" => Some(Harness::Codex),
            "claude-code" | "claude" => Some(Harness::Claude),
            _ => None,
        }
    }
}

/// Result of reconstructing a capture.
#[derive(Debug)]
pub struct Recon {
    /// History key seen on the wire: "messages" (Anthropic) or "input" (Codex).
    pub key: String,
    /// Harness identity derived from the envelopes (see [`Harness`] for the
    /// precedence rules). `None` for degenerate captures where neither the
    /// envelope `harness` field nor the history key resolved.
    pub harness: Option<Harness>,
    /// Message count from history deltas alone (matches recon.mjs `count`).
    pub history_len: usize,
    /// Final history, with the trailing completed response (if any) appended.
    pub messages: Vec<Value>,
    /// Number of trailing assistant items appended from the final response.
    pub trailing_appended: usize,
    /// Envelopes parsed.
    pub envelopes: usize,
}

struct SnapshotFile {
    name: String,
    file: File,
    len: u64,
}

struct SnapshotDetached {
    segment: SnapshotFile,
    base_len: u64,
    digest: String,
}

struct ReconstructionSnapshot {
    directory: PathBuf,
    sealed: Option<SnapshotFile>,
    detached: Option<SnapshotDetached>,
    raw: Option<SnapshotFile>,
}

struct SnapshotDirectory {
    path: PathBuf,
    file: File,
}

impl SnapshotDirectory {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("open session directory {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    fn lock_shared(&self) -> Result<()> {
        loop {
            // SAFETY: the retained directory descriptor remains valid for this
            // object's lifetime and flock does not take ownership of it.
            if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_SH) } == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error)
                    .with_context(|| format!("lock session directory {}", self.path.display()));
            }
        }
    }

    fn name(&self, name: &str) -> Result<CString> {
        if name.is_empty()
            || Path::new(name).file_name().and_then(|part| part.to_str()) != Some(name)
        {
            anyhow::bail!("invalid capture generation under {}", self.path.display());
        }
        CString::new(name)
            .with_context(|| format!("invalid capture generation under {}", self.path.display()))
    }

    fn open_generation(&self, name: &str) -> Result<SnapshotFile> {
        let name_c = self.name(name)?;
        // SAFETY: name_c is NUL-terminated, the retained directory descriptor
        // is valid, and a successful descriptor is transferred into File.
        let descriptor = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("open capture generation {}", self.path.join(name).display())
            });
        }
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata().with_context(|| {
            format!(
                "inspect capture generation {}",
                self.path.join(name).display()
            )
        })?;
        if !metadata.is_file() {
            anyhow::bail!(
                "capture generation is not a regular file at {}",
                self.path.join(name).display()
            );
        }
        Ok(SnapshotFile {
            name: name.to_string(),
            file,
            len: metadata.len(),
        })
    }

    fn same_file(left: &File, right: &File) -> Result<bool> {
        let left = left.metadata().context("inspect retained capture entry")?;
        let right = right.metadata().context("inspect current capture entry")?;
        Ok(left.is_file()
            && right.is_file()
            && left.dev() == right.dev()
            && left.ino() == right.ino())
    }

    fn revalidate_entry(&self, expected: &SnapshotFile) -> Result<()> {
        let current = self.open_generation(&expected.name)?;
        if !Self::same_file(&current.file, &expected.file)? {
            anyhow::bail!(
                "capture generation changed during inventory at {}",
                self.path.join(&expected.name).display()
            );
        }
        Ok(())
    }

    fn revalidate_path(&self) -> Result<()> {
        let current = Self::open(&self.path)?;
        let retained = self
            .file
            .metadata()
            .with_context(|| format!("inspect session directory {}", self.path.display()))?;
        let current = current
            .file
            .metadata()
            .with_context(|| format!("reinspect session directory {}", self.path.display()))?;
        if retained.dev() != current.dev() || retained.ino() != current.ino() {
            anyhow::bail!(
                "session directory changed during inventory at {}",
                self.path.display()
            );
        }
        Ok(())
    }

    fn entry_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.path)
            .with_context(|| format!("read session directory {}", self.path.display()))?
        {
            let entry = entry
                .with_context(|| format!("read session entry under {}", self.path.display()))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }
}

impl ReconstructionSnapshot {
    fn with_hook(dir: &Path, retained: impl FnOnce()) -> Result<Self> {
        let directory = SnapshotDirectory::open(dir)?;
        directory.lock_shared()?;
        directory.revalidate_path()?;

        let mut snapshot = Self {
            directory: dir.to_path_buf(),
            sealed: None,
            detached: None,
            raw: None,
        };
        for name in directory.entry_names()? {
            let kind = parse_capture_generation_name(&name).with_context(|| {
                format!(
                    "invalid detached generation at {}",
                    directory.path.join(&name).display()
                )
            })?;
            let Some(kind) = kind else {
                continue;
            };
            let segment = directory.open_generation(&name)?;
            match kind {
                CaptureGenerationName::Raw => snapshot.raw = Some(segment),
                CaptureGenerationName::Sealed => snapshot.sealed = Some(segment),
                CaptureGenerationName::Detached { base_len, digest } => {
                    if snapshot.detached.is_some() {
                        anyhow::bail!("multiple detached capture generations in {}", dir.display());
                    }
                    let actual = crate::vault::sha256_reader(segment.reader()?)?;
                    if actual != digest {
                        anyhow::bail!(
                            "detached generation digest mismatch at {}",
                            dir.join(&segment.name).display()
                        );
                    }
                    snapshot.detached = Some(SnapshotDetached {
                        segment,
                        base_len,
                        digest,
                    });
                }
            }
        }

        let mut identities = HashSet::new();
        for segment in snapshot.segments() {
            let metadata = segment.file.metadata().with_context(|| {
                format!(
                    "inspect capture generation {}",
                    dir.join(&segment.name).display()
                )
            })?;
            if !identities.insert((metadata.dev(), metadata.ino())) {
                anyhow::bail!("duplicate capture generation inode in {}", dir.display());
            }
            directory.revalidate_entry(segment)?;
        }
        directory.revalidate_path()?;
        retained();
        Ok(snapshot)
    }

    fn segments(&self) -> impl Iterator<Item = &SnapshotFile> {
        self.sealed
            .iter()
            .chain(self.detached.iter().map(|detached| &detached.segment))
            .chain(self.raw.iter())
    }
}

impl SnapshotFile {
    fn reader(&self) -> Result<Take<File>> {
        let mut file = self
            .file
            .try_clone()
            .context("clone retained capture generation")?;
        file.seek(SeekFrom::Start(0))
            .context("seek retained capture generation")?;
        Ok(file.take(self.len))
    }

    fn decoded_suffix_digest(&self, offset: u64) -> Result<String> {
        if offset > self.len {
            anyhow::bail!("sealed destination is shorter than detached base");
        }
        let mut file = self
            .file
            .try_clone()
            .context("clone retained sealed generation")?;
        file.seek(SeekFrom::Start(offset))
            .context("seek retained sealed generation")?;
        let decoder = zstd::Decoder::new(file.take(self.len - offset)).context("zstd decoder")?;
        crate::vault::sha256_reader(decoder)
    }
}

/// Reconstruct from a capture file path (`.zst` handled transparently).
///
/// A resumed capture can have sealed, detached-sealing, and live raw
/// generations. Entering through any canonical sibling reconstructs each
/// generation exactly once in that order.
pub fn reconstruct(path: &Path) -> Result<Recon> {
    let name = path.file_name().and_then(|name| name.to_str());
    let canonical = matches!(name, Some("turns.jsonl" | "turns.jsonl.zst"))
        || name.is_some_and(|name| name.starts_with("turns.jsonl.sealing-"));
    if canonical {
        return reconstruct_canonical(path);
    }

    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut st = ReconState::new();
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        let dec = zstd::Decoder::new(file).context("zstd decoder")?;
        run_segment(BufReader::new(dec), Segment::Sealed, &mut st)?;
    } else {
        run_segment(BufReader::new(file), Segment::LiveRaw, &mut st)?;
    }
    Ok(st.finish())
}

fn reconstruct_canonical(path: &Path) -> Result<Recon> {
    reconstruct_canonical_with_hook(path, || {})
}

fn reconstruct_canonical_with_hook(path: &Path, retained: impl FnOnce()) -> Result<Recon> {
    let dir = path.parent().unwrap_or_else(|| Path::new(""));
    let snapshot = ReconstructionSnapshot::with_hook(dir, retained)?;
    reconstruct_snapshot(snapshot)
}

fn reconstruct_snapshot(snapshot: ReconstructionSnapshot) -> Result<Recon> {
    let mut st = ReconState::new();
    if let Some(sealed) = &snapshot.sealed {
        let dec = zstd::Decoder::new(sealed.reader()?).context("zstd decoder")?;
        run_segment(BufReader::new(dec), Segment::Sealed, &mut st)?;
    }
    if let Some(detached) = &snapshot.detached {
        let sealed_path = snapshot.directory.join("turns.jsonl.zst");
        match snapshot.sealed.as_ref().map(|sealed| sealed.len) {
            Some(len) if len < detached.base_len => anyhow::bail!(
                "reconstruct: sealed destination shorter than detached base at {}",
                sealed_path.display()
            ),
            None if detached.base_len > 0 => anyhow::bail!(
                "reconstruct: detached generation has no sealed base at {}",
                sealed_path.display()
            ),
            Some(len) if len > detached.base_len => {
                let digest = snapshot
                    .sealed
                    .as_ref()
                    .expect("sealed length came from retained segment")
                    .decoded_suffix_digest(detached.base_len)
                    .with_context(|| {
                        format!("verify detached suffix at {}", sealed_path.display())
                    })?;
                if digest != detached.digest {
                    anyhow::bail!(
                        "reconstruct: sealed suffix conflicts with detached generation at {}",
                        sealed_path.display()
                    );
                }
            }
            _ => run_segment(
                BufReader::new(detached.segment.reader()?),
                Segment::Sealed,
                &mut st,
            )?,
        }
    }
    if let Some(raw) = &snapshot.raw {
        run_segment(BufReader::new(raw.reader()?), Segment::LiveRaw, &mut st)?;
    }
    Ok(st.finish())
}

/// Streaming core over any reader — treated as a single live raw segment (one
/// unterminated final fragment tolerated). Public for callers that already hold
/// a reader; path-based [`reconstruct`] distinguishes sealed vs raw strictness.
pub fn reconstruct_reader<R: Read>(reader: R) -> Result<Recon> {
    let mut st = ReconState::new();
    run_segment(BufReader::new(reader), Segment::LiveRaw, &mut st)?;
    Ok(st.finish())
}

/// A capture segment: `Sealed` records are final and MUST fully parse; `LiveRaw`
/// is the growing tail where exactly one unterminated final fragment is ignored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Segment {
    Sealed,
    LiveRaw,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonValueRange {
    pub start: usize,
    pub end: usize,
}

/// Decode every complete concatenated JSON value from one physical record.
/// Ranges are offsets in the supplied reader; `start` includes any whitespace
/// separator before the value so random-access callers can trim it without
/// retaining the record.
pub fn decode_concatenated<R, T>(
    reader: R,
    mut visit: impl FnMut(T, JsonValueRange),
) -> serde_json::Result<()>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut stream = serde_json::Deserializer::from_reader(reader).into_iter::<T>();
    let mut start = 0;
    while let Some(value) = stream.next() {
        let value = value?;
        let end = stream.byte_offset();
        visit(value, JsonValueRange { start, end });
        start = end;
    }
    Ok(())
}

impl Segment {
    fn label(self) -> &'static str {
        match self {
            Segment::Sealed => "sealed",
            Segment::LiveRaw => "raw",
        }
    }
}

/// Accumulated reconstruction state, shared across a capture's segments.
struct ReconState {
    msgs: Vec<Value>,
    hash_dict: HashMap<String, Value>,
    key: String,
    harness: Option<Harness>,
    trailing: Vec<Value>,
    envelopes: usize,
}

impl ReconState {
    fn new() -> Self {
        ReconState {
            msgs: Vec::new(),
            hash_dict: HashMap::new(),
            key: String::from("messages"),
            harness: None,
            trailing: Vec::new(),
            envelopes: 0,
        }
    }

    /// Apply one parsed Envelope value: derive harness, track its (possibly
    /// final) completed response as the trailing output, and replay its delta.
    fn apply(&mut self, env: &Value) {
        self.envelopes += 1;
        // Derive harness identity once, envelope-first: the envelope field is
        // the captured wire truth; key == "input" resolves Codex only while
        // no envelope has said otherwise.
        match env
            .get("harness")
            .and_then(Value::as_str)
            .and_then(Harness::from_label)
        {
            Some(h) => self.harness = Some(h),
            None => {
                if self.harness.is_none()
                    && env
                        .pointer("/request/body_delta/history/key")
                        .and_then(Value::as_str)
                        == Some("input")
                {
                    self.harness = Some(Harness::Codex);
                }
            }
        }
        // The response of every envelope *before* the last is reflected in the
        // next request's history delta; only the final envelope's completed
        // response needs appending. Track it per-envelope, keeping only the last.
        self.trailing = extract_response_output(env, self.harness);
        // Codex stamps each replayed item with the turn it belongs to; the
        // request-side items of this turn carry it already (baked into the
        // wire), but the response-side items we append here don't — add it so
        // a fork's resume replays them byte-identically to a native resume.
        if self.harness == Some(Harness::Codex) {
            if let Some(turn_id) = env.get("turn_id").and_then(Value::as_str) {
                for item in &mut self.trailing {
                    if let Some(o) = item.as_object_mut() {
                        o.insert(
                            "internal_chat_message_metadata_passthrough".into(),
                            serde_json::json!({ "turn_id": turn_id }),
                        );
                    }
                }
            }
        }

        if let Some(h) = env.pointer("/request/body_delta/history") {
            if let Some(k) = h.get("key").and_then(Value::as_str) {
                self.key = k.to_string();
            }
            apply_delta(h, &mut self.msgs, &mut self.hash_dict);
        }
    }

    fn finish(mut self) -> Recon {
        let history_len = self.msgs.len();
        let trailing_appended = self.trailing.len();
        self.msgs.extend(self.trailing);
        Recon {
            key: self.key,
            harness: self.harness,
            history_len,
            messages: self.msgs,
            trailing_appended,
            envelopes: self.envelopes,
        }
    }
}

/// Process one segment record-by-record. A physical record is the bytes up to a
/// newline (the final record may be unterminated). Each record may hold one or
/// more concatenated complete Envelope JSON values (a historical concurrent-write
/// artifact) followed by optional whitespace — every complete value is applied.
/// Whitespace-only records contribute nothing. Non-whitespace residue that
/// cannot form complete Envelopes fails with the segment and one-based record
/// number (never echoing content), except one unterminated final fragment in a
/// `LiveRaw` segment, which is ignored.
fn run_segment<R: BufRead>(mut reader: R, segment: Segment, st: &mut ReconState) -> Result<()> {
    let mut record_no = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        record_no += 1;
        let terminated = buf.last() == Some(&b'\n');
        let end = buf.len() - terminated as usize;
        let content = &buf[..end];
        let Some(start) = content.iter().position(|b| !b.is_ascii_whitespace()) else {
            continue; // whitespace-only record
        };
        let stop = content
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .unwrap()
            + 1;
        let trimmed = &content[start..stop];

        if decode_concatenated(trimmed, |value: Value, _| st.apply(&value)).is_err() {
            if !terminated && segment == Segment::LiveRaw {
                continue; // one unterminated final fragment on a live tail
            }
            anyhow::bail!(
                "reconstruct: {} record {record_no}: incomplete or malformed JSON",
                segment.label()
            );
        }
    }
    Ok(())
}

/// commonPrefix: element-wise JSON equality.
fn common_prefix(a: &[Value], b: &[Value]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// Encode `body` against the prior request body as the on-disk `body_delta`
/// shape: `{set, remove, history: {key, prefix_length, append}}`. For object
/// bodies whose history key is an array, `apply_delta` (history) plus
/// set/remove replay reconstructs `body` exactly; degenerate bodies
/// (non-object, non-array history) encode as an empty delta and rely on
/// state.json for ground truth. `big_fields` are stored only when changed;
/// everything else is verbatim.
pub fn encode_delta(prior: &Value, body: &Value, history_key: &str, big_fields: &[&str]) -> Value {
    let empty = vec![];
    let history = body
        .get(history_key)
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let prior_history = prior
        .get(history_key)
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let prefix = common_prefix(prior_history, history);

    let mut set = Map::new();
    let mut remove: Vec<String> = vec![];
    if let Some(obj) = body.as_object() {
        for (k, v) in obj {
            if k == history_key {
                continue;
            }
            if big_fields.contains(&k.as_str()) {
                if prior.get(k) != Some(v) {
                    set.insert(k.clone(), v.clone());
                }
            } else {
                set.insert(k.clone(), v.clone());
            }
        }
        if let Some(pobj) = prior.as_object() {
            for k in pobj.keys() {
                if !obj.contains_key(k) && k != history_key {
                    remove.push(k.clone());
                }
            }
        }
    }

    json!({
        "set": set,
        "remove": remove,
        "history": { "key": history_key, "prefix_length": prefix, "append": history[prefix..] },
    })
}

/// Apply one history delta (append or content-addressed form), mirroring recon.mjs.
pub fn apply_delta(h: &Value, msgs: &mut Vec<Value>, hash_dict: &mut HashMap<String, Value>) {
    let append = h.get("append").and_then(Value::as_array);
    let prefix = h.get("prefix_length").and_then(Value::as_u64);
    if let (Some(append), Some(prefix)) = (append, prefix) {
        msgs.truncate(prefix as usize);
        msgs.extend(append.iter().cloned());
    } else if let Some(order) = h.get("order").and_then(Value::as_array) {
        if let Some(new) = h.get("new").and_then(Value::as_object) {
            for (k, v) in new {
                hash_dict.insert(k.clone(), v.clone());
            }
        }
        *msgs = order
            .iter()
            .filter_map(|x| x.as_str().and_then(|k| hash_dict.get(k)).cloned())
            .collect();
    } else if let Some(append) = append {
        msgs.extend(append.iter().cloned());
    }
}

/// Extract the assistant output carried by an envelope's completed response.
/// v1 envelopes carry `response.sse` (raw SSE text); v2 carry `response.events`
/// (parsed event array). Returns wire-shaped items ready to append to history:
/// Anthropic → one assistant message; Codex → the Responses output items.
fn extract_response_output(env: &Value, harness: Option<Harness>) -> Vec<Value> {
    let Some(resp) = env.get("response") else {
        return vec![];
    };
    if resp.get("complete").and_then(Value::as_bool) != Some(true) {
        return vec![];
    }
    let events: Vec<Value> = if let Some(evs) = resp.get("events").and_then(Value::as_array) {
        evs.clone()
    } else if let Some(sse) = resp.get("sse").and_then(Value::as_str) {
        parse_sse(sse)
    } else {
        return vec![];
    };
    if harness == Some(Harness::Codex) {
        codex_output(&events)
    } else {
        anthropic_output(&events)
    }
}

/// Codex Responses: prefer explicit output_item.done items; fall back to
/// response.completed's `response.output` (often empty with store:false).
fn codex_output(events: &[Value]) -> Vec<Value> {
    let done: Vec<Value> = events
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("response.output_item.done"))
        .filter_map(|e| e.get("item").cloned())
        .collect();
    if !done.is_empty() {
        return done;
    }
    events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(Value::as_str) == Some("response.completed"))
        .and_then(|e| e.pointer("/response/output"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Anthropic SSE accumulation: message_start seeds the message; content blocks
/// grow via content_block_start/_delta. Returns one assistant message, or none
/// if the stream never terminated (no message_stop).
fn anthropic_output(events: &[Value]) -> Vec<Value> {
    let stopped = events
        .iter()
        .any(|e| e.get("type").and_then(Value::as_str) == Some("message_stop"));
    if !stopped {
        return vec![];
    }
    let mut blocks: Vec<Value> = Vec::new();
    for e in events {
        match e.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                if let Some(b) = e.get("content_block") {
                    blocks.push(b.clone());
                }
            }
            Some("content_block_delta") => {
                let (Some(idx), Some(delta)) =
                    (e.get("index").and_then(Value::as_u64), e.get("delta"))
                else {
                    continue;
                };
                let Some(block) = blocks.get_mut(idx as usize) else {
                    continue;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => append_str(block, "text", delta.get("text")),
                    Some("thinking_delta") => append_str(block, "thinking", delta.get("thinking")),
                    Some("signature_delta") => {
                        append_str(block, "signature", delta.get("signature"))
                    }
                    Some("input_json_delta") => {
                        append_str(block, "partial_json", delta.get("partial_json"))
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // Finalize tool_use blocks: parse accumulated partial_json into `input`.
    for b in &mut blocks {
        if b.get("type").and_then(Value::as_str) == Some("tool_use") {
            if let Some(pj) = b.get("partial_json").and_then(Value::as_str) {
                if let Ok(input) = serde_json::from_str::<Value>(pj) {
                    b["input"] = input;
                }
            }
            if let Some(obj) = b.as_object_mut() {
                obj.remove("partial_json");
            }
        }
    }
    if blocks.is_empty() {
        return vec![];
    }
    vec![serde_json::json!({"role": "assistant", "content": blocks})]
}

fn append_str(block: &mut Value, field: &str, addition: Option<&Value>) {
    let Some(add) = addition.and_then(Value::as_str) else {
        return;
    };
    let existing = block.get(field).and_then(Value::as_str).unwrap_or("");
    block[field] = Value::String(format!("{existing}{add}"));
}

/// Parse SSE text into JSON data events, ignoring terminal and malformed lines.
pub fn parse_sse(sse: &str) -> Vec<Value> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_append(prefix: u64, role: &str, content: &str) -> String {
        json!({
            "harness": "claude-code",
            "request": { "body_delta": { "history": {
                "key": "messages", "prefix_length": prefix,
                "append": [{ "role": role, "content": content }],
            }}},
        })
        .to_string()
    }

    #[test]
    fn concatenated_record_recovers_every_envelope() {
        // The historical concurrent-write artifact: two complete Envelopes on one
        // physical record (`JSONJSON`) followed by a blank record.
        let a = env_append(0, "user", "a");
        let b = env_append(1, "user", "b");
        let raw = format!("{a}{b}\n\n");
        let r = reconstruct_reader(raw.as_bytes()).unwrap();
        assert_eq!(r.envelopes, 2, "both concatenated envelopes applied");
        assert_eq!(r.history_len, 2);
        assert_eq!(r.messages[0]["content"], "a");
        assert_eq!(r.messages[1]["content"], "b");
    }

    #[test]
    fn whitespace_only_records_ignored() {
        let a = env_append(0, "user", "a");
        let raw = format!("\n   \n{a}\n\t\n");
        let r = reconstruct_reader(raw.as_bytes()).unwrap();
        assert_eq!(r.envelopes, 1);
        assert_eq!(r.history_len, 1);
    }

    #[test]
    fn live_raw_ignores_one_unterminated_final_fragment() {
        let a = env_append(0, "user", "a");
        let raw = format!("{a}\n{{\"harness\":\"claude-code\",\"req"); // truncated tail, no newline
        let r = reconstruct_reader(raw.as_bytes()).unwrap();
        assert_eq!(r.envelopes, 1);
    }

    #[test]
    fn sealed_segment_fails_on_malformed_trailing_content() {
        // A sealed (fully terminated) capture must not silently drop a broken tail.
        let a = env_append(0, "user", "a");
        let sealed_bytes = format!("{a}\n{{\"harness\":\"claude-code\",\"req\n"); // terminated junk
        let tmp = tempfile::TempDir::new().unwrap();
        let zst = tmp.path().join("turns.jsonl.zst");
        std::fs::write(&zst, zstd::encode_all(sealed_bytes.as_bytes(), 3).unwrap()).unwrap();
        let err = reconstruct(&zst).unwrap_err().to_string();
        assert!(err.contains("sealed"), "error names the segment: {err}");
        assert!(
            !err.contains("harness"),
            "error must not echo content: {err}"
        );
    }

    #[test]
    fn terminated_junk_record_in_raw_fails() {
        // A non-final terminated record that can't form an Envelope is corruption.
        let a = env_append(0, "user", "a");
        let raw = format!("{a}\nnot json at all\n{}\n", env_append(1, "user", "b"));
        let err = reconstruct_reader(raw.as_bytes()).unwrap_err().to_string();
        assert!(err.contains("raw record 2"), "locates the record: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn retained_snapshot_survives_sealed_replace_and_detached_unlink() {
        let root = tempfile::TempDir::new().unwrap();
        let first = format!("{}\n", env_append(0, "user", "first"));
        let second = format!("{}\n", env_append(1, "user", "second"));
        let first_frame = zstd::encode_all(first.as_bytes(), 3).unwrap();
        let second_frame = zstd::encode_all(second.as_bytes(), 3).unwrap();
        let sealed = root.path().join("turns.jsonl.zst");
        std::fs::write(&sealed, &first_frame).unwrap();
        let detached = root.path().join(format!(
            "turns.jsonl.sealing-{}-{}",
            first_frame.len(),
            crate::vault::sha256_hex(second.as_bytes())
        ));
        std::fs::write(&detached, second.as_bytes()).unwrap();
        let merged = root.path().join(".merged");
        let mut committed = first_frame;
        committed.extend(second_frame);
        std::fs::write(&merged, committed).unwrap();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
        let writer_root = root.path().to_path_buf();
        let writer_detached = detached.clone();
        let writer = std::thread::spawn(move || {
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&writer_root)
                .unwrap();
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            // SAFETY: directory remains open and flock borrows its descriptor.
            assert_ne!(
                unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB,) },
                0,
                "writer acquired EX while reconstruction retained SH"
            );
            let error = std::io::Error::last_os_error().raw_os_error();
            assert!(
                error == Some(libc::EWOULDBLOCK) || error == Some(libc::EAGAIN),
                "unexpected flock error: {error:?}"
            );
            blocked_tx.send(()).unwrap();
            loop {
                // SAFETY: directory remains open and flock borrows its descriptor.
                if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                assert_eq!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::Interrupted
                );
            }
            std::fs::rename(
                writer_root.join(".merged"),
                writer_root.join("turns.jsonl.zst"),
            )
            .unwrap();
            std::fs::remove_file(writer_detached).unwrap();
        });
        ready_rx.recv().unwrap();

        let snapshot = ReconstructionSnapshot::with_hook(root.path(), || {
            go_tx.send(()).unwrap();
            blocked_rx.recv().unwrap();
        })
        .unwrap();
        writer.join().unwrap();
        let retained = reconstruct_snapshot(snapshot).unwrap();

        assert_eq!(retained.envelopes, 2);
        assert_eq!(retained.messages[0]["content"], "first");
        assert_eq!(retained.messages[1]["content"], "second");
        let fresh = reconstruct(&sealed).unwrap();
        assert_eq!(fresh.messages, retained.messages);
        assert_eq!(fresh.envelopes, 2);
    }

    #[test]
    fn retained_live_raw_reader_stops_at_its_snapshot_length() {
        use std::io::Write;

        let root = tempfile::TempDir::new().unwrap();
        let raw = root.path().join("turns.jsonl");
        let first = format!("{}\n", env_append(0, "user", "first"));
        let second = format!("{}\n", env_append(1, "user", "second"));
        std::fs::write(&raw, first).unwrap();

        let retained = reconstruct_canonical_with_hook(&raw, || {
            let mut file = OpenOptions::new().append(true).open(&raw).unwrap();
            file.write_all(second.as_bytes()).unwrap();
        })
        .unwrap();

        assert_eq!(retained.envelopes, 1);
        assert_eq!(retained.messages[0]["content"], "first");
        let fresh = reconstruct(&raw).unwrap();
        assert_eq!(fresh.envelopes, 2);
        assert_eq!(fresh.messages[1]["content"], "second");
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlink_fifo_directory_and_duplicate_inode_generations() {
        use std::os::unix::fs::symlink;

        let root = tempfile::TempDir::new().unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"outside evidence\n").unwrap();
        let raw = root.path().join("turns.jsonl");
        symlink(&outside, &raw).unwrap();
        assert!(reconstruct(&raw).unwrap_err().to_string().contains("open"));
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside evidence\n");
        std::fs::remove_file(&raw).unwrap();

        let fifo_name = CString::new(raw.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: fifo_name is a valid NUL-terminated pathname.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        assert!(reconstruct(&raw)
            .unwrap_err()
            .to_string()
            .contains("not a regular file"));
        std::fs::remove_file(&raw).unwrap();

        std::fs::create_dir(&raw).unwrap();
        assert!(reconstruct(&raw)
            .unwrap_err()
            .to_string()
            .contains("not a regular file"));
        std::fs::remove_dir(&raw).unwrap();

        let sealed = root.path().join("turns.jsonl.zst");
        let body = format!("{}\n", env_append(0, "user", "first"));
        std::fs::write(&sealed, zstd::encode_all(body.as_bytes(), 3).unwrap()).unwrap();
        std::fs::hard_link(&sealed, &raw).unwrap();
        assert!(reconstruct(&raw)
            .unwrap_err()
            .to_string()
            .contains("duplicate capture generation inode"));
    }

    /// Test-only inverse of `encode_delta`: replay set/remove over the prior
    /// body and `apply_delta` over its history.
    fn apply_body(prior: &Value, delta: &Value, history_key: &str) -> Value {
        let mut out = prior.as_object().cloned().unwrap_or_default();
        for (k, v) in delta["set"].as_object().unwrap() {
            out.insert(k.clone(), v.clone());
        }
        for k in delta["remove"].as_array().unwrap() {
            out.remove(k.as_str().unwrap());
        }
        let mut msgs: Vec<Value> = prior
            .get(history_key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut dict = HashMap::new();
        apply_delta(&delta["history"], &mut msgs, &mut dict);
        out.insert(history_key.to_string(), Value::Array(msgs));
        Value::Object(out)
    }

    fn msg(i: u64) -> Value {
        json!({ "role": if i.is_multiple_of(2) { "user" } else { "assistant" }, "content": format!("m{i}") })
    }

    #[test]
    fn encode_apply_round_trip_property() {
        const BIG: &[&str] = &["tools", "system"];
        // Deterministic LCG-driven generator: histories that share prefixes,
        // grow, compact, and diverge; big/small fields that change or vanish.
        let mut seed: u64 = 0x9e3779b97f4a7c15;
        let mut rand = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let mut prior = json!({});
        for _ in 0..200 {
            let mut body = Map::new();
            body.insert("model".into(), json!(format!("m{}", rand(3))));
            if rand(4) > 0 {
                body.insert("tools".into(), json!([{ "name": format!("t{}", rand(2)) }]));
            }
            if rand(4) > 0 {
                body.insert("system".into(), json!(format!("sys{}", rand(2))));
            }
            if rand(3) > 0 {
                body.insert("temperature".into(), json!(rand(10)));
            }
            let prior_len = prior
                .get("messages")
                .and_then(Value::as_array)
                .map_or(0, Vec::len) as u64;
            let keep = rand(prior_len + 1); // 0..=prior_len shared prefix
            let grow = rand(4);
            let history: Vec<Value> = (0..keep)
                .chain((100 + keep)..(100 + keep + grow)) // diverging tail
                .map(msg)
                .collect();
            body.insert("messages".into(), Value::Array(history));
            let body = Value::Object(body);

            let delta = encode_delta(&prior, &body, "messages", BIG);
            assert_eq!(
                apply_body(&prior, &delta, "messages"),
                body,
                "prior={prior} body={body} delta={delta}"
            );
            prior = body;
        }
    }

    #[test]
    fn encode_delta_round_trip_big_field_set_and_remove() {
        const BIG: &[&str] = &["tools", "system"];
        let prior = json!({
            "model": "m",
            "system": "sys",
            "tools": [{ "name": "t" }],
            "temperature": 0.5,
            "messages": [msg(0), msg(1)],
        });
        // Big field `tools` changes, `system` disappears, small field
        // `temperature` disappears; history compacts to a diverging singleton.
        let body = json!({
            "model": "m",
            "tools": [{ "name": "t2" }],
            "messages": [msg(7)],
        });
        let delta = encode_delta(&prior, &body, "messages", BIG);
        assert_eq!(delta["set"]["tools"], body["tools"]);
        assert!(delta["set"].get("system").is_none());
        let removed = delta["remove"].as_array().unwrap();
        assert!(removed.contains(&json!("system")));
        assert!(removed.contains(&json!("temperature")));
        assert_eq!(delta["history"]["prefix_length"], 0);
        assert_eq!(apply_body(&prior, &delta, "messages"), body);

        // Unchanged big field: absent from `set`, restored from prior on apply.
        let body2 = json!({
            "model": "m",
            "tools": [{ "name": "t" }],
            "messages": [msg(0), msg(1), msg(2)],
        });
        let delta2 = encode_delta(&prior, &body2, "messages", BIG);
        assert!(delta2["set"].get("tools").is_none());
        assert_eq!(delta2["history"]["prefix_length"], 2);
        // `system` was dropped, `tools` kept-but-deduped: apply must restore
        // exactly body2, tools included.
        assert_eq!(apply_body(&prior, &delta2, "messages"), body2);
    }
}
