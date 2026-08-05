use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use super::{Journal, Stage};
use crate::capture::session_fs::SessionDirectory;

const IO_CHUNK: usize = 64 * 1024;

pub(super) struct RawGeneration {
    _directory: SessionDirectory,
    file: fs::File,
    path: PathBuf,
}

impl RawGeneration {
    pub(super) fn open(directory: &Path, create: bool) -> Result<Option<Self>, String> {
        let directory_handle = SessionDirectory::open(directory)
            .map_err(|error| format!("capture commit: {error}"))?;
        let Some(file) = directory_handle
            .open_append("turns.jsonl", create)
            .map_err(|error| format!("capture commit: {error}"))?
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            _directory: directory_handle,
            file,
            path: directory.join("turns.jsonl"),
        }))
    }

    fn read_exact_at(&self, mut bytes: &mut [u8], mut offset: u64) -> Result<(), String> {
        while !bytes.is_empty() {
            let read = self.file.read_at(bytes, offset).map_err(|error| {
                format!("capture commit: read {}: {error}", self.path.display())
            })?;
            if read == 0 {
                return Err(format!(
                    "capture commit: unexpected end of {}",
                    self.path.display()
                ));
            }
            offset += read as u64;
            bytes = &mut bytes[read..];
        }
        Ok(())
    }

    fn find_forward(
        &self,
        range: Range<u64>,
        predicate: impl Fn(u8) -> bool,
    ) -> Result<Option<u64>, String> {
        let mut offset = range.start;
        let mut chunk = vec![0; IO_CHUNK];
        while offset < range.end {
            let len = (range.end - offset).min(IO_CHUNK as u64) as usize;
            self.read_exact_at(&mut chunk[..len], offset)?;
            if let Some(position) = chunk[..len].iter().position(|byte| predicate(*byte)) {
                return Ok(Some(offset + position as u64));
            }
            offset += len as u64;
        }
        Ok(None)
    }

    fn find_backward(
        &self,
        range: Range<u64>,
        predicate: impl Fn(u8) -> bool,
    ) -> Result<Option<u64>, String> {
        let mut end = range.end;
        let mut chunk = vec![0; IO_CHUNK];
        while end > range.start {
            let start = end.saturating_sub(IO_CHUNK as u64).max(range.start);
            let len = (end - start) as usize;
            self.read_exact_at(&mut chunk[..len], start)?;
            if let Some(position) = chunk[..len].iter().rposition(|byte| predicate(*byte)) {
                return Ok(Some(start + position as u64));
            }
            end = start;
        }
        Ok(None)
    }

    fn range_matches_prefix(&self, range: &Range<u64>, expected: &[u8]) -> Result<bool, String> {
        let length = range.end - range.start;
        if length > expected.len() as u64 {
            return Ok(false);
        }
        let mut offset = 0usize;
        let mut chunk = vec![0; IO_CHUNK];
        while offset < length as usize {
            let len = (length as usize - offset).min(IO_CHUNK);
            self.read_exact_at(&mut chunk[..len], range.start + offset as u64)?;
            if chunk[..len] != expected[offset..offset + len] {
                return Ok(false);
            }
            offset += len;
        }
        Ok(true)
    }

    pub(super) fn append_record(&mut self, serialized: &[u8]) -> Result<(), String> {
        self.file
            .write_all(serialized)
            .and_then(|_| self.file.write_all(b"\n"))
            .map_err(|error| format!("capture commit: append {}: {error}", self.path.display()))
    }

    fn truncate(&self, offset: u64) -> Result<(), String> {
        self.file
            .set_len(offset)
            .map_err(|error| format!("capture commit: truncate {}: {error}", self.path.display()))
    }
}

struct FileRange<'a> {
    raw: &'a RawGeneration,
    offset: u64,
    end: u64,
}

impl Read for FileRange<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let len = (self.end - self.offset).min(bytes.len() as u64) as usize;
        if len == 0 {
            return Ok(0);
        }
        let read = self.raw.file.read_at(&mut bytes[..len], self.offset)?;
        self.offset += read as u64;
        Ok(read)
    }
}

#[derive(Deserialize)]
struct EnvelopeIdentity {
    request_id: String,
}

pub(super) enum CaptureTail {
    Blank,
    ValidTerminated {
        range: Range<u64>,
        request_id: String,
    },
    MalformedTerminated,
    Unterminated {
        range: Range<u64>,
    },
}

pub(super) fn capture_tail(raw: &RawGeneration) -> Result<CaptureTail, String> {
    let length = raw
        .file
        .metadata()
        .map_err(|error| format!("capture commit: stat {}: {error}", raw.path.display()))?
        .len();
    let Some(last_content) = raw.find_backward(0..length, |byte| !byte.is_ascii_whitespace())?
    else {
        return Ok(CaptureTail::Blank);
    };
    let start = raw
        .find_backward(0..last_content, |byte| byte == b'\n')?
        .map_or(0, |newline| newline + 1);
    let Some(record_end) = raw.find_forward(last_content + 1..length, |byte| byte == b'\n')? else {
        return Ok(CaptureTail::Unterminated {
            range: start..length,
        });
    };

    let mut invalid_identity = false;
    let mut final_value = None;
    let decoded = vaultr::recon::decode_concatenated(
        FileRange {
            raw,
            offset: start,
            end: record_end,
        },
        |identity: EnvelopeIdentity, range| {
            if uuid::Uuid::parse_str(&identity.request_id).is_err() {
                invalid_identity = true;
            }
            final_value = Some((range, identity.request_id));
        },
    );
    let Some((range, request_id)) = final_value else {
        return Ok(CaptureTail::MalformedTerminated);
    };
    if decoded.is_err() || invalid_identity {
        return Ok(CaptureTail::MalformedTerminated);
    }
    let range_start = start + range.start as u64;
    let range_end = start + range.end as u64;
    let Some(value_start) =
        raw.find_forward(range_start..range_end, |byte| !byte.is_ascii_whitespace())?
    else {
        return Ok(CaptureTail::MalformedTerminated);
    };
    let value_end = raw
        .find_backward(value_start..range_end, |byte| !byte.is_ascii_whitespace())?
        .expect("value range has non-whitespace")
        + 1;
    Ok(CaptureTail::ValidTerminated {
        range: value_start..value_end,
        request_id,
    })
}

fn reconcile_append(raw: &mut RawGeneration, envelope: &Value) -> Result<(), String> {
    let serialized = serde_json::to_vec(envelope).map_err(|error| error.to_string())?;
    let request_id = envelope.get("request_id").and_then(Value::as_str);
    match capture_tail(raw)? {
        CaptureTail::Blank => raw.append_record(&serialized),
        CaptureTail::ValidTerminated {
            range,
            request_id: tail_request_id,
        } if Some(tail_request_id.as_str()) == request_id => {
            if range.end - range.start == serialized.len() as u64
                && raw.range_matches_prefix(&range, &serialized)?
            {
                Ok(())
            } else {
                Err("capture commit: committed envelope conflicts with stage".into())
            }
        }
        CaptureTail::ValidTerminated { .. } => raw.append_record(&serialized),
        CaptureTail::MalformedTerminated => {
            Err("capture commit: malformed terminated capture tail".into())
        }
        CaptureTail::Unterminated { range } if raw.range_matches_prefix(&range, &serialized)? => {
            raw.truncate(range.start)?;
            raw.append_record(&serialized)
        }
        CaptureTail::Unterminated { .. } => {
            Err("capture commit: persisted tail conflicts with stage".into())
        }
    }
}

fn committed_exactly(raw: &RawGeneration, envelope: &Value) -> Result<bool, String> {
    let serialized = serde_json::to_vec(envelope).map_err(|error| error.to_string())?;
    let CaptureTail::ValidTerminated { range, .. } = capture_tail(raw)? else {
        return Ok(false);
    };
    Ok(range.end - range.start == serialized.len() as u64
        && raw.range_matches_prefix(&range, &serialized)?)
}

pub(super) fn commit_stage(journal: &mut Journal, stage: &Stage) -> Result<(), String> {
    let next = journal.require_order()?.next_to_drain;
    if stage.sequence < next {
        let committed = match RawGeneration::open(&journal.dir, false)? {
            Some(raw) => committed_exactly(&raw, &stage.envelope)?,
            None => false,
        };
        if stage.sequence + 1 != next || !committed {
            return Err(format!(
                "capture commit: retired stage conflicts at {}",
                stage.path.display()
            ));
        }
        return fs::remove_file(&stage.path).map_err(|error| {
            format!(
                "capture commit: remove retired stage {}: {error}",
                stage.path.display()
            )
        });
    }
    if stage.sequence != next {
        return Err(format!(
            "capture commit: stage sequence gap at {}",
            stage.path.display()
        ));
    }
    let mut raw = RawGeneration::open(&journal.dir, true)?
        .expect("create=true returns a raw generation handle");
    reconcile_append(&mut raw, &stage.envelope)?;
    {
        let order = journal.require_order_mut()?;
        order.next_to_drain += 1;
        order.pending.remove(&stage.sequence);
    }
    journal.persist()?;
    fs::remove_file(&stage.path).map_err(|error| {
        format!(
            "capture commit: remove stage {}: {error}",
            stage.path.display()
        )
    })
}
