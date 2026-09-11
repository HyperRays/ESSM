//! Public scan options, handles, pages, and reports.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MountPolicy {
    #[default]
    StayOnFilesystem,
    Cross,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ranking {
    #[default]
    Default,
    NameBiased,
    Macos,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanOptions {
    /// Findex metadata fields. Recursive traversal requires `type`.
    pub fields: Vec<String>,
    /// Defaults to twice the BEAM dirty-I/O scheduler count.
    pub concurrency: Option<u32>,
    /// Bytes requested from `getattrlistbulk` per batch.
    pub buffer_size: Option<u32>,
    /// Named policy evaluated directly by Findex's Elixir scheduler.
    pub ranking: Ranking,
    pub mount_policy: MountPolicy,
    /// Number of directory failures retained in the Elixir report.
    pub failure_sample_limit: Option<u32>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            fields: vec!["type".to_owned()],
            concurrency: None,
            buffer_size: None,
            ranking: Ranking::Default,
            mount_policy: MountPolicy::StayOnFilesystem,
            failure_sample_limit: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ScanOutcome {
    Ok,
    Fatal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct IndexId(pub u64);

#[derive(Clone, Debug, Deserialize)]
pub struct IndexHandle {
    pub index_id: IndexId,
    pub root: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum EncodedBinary {
    Utf8(String),
    Base64 { base64: String },
}

impl EncodedBinary {
    pub fn as_utf8(&self) -> Option<&str> {
        match self {
            Self::Utf8(value) => Some(value),
            Self::Base64 { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScanResult {
    pub outcome: ScanOutcome,
    pub report: ScanReport,
    pub failure: Option<ScanFailure>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScanFailure {
    pub kind: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScanReport {
    pub root: String,
    pub complete: bool,
    pub elapsed_ms: f64,
    pub entries: u64,
    pub directories: u64,
    pub regular_files: u64,
    pub symlinks: u64,
    pub other: u64,
    pub metadata_errors: u64,
    pub metadata_error_counts: BTreeMap<String, u64>,
    pub directory_failure_counts: BTreeMap<String, u64>,
    pub directory_failure_reasons: BTreeMap<String, u64>,
    pub directory_failure_samples: Vec<DirectoryFailure>,
    pub skipped_mounts: u64,
    pub store: StoreStats,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DirectoryFailure {
    pub id: u64,
    pub path: EncodedBinary,
    pub phase: String,
    pub reason: String,
    pub category: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    Running,
    Finished,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IndexStatus {
    pub index_id: IndexId,
    pub root: String,
    pub state: IndexState,
    pub ranking: Ranking,
    pub pending: u64,
    pub in_flight: u64,
    pub counters: ScanCounters,
    pub store: StoreStats,
    pub outcome: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScanCounters {
    pub entries: u64,
    pub directories: u64,
    pub regular_files: u64,
    pub symlinks: u64,
    pub other: u64,
    pub metadata_errors: u64,
    pub metadata_error_counts: BTreeMap<String, u64>,
    pub directory_failure_counts: BTreeMap<String, u64>,
    pub directory_failure_reasons: BTreeMap<String, u64>,
    pub skipped_mounts: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CompletionPage {
    pub index_id: IndexId,
    pub from_cursor: u64,
    pub cursor: u64,
    pub directory_ids: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryState {
    Pending,
    Published,
    Failed,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DirectoryPage {
    pub index_id: IndexId,
    pub directory_id: u32,
    pub state: DirectoryState,
    pub parent_id: Option<u32>,
    pub name: EncodedBinary,
    pub error: Option<Value>,
    pub entry_count: u64,
    pub child_count: u64,
    pub offset: u64,
    pub next_offset: u64,
    pub done: bool,
    pub entries: Vec<EntryRow>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EntryRow {
    pub row: u64,
    pub child_directory_id: Option<u32>,
    pub values: BTreeMap<String, Value>,
}

/// Aggregate of one completed directory from `summarize_directories`.
#[derive(Clone, Debug, Deserialize)]
pub struct DirectoryStats {
    pub directory_id: u32,
    pub state: DirectoryState,
    pub parent_id: Option<u32>,
    pub name: EncodedBinary,
    #[serde(default)]
    pub error: Option<Value>,
    pub entry_count: u64,
    pub child_count: u64,
    /// Sum of the requested size field over immediate regular files.
    pub size_bytes: u64,
    /// The directory's largest immediate regular files, size-descending.
    pub largest: Vec<SizedName>,
    /// Sparse log₂ histogram contributions: `[bucket, count, bytes]`.
    pub histogram: Vec<[u64; 3]>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SizedName {
    pub name: EncodedBinary,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StoreStats {
    pub directory_count: u64,
    pub published_directory_count: u64,
    pub failed_directory_count: u64,
    pub pending_directory_count: u64,
    pub completion_count: u64,
    pub entry_count: u64,
    pub block_bytes: u64,
    pub payload_bytes: u64,
    pub directory_table_bytes: u64,
    pub completion_journal_bytes: u64,
    pub root_name_bytes: u64,
    pub native_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct BackendError {
    pub code: String,
    pub message: String,
}
