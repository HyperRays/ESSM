#![doc = include_str!("../README.md")]

mod client;
mod error;
mod types;
mod wire;

pub use client::{Client, PROTOCOL_VERSION, development_command};
pub use error::Error;
pub use types::{
    BackendError, CompletionPage, DirectoryFailure, DirectoryPage, DirectoryState, DirectoryStats,
    EncodedBinary, EntryRow, IndexHandle, IndexId, IndexState, IndexStatus, MountPolicy, Ranking,
    ScanCounters, ScanFailure, ScanOptions, ScanOutcome, ScanReport, ScanResult, SizedName,
    StoreStats,
};
