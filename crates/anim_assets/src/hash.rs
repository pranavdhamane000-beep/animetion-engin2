//! Streaming BLAKE3 and the fixed-width hash form (spec 11.3, 11.6, 11.8).
//!
//! Two jobs, both narrow. The first is [`ContentHash`]: thirty-two bytes with exactly
//! one text spelling — 64 lowercase hex — because that text is also the object file
//! name, and a hash that could be written two ways would give one object two paths.
//! The strictness rule is the one ADR-0004 set for `Rational` and [`anim_core::id`]
//! set for UUIDs: parse, re-render, require the text back unchanged.
//!
//! The second is [`StreamingHash`], which exists because spec 11.6 hashes "the
//! original stored source bytes" and spec 11.12 names an import larger than a memory
//! buffer as a test case. A one-shot `blake3::hash(&whole_file)` would satisfy the
//! first and fail the second.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{io_error, AssetError, CHUNK_SIZE};

/// The digest width, spelled once. Spec 11.8's `ContentHash(pub [u8; 32])`.
const DIGEST_WIDTH: usize = 32;

/// The BLAKE3 digest of one stored object's bytes.
///
/// Identity of *content*, deliberately separate from `AssetId`'s identity of
/// a logical asset: spec 11.6 keeps the two axes apart, so a reimport that changes
/// bytes moves this and leaves the id alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContentHash(pub [u8; DIGEST_WIDTH]);

impl ContentHash {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; DIGEST_WIDTH]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; DIGEST_WIDTH] {
        &self.0
    }

    /// Parse the canonical spelling: exactly 64 lowercase hex characters.
    ///
    /// Uppercase is refused rather than folded, and the simple 64-character form
    /// without hyphens is not a hash this crate will guess at, because a tolerant
    /// parser accepts a file name that the store would then write at a *different*
    /// path from the one it verified.
    pub fn from_hex(raw: &str) -> Result<Self, AssetError> {
        let rejected = || AssetError::InvalidHash {
            raw: raw.to_owned(),
        };
        if raw.len() != DIGEST_WIDTH * 2
            || !raw
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(rejected());
        }
        let mut bytes = [0u8; DIGEST_WIDTH];
        hex::decode_to_slice(raw, &mut bytes).map_err(|_| rejected())?;
        Ok(Self(bytes))
    }

    /// The canonical text, which is also the object file name (spec 11.7).
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The two directory levels this hash sorts under, as lowercase hex pairs.
    ///
    /// Taken from the digest rather than from a filename or a display name, which is
    /// the whole of spec 11.6's "path construction accepts only a validated fixed-size
    /// hash, preventing traversal through filenames".
    #[must_use]
    pub fn path_shards(&self) -> (String, String) {
        let hex = self.to_hex();
        (hex[..2].to_owned(), hex[2..4].to_owned())
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for ContentHash {
    type Err = AssetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl PartialOrd for ContentHash {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for ContentHash {
    /// Over the raw bytes, spelled out here for the reason [`anim_core::id`] gives for
    /// the same choice: a manifest and a dependency list are ordered containers, and
    /// their order must not follow whatever a dependency crate's `derive(Ord)` happens
    /// to do.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.as_slice().cmp(other.0.as_slice())
    }
}

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::from_hex(&raw).map_err(serde::de::Error::custom)
    }
}

/// A BLAKE3 digest built incrementally over bytes that are never all resident.
///
/// The byte count travels with the hasher because spec 11.7 asks for a "checked byte
/// count" and the record stores both axes: `content_hash` *and* `byte_length`. A count
/// that wrapped would make an object look smaller than it is, and the length check in
/// `verify` is a cheap tripwire for exactly that, so it has to be trustworthy.
#[derive(Clone, Debug)]
pub struct StreamingHash {
    hasher: blake3::Hasher,
    bytes: u64,
}

impl StreamingHash {
    #[must_use]
    pub fn new() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
            bytes: 0,
        }
    }

    /// Absorb one chunk and count it.
    pub fn update(&mut self, chunk: &[u8]) -> Result<(), AssetError> {
        let next = self
            .bytes
            .checked_add(chunk.len() as u64)
            .ok_or(AssetError::SizeOverflow { bytes: self.bytes })?;
        self.hasher.update(chunk);
        self.bytes = next;
        Ok(())
    }

    /// Bytes absorbed so far.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The digest so far. `blake3`'s hasher is not consumed by finalizing, so a caller
    /// may report a partial digest while streaming — the progress line an operator
    /// watches during a large import is not a second hash pass.
    #[must_use]
    pub fn finalize(&self) -> ContentHash {
        ContentHash(*self.hasher.finalize().as_bytes())
    }
}

impl Default for StreamingHash {
    fn default() -> Self {
        Self::new()
    }
}

/// Digest a whole slice in one step. For fixtures and tests, not for objects.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> ContentHash {
    ContentHash(*blake3::hash(bytes).as_bytes())
}

/// Digest a file in bounded chunks, returning the digest and the length.
///
/// The length is read from the same pass rather than from `metadata`, so a
/// verification that reports agreement is agreeing about bytes it actually looked at.
pub fn hash_file(path: &Path) -> Result<(ContentHash, u64), AssetError> {
    let mut file =
        File::open(path).map_err(|e| io_error("open for hashing", path, e.to_string()))?;
    let mut stream = StreamingHash::new();
    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| io_error("read for hashing", path, e.to_string()))?;
        if read == 0 {
            break;
        }
        stream.update(&buffer[..read])?;
    }
    Ok((stream.finalize(), stream.bytes()))
}

/// Digest a stream in bounded chunks (spec 11.7's "read bounded chunks; update BLAKE3
/// and checked byte count").
///
/// An [`std::io::ErrorKind::Interrupted`] is retried here, as std requires, because
/// this is a read of bytes already decided upon. The *import* path treats that same
/// error kind as cancellation, and the difference is deliberate: see
/// [`crate::import`].
pub fn hash_stream<R: Read>(source: &mut R) -> Result<(ContentHash, u64), AssetError> {
    let mut stream = StreamingHash::new();
    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => stream.update(&buffer[..read])?,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(AssetError::Io {
                    context: "read source stream",
                    path: String::new(),
                    detail: e.to_string(),
                })
            }
        }
    }
    Ok((stream.finalize(), stream.bytes()))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    /// The published BLAKE3 input pattern: a repeating sequence of 251 bytes,
    /// `0, 1, 2, …, 250`, truncated to the length under test. Every vector in the
    /// official table is built from it, so a fixture that reproduces it reproduces
    /// the table.
    fn published_input(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn hex_form_is_strict_and_round_trips() {
        let hash = hash_bytes(b"anim");
        let text = hash.to_hex();
        assert_eq!(text.len(), 64);
        assert_eq!(text.to_lowercase(), text);
        assert_eq!(ContentHash::from_hex(&text).unwrap(), hash);
        assert_eq!(ContentHash::from_str(&text).unwrap(), hash);

        for bad in [
            String::new(),
            text[..63].to_owned(),
            format!("{text}0"),
            text.to_uppercase(),
            {
                let mut mixed = text.clone();
                mixed.replace_range(10..12, &text[10..12].to_uppercase());
                mixed
            },
            format!("0x{text}"),
            format!("{text} "),
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(),
        ] {
            let e = ContentHash::from_hex(&bad).unwrap_err();
            assert!(matches!(e, AssetError::InvalidHash { .. }), "{e}");
            assert_eq!(e.code().as_str(), "asset.invalid_hash");
        }
    }

    #[test]
    fn json_is_the_one_lowercase_string_form() {
        let hash = hash_bytes(b"anim");
        let json = serde_json::to_string(&hash).unwrap();
        assert_eq!(json, format!("\"{}\"", hash.to_hex()));
        assert_eq!(serde_json::from_str::<ContentHash>(&json).unwrap(), hash);
        // The 32-element byte array `serde` would otherwise accept is not a
        // persisted form here, for the reason `anim_core::id` gives for UUIDs.
        let as_array = serde_json::to_string(&hash.0).unwrap();
        assert!(serde_json::from_str::<ContentHash>(&as_array).is_err());
    }

    #[test]
    fn shards_come_from_the_digest_and_are_two_hex_chars_each() {
        let hash = ContentHash::from_hex(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )
        .unwrap();
        assert_eq!(hash.path_shards(), ("ab".to_owned(), "cd".to_owned()));
        // A lowercase-only form is what makes the derived path safe: no separator can
        // reach it, so no filename can either.
        for c in hash.path_shards().0.chars() {
            assert!(
                c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
                "{c} must be lowercase hex"
            );
        }
    }

    #[test]
    fn streaming_matches_the_one_shot_digest() {
        let payload = published_input(100_000);
        let mut stream = StreamingHash::new();
        for chunk in payload.chunks(4096) {
            stream.update(chunk).unwrap();
        }
        assert_eq!(stream.bytes(), 100_000);
        assert_eq!(stream.finalize(), hash_bytes(&payload));
        // A partial finalize does not disturb the running state.
        let mut small = StreamingHash::new();
        small.update(&payload[..1000]).unwrap();
        let mid = small.finalize();
        small.update(&payload[1000..]).unwrap();
        assert_ne!(mid, small.finalize(), "a prefix is not the whole digest");
        assert_eq!(small.finalize(), hash_bytes(&payload));
    }

    #[test]
    fn byte_count_is_checked_not_wrapped() {
        let mut stream = StreamingHash::new();
        stream.bytes = u64::MAX - 1;
        stream.update(&[0, 1]).unwrap_err();
        assert_eq!(
            stream.bytes,
            u64::MAX - 1,
            "a refused chunk must not advance the count"
        );
    }

    #[test]
    fn order_is_the_byte_order() {
        let low = ContentHash::from_bytes([0x00; 32]);
        let high = ContentHash::from_bytes([0xff; 32]);
        assert!(low < high);
        let mut hashes = [
            ContentHash::from_bytes([0x10; 32]),
            high,
            ContentHash::from_bytes([0x0f; 32]),
            low,
        ];
        hashes.sort();
        assert_eq!(hashes, [low, hashes[1], hashes[2], high]);
        assert_eq!(hashes[1], ContentHash::from_bytes([0x0f; 32]));
    }

    #[test]
    fn published_blake3_vectors_match_the_streaming_path() {
        // Spec 11.12's first named check. The table is the official one, fetched from
        // the reference implementation's repository and vendored so this test stays
        // runnable offline; see DECISIONS.md #27 for provenance.
        let table = include_str!("../tests/fixtures/blake3_test_vectors.json");
        let vectors: PublishedVectors = serde_json::from_str(table).unwrap();
        assert_eq!(
            vectors.key, "whats the Elvish word for friend",
            "the fixture is the keyed-hash table; only the unkeyed column is used here"
        );
        assert!(
            vectors.cases.len() > 30,
            "the fixture must be the published table, not a hand-picked subset"
        );
        for case in &vectors.cases {
            let input = published_input(case.input_len);
            // One pass in memory, then the same bytes in 64 KiB chunks, then the
            // same bytes through a file: three paths, one digest.
            let expected = &case.hash[..64];
            assert_eq!(
                hash_bytes(&input).to_hex(),
                expected,
                "one-shot digest for {} bytes",
                case.input_len
            );
            let (streamed, streamed_len) = hash_stream(&mut input.as_slice()).unwrap();
            assert_eq!(
                streamed.to_hex(),
                expected,
                "streamed digest for {} bytes",
                case.input_len
            );
            assert_eq!(streamed_len, case.input_len as u64);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("object");
            std::fs::write(&path, &input).unwrap();
            let (from_file, file_len) = hash_file(&path).unwrap();
            assert_eq!(
                from_file.to_hex(),
                expected,
                "file digest for {} bytes",
                case.input_len
            );
            assert_eq!(file_len, case.input_len as u64);
        }
    }

    /// The shape of the published vector file, with only the fields this crate uses.
    #[derive(serde::Deserialize)]
    struct PublishedVectors {
        key: String,
        cases: Vec<PublishedCase>,
    }

    #[derive(serde::Deserialize)]
    struct PublishedCase {
        input_len: usize,
        /// The published field is 131 bytes of extended output; the first 32 are the
        /// digest this crate computes.
        hash: String,
    }
}
