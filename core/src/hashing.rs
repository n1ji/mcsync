//! File hashes used for mod resolution: sha1/sha512 (Modrinth) and the
//! CurseForge murmur2 fingerprint.

use crate::error::{IoContext, Result};
use sha1::Digest;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHashes {
    pub sha1: String,
    pub sha512: String,
    pub size: u64,
    pub murmur2: u32,
}

pub fn hash_file(path: &Path) -> Result<FileHashes> {
    let bytes = std::fs::read(path).at(path)?;
    Ok(hash_bytes(&bytes))
}

pub fn hash_bytes(bytes: &[u8]) -> FileHashes {
    FileHashes {
        sha1: hex::encode(sha1::Sha1::digest(bytes)),
        sha512: hex::encode(sha2::Sha512::digest(bytes)),
        size: bytes.len() as u64,
        murmur2: curseforge_fingerprint(bytes),
    }
}

pub fn sha1_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).at(path)?;
    let mut hasher = sha1::Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).at(path)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// CurseForge fingerprint: MurmurHash2 (seed 1) over the file with the
/// whitespace bytes 0x09, 0x0A, 0x0D and 0x20 removed.
pub fn curseforge_fingerprint(bytes: &[u8]) -> u32 {
    let normalized: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|b| !matches!(b, 9 | 10 | 13 | 32))
        .collect();
    murmur2(&normalized, 1)
}

fn murmur2(data: &[u8], seed: u32) -> u32 {
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;
    let len = data.len();
    let mut h: u32 = seed ^ (len as u32);
    let mut chunks = data.chunks_exact(4);
    for chunk in &mut chunks {
        let mut k = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h = h.wrapping_mul(M);
        h ^= k;
    }
    let rem = chunks.remainder();
    match rem.len() {
        3 => {
            h ^= (rem[2] as u32) << 16;
            h ^= (rem[1] as u32) << 8;
            h ^= rem[0] as u32;
            h = h.wrapping_mul(M);
        }
        2 => {
            h ^= (rem[1] as u32) << 8;
            h ^= rem[0] as u32;
            h = h.wrapping_mul(M);
        }
        1 => {
            h ^= rem[0] as u32;
            h = h.wrapping_mul(M);
        }
        _ => {}
    }
    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn murmur2_known_vectors() {
        // Empty input: only the final avalanche runs. Real-file coverage is in
        // tests/real_fingerprint.rs.
        assert_eq!(murmur2(b"", 1), 0x5bd1_5e36);
    }

    #[test]
    fn fingerprint_ignores_whitespace() {
        assert_eq!(
            curseforge_fingerprint(b"a b\tc\r\nd"),
            curseforge_fingerprint(b"abcd")
        );
    }

    #[test]
    fn sha_hashes() {
        let h = hash_bytes(b"abc");
        assert_eq!(h.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert!(h.sha512.starts_with("ddaf35a193617aba"));
        assert_eq!(h.size, 3);
    }
}
