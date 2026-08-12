//! Streaming reconstruction of the final message history from a turns.jsonl
//! (raw or zstd) capture, plus the `body_delta` encoder (`encode_delta`) that
//! plant uses at capture time — encode and apply live side by side so they
//! can't drift. Memory is bounded by the largest single envelope plus the
//! final history — the archive is never loaded whole.

mod observed;

use anyhow::{Context, Result};
use observed::ReconState;
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

/// Harness identity of a capture, derived during reconstruction.
///
/// Precedence: the first recognized envelope `harness` field is ground truth
/// and later recognized labels must agree. A history key of "input" is only a
/// provisional Codex inference. Only when neither resolves (`Recon::harness`
/// is `None`) may callers fall back to meta.harness.
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

    /// Derive harness identity from one Envelope, preserving prior truth when
    /// the current Envelope is legacy and carries no identifying field.
    pub fn from_envelope(env: &Value, current: Option<Harness>) -> Option<Harness> {
        env.get("harness")
            .and_then(Value::as_str)
            .and_then(Harness::from_label)
            .or_else(|| {
                if current.is_none()
                    && env
                        .pointer("/request/body_delta/history/key")
                        .and_then(Value::as_str)
                        == Some("input")
                {
                    Some(Harness::Codex)
                } else {
                    current
                }
            })
    }
}

/// Result of reconstructing a capture.
#[derive(Debug, Clone)]
pub struct ObservedMessage {
    pub message: Value,
    pub in_final_replay: bool,
    pub observed_at: Option<String>,
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
    /// Every message occurrence observed in request histories or completed output.
    pub observed_messages: Vec<ObservedMessage>,
    /// True when one or more history deltas could not be replayed.
    pub partial: bool,
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
    let mut state = ReconState::new();
    for_each_envelope(path, |envelope| state.apply(envelope))?;
    Ok(state.finish())
}

/// Stream every Envelope from one retained capture snapshot to `visit`.
///
/// Canonical siblings and detached generations are inventoried once under the
/// session lock, then visited in reconstruction order with strict lineage and
/// live-tail semantics.
pub fn for_each_envelope(path: &Path, mut visit: impl FnMut(&Value) -> Result<()>) -> Result<()> {
    let name = path.file_name().and_then(|name| name.to_str());
    let canonical = matches!(name, Some("turns.jsonl" | "turns.jsonl.zst"))
        || name.is_some_and(|name| name.starts_with("turns.jsonl.sealing-"));
    if canonical {
        let directory = path.parent().unwrap_or_else(|| Path::new(""));
        let snapshot = ReconstructionSnapshot::with_hook(directory, || {})?;
        return visit_snapshot(&snapshot, &mut visit);
    }
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        let dec = zstd::Decoder::new(file).context("zstd decoder")?;
        run_segment(BufReader::new(dec), Segment::Sealed, &mut visit)?;
    } else {
        run_segment(BufReader::new(file), Segment::LiveRaw, &mut visit)?;
    }
    Ok(())
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
    let mut state = ReconState::new();
    visit_snapshot(&snapshot, &mut |envelope| state.apply(envelope))?;
    Ok(state.finish())
}

fn visit_snapshot(
    snapshot: &ReconstructionSnapshot,
    visit: &mut impl FnMut(&Value) -> Result<()>,
) -> Result<()> {
    if let Some(sealed) = &snapshot.sealed {
        let dec = zstd::Decoder::new(sealed.reader()?).context("zstd decoder")?;
        run_segment(BufReader::new(dec), Segment::Sealed, visit)?;
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
                visit,
            )?,
        }
    }
    if let Some(raw) = &snapshot.raw {
        run_segment(BufReader::new(raw.reader()?), Segment::LiveRaw, visit)?;
    }
    Ok(())
}

/// Streaming core over any reader — treated as a single live raw segment (one
/// unterminated final fragment tolerated). Public for callers that already hold
/// a reader; path-based [`reconstruct`] distinguishes sealed vs raw strictness.
pub fn reconstruct_reader<R: Read>(reader: R) -> Result<Recon> {
    let mut st = ReconState::new();
    run_segment(BufReader::new(reader), Segment::LiveRaw, &mut |env| {
        st.apply(env)
    })?;
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

/// Process one segment record-by-record. A physical record is the bytes up to a
/// newline (the final record may be unterminated). Each record may hold one or
/// more concatenated complete Envelope JSON values (a historical concurrent-write
/// artifact) followed by optional whitespace — every complete value is applied.
/// Whitespace-only records contribute nothing. Non-whitespace residue that
/// cannot form complete Envelopes fails with the segment and one-based record
/// number (never echoing content), except one unterminated final fragment in a
/// `LiveRaw` segment, which is ignored.
fn run_segment<R: BufRead>(
    mut reader: R,
    segment: Segment,
    visit: &mut impl FnMut(&Value) -> Result<()>,
) -> Result<()> {
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

        let mut stream = serde_json::Deserializer::from_slice(trimmed).into_iter::<Value>();
        let mut consumed = 0usize;
        loop {
            match stream.next() {
                Some(Ok(v)) => {
                    consumed = stream.byte_offset();
                    visit(&v).with_context(|| {
                        format!("reconstruct: {} record {record_no}", segment.label())
                    })?;
                }
                Some(Err(_)) => {
                    let rest = &trimmed[consumed..];
                    if !rest.iter().any(|b| !b.is_ascii_whitespace()) {
                        break; // only trailing whitespace remained
                    }
                    // Non-whitespace residue that cannot form a complete Envelope.
                    if !terminated && segment == Segment::LiveRaw {
                        break; // one unterminated final fragment on a live tail
                    }
                    anyhow::bail!(
                        "reconstruct: {} record {record_no}: incomplete or malformed JSON",
                        segment.label()
                    );
                }
                None => break,
            }
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

#[derive(Debug)]
struct HistoryTransition {
    retained_prefix: usize,
}

#[derive(Debug)]
struct DeltaError {
    message: &'static str,
    recoverable: bool,
}

impl DeltaError {
    fn invalid(message: &'static str) -> Self {
        Self {
            message,
            recoverable: false,
        }
    }

    fn broken_lineage(message: &'static str) -> Self {
        Self {
            message,
            recoverable: true,
        }
    }

    fn recoverable(&self) -> bool {
        self.recoverable
    }
}

impl std::fmt::Display for DeltaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for DeltaError {}

/// Apply one history delta (append or content-addressed form), mirroring recon.mjs.
pub fn apply_delta(
    h: &Value,
    msgs: &mut Vec<Value>,
    hash_dict: &mut HashMap<String, Value>,
) -> Result<()> {
    apply_delta_transition(h, msgs, hash_dict)
        .map(|_| ())
        .map_err(Into::into)
}

fn apply_delta_transition(
    h: &Value,
    msgs: &mut Vec<Value>,
    hash_dict: &mut HashMap<String, Value>,
) -> std::result::Result<HistoryTransition, DeltaError> {
    let h = h
        .as_object()
        .ok_or_else(|| DeltaError::invalid("history delta must be an object"))?;
    let append = h
        .get("append")
        .map(|value| {
            value
                .as_array()
                .ok_or_else(|| DeltaError::invalid("history append must be an array"))
        })
        .transpose()?;
    let prefix = h
        .get("prefix_length")
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .ok_or_else(|| DeltaError::invalid("history prefix must be an unsigned index"))
        })
        .transpose()?;
    let order = h
        .get("order")
        .map(|value| {
            value
                .as_array()
                .ok_or_else(|| DeltaError::invalid("history order must be an array"))
        })
        .transpose()?;
    let new = h
        .get("new")
        .map(|value| {
            value
                .as_object()
                .ok_or_else(|| DeltaError::invalid("history new must be an object"))
        })
        .transpose()?;

    let retained_prefix = match (append, prefix, order) {
        (Some(append), Some(prefix), None) => {
            if new.is_some() || prefix > msgs.len() {
                return Err(DeltaError::broken_lineage("invalid append history lineage"));
            }
            msgs.truncate(prefix);
            msgs.extend(append.iter().cloned());
            prefix
        }
        (Some(append), None, None) => {
            if new.is_some() {
                return Err(DeltaError::invalid("invalid legacy append history"));
            }
            let retained = msgs.len();
            msgs.extend(append.iter().cloned());
            retained
        }
        (None, None, Some(order)) => {
            let mut resolved = Vec::with_capacity(order.len());
            for entry in order {
                let key = entry
                    .as_str()
                    .ok_or_else(|| DeltaError::invalid("history order entry must be a string"))?;
                resolved.push(
                    new.and_then(|values| values.get(key))
                        .or_else(|| hash_dict.get(key))
                        .cloned()
                        .ok_or_else(|| {
                            DeltaError::broken_lineage("history order entry does not resolve")
                        })?,
                );
            }
            let retained = common_prefix(msgs, &resolved);
            if let Some(new) = new {
                hash_dict.extend(new.iter().map(|(key, value)| (key.clone(), value.clone())));
            }
            *msgs = resolved;
            retained
        }
        _ => return Err(DeltaError::invalid("unrecognized history delta shape")),
    };
    Ok(HistoryTransition { retained_prefix })
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
mod tests;
