//! State sync types.
//!
//! Note [Manifest Hash]
//! ====================
//!
//! This note describes how hashes included into the manifest are computed.
//!
//! All integers are hashed using their Big-Endian representation.  E.g., u64
//! occupies 8 bytes and 1 is encoded as 0000_0000_0000_0001, and u32 occupies 4
//! bytes and 1 is encoded as 0000_0001.
//!
//! All the variable-size values (paths, arrays) are prefixed with their length
//! to get a unique encoding. If it wasn't the case, it would be possible to
//! interpret 2 file table entries
//!
//! ```text
//! [ "path1" size1 hash1 ]
//! [ "path2" size2 hash2 ]
//! ```
//!
//! as a single file table entry:
//!
//! ```text
//! [ "path1 size1 hash1 path2" size2 hash2 ]
//! ```
//!
//! and get the same manifest hash.
//!
//! Below are the rules for computing hashes of table entries and the root hash
//! of a manifest:
//!
//! * The hash in the chunk table is simply the hash of the raw chunk content.
//! ```text
//!   chunk_hash := hash(dsep("ic-state-chunk") · file[offset:offset + size_bytes])
//! ```
//! where
//! ```text
//! dsep(seq) = byte(len(seq)) · seq
//! ```
//!
//! * The hash in the file table is the hash of the slice of the chunk table
//!   corresponding to this file:
//! ```text
//!   file_hash   := hash(dsep("ic-state-file") · len(slice) as u32 · chunk_entry*)
//!   chunk_entry := size_bytes as u32
//!                  · offset as u64
//!                  · chunk_hash
//! ```
//!
//! * The manifest hash is the hash of the protobuf-encoded meta manifest.
//!
//! * Before `StateSyncVersion::V3` the file hash additionally includes the file
//!   index within every chunk entry:
//! ```text
//!   file_hash   := hash(dsep("ic-state-file") · len(slice) as u32 · chunk_entry*)
//!   chunk_entry := file_index as u32
//!                  · size_bytes as u32
//!                  · offset as u64
//!                  · chunk_hash
//! ```
//!
//! * The `StateSyncVersion::V1` manifest hash is computed by hashing the file
//!   and chunk tables:
//! ```text
//!   manifest_hash := hash(dsep("ic-state-manifest")
//!                    · version as u32
//!                    · len(file_table) as u32
//!                    · file_entry*
//!                    · len(chunk_table) as u32
//!                    · chunk_entry*
//!                    )
//!   file_entry    := len(relative_path) as u32
//!                    · relative_path
//!                    · size_bytes as u64
//!                    · file_hash
//! ```
//!
//! * The `StateSyncVersion::V0` manifest hash is computed by hashing the file
//!   table only and does not include a version number.
pub mod proto;

use crate::chunkable::ChunkId;
use ic_protobuf::{proxy::ProtoProxy, state::sync::v1 as pb};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt::{self, Display},
    ops::{Deref, Range},
    sync::Arc,
};
use strum_macros::EnumIter;

/// The default chunk size used in manifest computation and state sync.
pub const DEFAULT_CHUNK_SIZE: u32 = 1 << 20; // 1 MiB.

/// ID of the meta-manifest chunk in StateSync artifact.
pub const META_MANIFEST_CHUNK: ChunkId = ChunkId::new(0);

/// The IDs of file chunks in state sync start from 1 as chunk id 0 is for the meta-manifest.
/// The chunk id of a file chunk is equal to its index in the chunk table plus 1.
pub const FILE_CHUNK_ID_OFFSET: usize = 1;

/// Some small files are grouped into chunks during state sync and
/// they need to use a separate range of chunk id to avoid conflicts with normal chunks.
//
// The value of `FILE_GROUP_CHUNK_ID_OFFSET` is set as 1 << 30 (1_073_741_824).
// It is within the whole chunk id range and also large enough to avoid conflicts as shown in the calculations below.
// Every subnet starts off with an allocation of 1 << 20 canister IDs. Suppose a subnet ends up with 10 * 1 << 20 canisters and 100 TiB state.
// Given that each canister has fewer than 10 files in a checkpoint and 1 TiB of state has approximately 1 << 20 chunks,
// the length of chunk table will be smaller than 10 * 10 * 1 << 20 + 100 * 1 << 20 = 209_715_200.
// The real number of canisters and size of state are not even close to the assumption so the value of `FILE_GROUP_CHUNK_ID_OFFSET` is chosen safely.
pub const FILE_GROUP_CHUNK_ID_OFFSET: u32 = 1 << 30;

/// The IDs of chunks for fetching the manifest in state sync start from this offset.
//
// The value of `MANIFEST_CHUNK_ID_OFFSET` is set as 1 << 31 (2_147_483_648).
// It is within the whole chunk id range (1 << 32) and can avoid collision with normal file chunks and file group chunks.
// First, the length of the chunk table is smaller than 1_073_741_824 from the analysis for `FILE_GROUP_CHUNK_ID_OFFSET`. Second, each file group chunk contains multiple files.
// Therefore the number of file groups is smaller than the length of chunk table, and thus much smaller than 1_073_741_824.
// From another perspective, the number of file group chunks is smaller than 1/128 of the number of canisters because currently it only includes `canister.pbuf` files smaller than 8 KiB.
// Therefore, the space between `FILE_GROUP_CHUNK_ID_OFFSET` and `MANIFEST_CHUNK_ID_OFFSET` is adequate for file group chunks.
pub const MANIFEST_CHUNK_ID_OFFSET: u32 = 1 << 31;

/// `MANIFEST_CHUNK_ID_OFFSET` should be greater than `FILE_GROUP_CHUNK_ID_OFFSET` to have valid ID range assignment.
#[allow(clippy::assertions_on_constants)]
const _: () = assert!(MANIFEST_CHUNK_ID_OFFSET > FILE_GROUP_CHUNK_ID_OFFSET);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, EnumIter, Serialize, Deserialize,
)]
pub enum StateSyncVersion {
    /// Initial version.
    V0 = 0,

    /// Also include version and chunk hashes into manifest hash.
    V1 = 1,

    /// Compute the manifest hash based on the encoded manifest.
    V2 = 2,

    /// File index-independent manifest hash: file index no longer included in file
    /// hash.
    V3 = 3,
}

impl std::convert::TryFrom<u32> for StateSyncVersion {
    type Error = u32;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        use strum::IntoEnumIterator;
        for version in StateSyncVersion::iter() {
            if version as u32 == n {
                return Ok(version);
            }
        }
        Err(n)
    }
}

impl Display for StateSyncVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// The version of StateSync protocol that should be used for all newly created manifests.
pub const CURRENT_STATE_SYNC_VERSION: StateSyncVersion = StateSyncVersion::V2;

/// Maximum supported StateSync version.
///
/// The replica will panic if trying to deal with a manifest with a version higher than this.
pub const MAX_SUPPORTED_STATE_SYNC_VERSION: StateSyncVersion = StateSyncVersion::V3;

/// The type and associated index (if applicable) of a chunk in state sync.
#[derive(Debug, PartialEq, Eq)]
pub enum StateSyncChunk {
    /// The chunk representing the meta-manifest.
    MetaManifestChunk,
    /// Nth file chunk (0-based).
    FileChunk(u32),
    /// Chunk grouping multiple small files (index starting from `FILE_GROUP_CHUNK_ID_OFFSET`).
    FileGroupChunk(u32),
    /// Nth encoded manifest chunk (0-based).
    ManifestChunk(u32),
}

/// Convert a chunk ID to its chunk type and associated index based on chunk ID range assignment.
/// Note that the conversion only works when `MANIFEST_CHUNK_ID_OFFSET` is greater than `FILE_GROUP_CHUNK_ID_OFFSET`.
pub fn state_sync_chunk_type(chunk_id: u32) -> StateSyncChunk {
    const FILE_CHUNK_END_INCLUSIVE: u32 = FILE_GROUP_CHUNK_ID_OFFSET - 1;
    const FILE_GROUP_CHUNK_END_INCLUSIVE: u32 = MANIFEST_CHUNK_ID_OFFSET - 1;
    match chunk_id {
        0 => StateSyncChunk::MetaManifestChunk,
        1..=FILE_CHUNK_END_INCLUSIVE => {
            StateSyncChunk::FileChunk(chunk_id - FILE_CHUNK_ID_OFFSET as u32)
        }
        FILE_GROUP_CHUNK_ID_OFFSET..=FILE_GROUP_CHUNK_END_INCLUSIVE => {
            // Note that key of file group chunks mapping is the exact chunk id so it does not need to be offset.
            StateSyncChunk::FileGroupChunk(chunk_id)
        }
        MANIFEST_CHUNK_ID_OFFSET.. => {
            StateSyncChunk::ManifestChunk(chunk_id - MANIFEST_CHUNK_ID_OFFSET)
        }
    }
}

/// An entry of the file table.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileInfo {
    /// Path relative to the checkpoint root.
    pub relative_path: std::path::PathBuf,
    /// Total size of the file in bytes.
    pub size_bytes: u64,
    /// SHA-256 hash of the file metadata and all entries from the chunk table.
    /// See note [Manifest Hash].
    pub hash: [u8; 32],
}

/// An entry of the chunk table.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ChunkInfo {
    /// Index of the file in the file table.
    pub file_index: u32,
    /// Total size of this chunk in bytes.
    pub size_bytes: u32,
    /// Offset of the chunk within the file.
    pub offset: u64,
    /// SHA-256 hash of the chunk content.
    /// See note [Manifest Hash].
    pub hash: [u8; 32],
}

impl ChunkInfo {
    /// Returns the range of bytes belonging to this chunk.
    pub fn byte_range(&self) -> Range<usize> {
        self.offset as usize..(self.offset as usize + self.size_bytes as usize)
    }
}

/// We wrap the actual Manifest (ManifestData) in an Arc, in order to
/// make Manifest both immutable and cheap to copy
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Manifest(
    #[serde(serialize_with = "ic_utils::serde_arc::serialize_arc")]
    #[serde(deserialize_with = "ic_utils::serde_arc::deserialize_arc")]
    Arc<ManifestData>,
);

impl Manifest {
    pub fn new(
        version: StateSyncVersion,
        file_table: Vec<FileInfo>,
        chunk_table: Vec<ChunkInfo>,
    ) -> Self {
        Self(Arc::new(ManifestData {
            version,
            file_table,
            chunk_table,
        }))
    }
}

impl Deref for Manifest {
    type Target = ManifestData;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Manifest is a short description of the checkpoint contents.
///
/// The manifest is structured as 2 tables: a file table and a chunk table,
/// where chunks point to the files they come from.  Such a structure allows us
/// to explicitly enumerate all the chunks in a space-efficient manner.
///
/// The logical structure of the manifest is the following:
/// ```text
/// -- FILES
/// [0]: ("system_metadata.cbor", 1500, <hash>)
/// ...
/// [8]: ("canister_states/00..11/software.wasm", 93000, <hash>)
/// -- CHUNKS
/// -- chunk indices start from 1 because 0 is the ID of the ,meta-manifest chunk.
/// [ 1]: (0, 1500, 0, <hash>)
/// ...
/// [45]: (8, 93000, 0, <hash>)
/// ```
#[derive(Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ManifestData {
    /// Which version of the hashing procedure should be used.
    pub version: StateSyncVersion,
    pub file_table: Vec<FileInfo>,
    pub chunk_table: Vec<ChunkInfo>,
}

/// MetaManifest describes how the manifest is encoded, split and hashed.
///
/// The meta-manifest is built in the following way:
///     1. Use protobuf to encode the manifest into raw bytes
///     2. Split the encoded manifest into chunks of `DEFAULT_CHUNK_SIZE` bytes, called sub-manifests.
///     3. Hash each sub-manifest and collect their hashes
///
/// The hash of meta-manifest is computed by hashing the version, the length and `sub_manifest_hashes`:
/// ```text
///   meta_manifest_hash := hash(dsep("ic-state-meta-manifest")
///                         · version as u32
///                         · len(sub_manifest_hashes) as u32
///                         · sub_manifest_hashes
///                         )
///   sub_manifest_hash  := hash(dsep("ic-state-sub-manifest") · encoded_manifest[offset:offset + size_bytes])
/// ```
///
/// The `meta_manifest_hash` is used as the manifest hash when the manifest version is greater than or equal to `StateSyncVersion::V1`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct MetaManifest {
    pub version: StateSyncVersion,
    pub sub_manifest_hashes: Vec<[u8; 32]>,
}

impl fmt::Display for Manifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn write_header(f: &mut fmt::Formatter<'_>, spec: &[(&'static str, usize)]) -> fmt::Result {
            for (idx, (field, width)) in spec.iter().enumerate() {
                write!(
                    f,
                    "{}{:^width$}",
                    if idx > 0 { "|" } else { "" },
                    field,
                    width = width
                )?;
            }
            writeln!(f)?;
            for (idx, (_, width)) in spec.iter().enumerate() {
                write!(
                    f,
                    "{}{:-^width$}",
                    if idx > 0 { "+" } else { "" },
                    "",
                    width = width
                )?;
            }
            writeln!(f)?;
            Ok(())
        }

        let max_path_len = self
            .file_table
            .iter()
            .map(|f| f.relative_path.as_os_str().len())
            .max()
            .unwrap_or(6);

        writeln!(f, "MANIFEST VERSION: {}", self.version)?;
        writeln!(f, "FILE TABLE")?;
        write_header(
            f,
            &[
                ("idx", 12),
                ("size", 12),
                ("hash", 66),
                ("path", max_path_len),
            ],
        )?;
        for (idx, file_info) in self.file_table.iter().enumerate() {
            writeln!(
                f,
                " {:>10} | {:>10} | {:64} | {}",
                idx,
                file_info.size_bytes,
                hex::encode(file_info.hash),
                file_info.relative_path.display()
            )?;
        }
        writeln!(f, "CHUNK TABLE")?;
        write_header(
            f,
            &[
                ("idx", 12),
                ("file_idx", 12),
                ("offset", 12),
                ("size", 12),
                ("hash", 66),
            ],
        )?;
        for (idx, chunk_info) in self.chunk_table.iter().enumerate() {
            writeln!(
                f,
                " {:>10} | {:>10} | {:>10} | {:>10} | {}",
                idx,
                chunk_info.file_index,
                chunk_info.offset,
                chunk_info.size_bytes,
                hex::encode(chunk_info.hash),
            )?;
        }
        Ok(())
    }
}

/// Serializes the manifest into a byte array.
pub fn encode_manifest(manifest: &Manifest) -> Vec<u8> {
    pb::Manifest::proxy_encode(manifest.clone()).expect("Failed to serialize manifest.")
}

/// Deserializes the manifest from a byte array.
pub fn decode_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    pb::Manifest::proxy_decode(bytes)
        .map_err(|err| format!("failed to convert Manifest proto into an object: {}", err))
}

pub fn encode_meta_manifest(meta_manifest: &MetaManifest) -> Vec<u8> {
    pb::MetaManifest::proxy_encode(meta_manifest.clone())
        .expect("Failed to serialize meta-manifest.")
}

pub fn decode_meta_manifest(bytes: &[u8]) -> Result<MetaManifest, String> {
    pb::MetaManifest::proxy_decode(bytes).map_err(|err| {
        format!(
            "failed to convert MetaManifest proto into an object: {}",
            err
        )
    })
}

type P2PChunkId = u32;
type ManifestChunkTableIndex = u32;

/// A chunk id from the P2P level is mapped to a group of indices from the manifest chunk table.
/// `FileGroupChunks` stores the mapping and can be used to assemble or split the file group chunk.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileGroupChunks(BTreeMap<P2PChunkId, Vec<ManifestChunkTableIndex>>);

impl FileGroupChunks {
    pub fn new(value: BTreeMap<P2PChunkId, Vec<ManifestChunkTableIndex>>) -> Self {
        FileGroupChunks(value)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn keys(&self) -> impl Iterator<Item = &ManifestChunkTableIndex> {
        self.0.keys()
    }

    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (&ManifestChunkTableIndex, &Vec<ManifestChunkTableIndex>)> {
        self.0.iter()
    }

    pub fn get(&self, chunk_id: &P2PChunkId) -> Option<&Vec<ManifestChunkTableIndex>> {
        self.0.get(chunk_id)
    }

    pub fn last_chunk_id(&self) -> Option<P2PChunkId> {
        self.0.last_key_value().map(|(chunk_id, _)| *chunk_id)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_sync_chunk_type() {
        assert_eq!(state_sync_chunk_type(0), StateSyncChunk::MetaManifestChunk);

        (1..FILE_GROUP_CHUNK_ID_OFFSET)
            .step_by(100)
            .chain(std::iter::once(FILE_GROUP_CHUNK_ID_OFFSET - 1))
            .for_each(|i| assert_eq!(state_sync_chunk_type(i), StateSyncChunk::FileChunk(i - 1)));

        (FILE_GROUP_CHUNK_ID_OFFSET..MANIFEST_CHUNK_ID_OFFSET)
            .step_by(100)
            .chain(std::iter::once(MANIFEST_CHUNK_ID_OFFSET - 1))
            .for_each(|i| assert_eq!(state_sync_chunk_type(i), StateSyncChunk::FileGroupChunk(i)));

        (MANIFEST_CHUNK_ID_OFFSET..=u32::MAX)
            .step_by(100)
            .chain(std::iter::once(u32::MAX))
            .for_each(|i| {
                assert_eq!(
                    state_sync_chunk_type(i),
                    StateSyncChunk::ManifestChunk(i - MANIFEST_CHUNK_ID_OFFSET)
                )
            });
    }
}
