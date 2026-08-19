//! Content hashing. Every digest in the vault — capture generation names,
//! detached-generation proofs, staging keys — is a lowercase hex SHA-256
//! produced here.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(data);
    format!("{:x}", hash.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("hash capture generation {}", path.display()))?;
    sha256_reader(file)
}

pub fn sha256_reader(mut reader: impl Read) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Decode the concatenated-zstd suffix at `offset` and hash its exact
/// uncompressed bytes. Reconstruction and Sealing use this same evidence proof.
pub fn decoded_zstd_suffix_digest(mut reader: impl Read + Seek, offset: u64) -> Result<String> {
    reader.seek(SeekFrom::Start(offset))?;
    let decoder = zstd::Decoder::new(reader)?;
    sha256_reader(decoder)
}
