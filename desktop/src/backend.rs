//! Desktop adapter for the synchronous `findex-client`.
//!
//! The worker owns the client and performs the only desktop-specific work:
//! draining completed directory IDs and asking the bridge for compact size
//! summaries. Traversal policy stays in Findex. A small bounded channel is the
//! complete boundary between that worker and iced, so a busy renderer cannot
//! grow an unbounded event backlog.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use findex_client::{
    Client, DirectoryState, DirectoryStats, EncodedBinary, IndexId, IndexState, IndexStatus,
    MountPolicy, Ranking, ScanOptions, ScanResult, development_command,
};
use iced::futures::SinkExt;
use iced::futures::channel::mpsc::{Receiver, Sender, channel};
use serde_json::Value;

pub const SIZE_FIELD: &str = "allocated_size";
/// Desktop scans optimize for finding macOS space consumers early. This is a
/// desktop product default; the reusable client keeps Findex's native order.
pub const DEFAULT_RANKING: Ranking = Ranking::Macos;

/// The bridge accepts at most 4,096 IDs in one summary request. Keeping the
/// journal and summary pages the same size removes the many tiny IPC round
/// trips that used to make the desktop trail a completed scan.
pub const COMPLETION_BATCH_SIZE: u32 = 4_096;
const EVENT_CHANNEL_CAPACITY: usize = 4;
const LARGEST_PER_DIRECTORY: u32 = 8;

#[derive(Clone, Debug)]
pub struct BackendOptions {
    pub project_root: PathBuf,
    pub root: PathBuf,
    pub fields: Vec<String>,
    pub concurrency: Option<u32>,
    pub poll_interval: Duration,
    pub progress_interval: Duration,
    pub mount_policy: MountPolicy,
}

#[derive(Clone, Debug)]
pub struct DirectorySummary {
    pub id: u32,
    pub parent_id: Option<u32>,
    pub name: String,
    pub state: DirectoryState,
    pub entries: u64,
    pub children: u64,
    pub error: String,
    /// Sum of `allocated_size` over this directory's immediate regular files.
    pub allocated_bytes: u64,
    /// This directory's largest immediate regular files, size-descending.
    pub largest_files: Vec<LargestFile>,
}

#[derive(Clone, Debug)]
pub struct LargestFile {
    pub name: String,
    pub size: u64,
}

/// Log2 buckets of regular-file sizes; bucket `b > 0` covers
/// `[2^(b-1), 2^b)` bytes and bucket `0` counts empty files.
pub const HISTOGRAM_BUCKETS: usize = 44;

#[derive(Clone, Debug)]
pub struct SizeHistogram {
    pub counts: [u64; HISTOGRAM_BUCKETS],
    pub bytes: [u64; HISTOGRAM_BUCKETS],
}

impl Default for SizeHistogram {
    fn default() -> Self {
        Self {
            counts: [0; HISTOGRAM_BUCKETS],
            bytes: [0; HISTOGRAM_BUCKETS],
        }
    }
}

impl SizeHistogram {
    fn merge(&mut self, contributions: &[[u64; 3]]) {
        for &[bucket, count, bytes] in contributions {
            let bucket = (bucket as usize).min(HISTOGRAM_BUCKETS - 1);
            self.counts[bucket] = self.counts[bucket].saturating_add(count);
            self.bytes[bucket] = self.bytes[bucket].saturating_add(bytes);
        }
    }

    pub fn bucket_floor(bucket: usize) -> u64 {
        if bucket == 0 { 0 } else { 1 << (bucket - 1) }
    }
}

#[derive(Clone, Debug)]
pub enum BackendEvent {
    Started {
        root: String,
        concurrency: Option<u32>,
        mount_policy: MountPolicy,
    },
    Update {
        status: IndexStatus,
        elapsed_ms: f64,
        allocated_bytes: u64,
        directories: Vec<DirectorySummary>,
        histogram: Box<SizeHistogram>,
    },
    Finished {
        result: ScanResult,
        allocated_bytes: u64,
    },
    Error(String),
}

pub struct BackendHandle {
    pub events: Receiver<BackendEvent>,
    pub worker: JoinHandle<Result<(), String>>,
}

impl BackendHandle {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn join(self) -> Result<(), String> {
        self.worker
            .join()
            .map_err(|_| "backend worker panicked".to_owned())?
    }
}

pub fn spawn(options: BackendOptions) -> BackendHandle {
    let (mut event_sender, events) = channel(EVENT_CHANNEL_CAPACITY);
    let worker = thread::spawn(move || {
        let result = run(options, &mut event_sender);
        if let Err(error) = &result {
            let _ = send_event(&mut event_sender, BackendEvent::Error(error.clone()));
        }
        result
    });

    BackendHandle { events, worker }
}

fn packaged_backend_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let release = executable
        .parent()?
        .join("../Resources/backend/bin/backend");
    release.is_file().then_some(release)
}

pub fn packaged_backend_available() -> bool {
    packaged_backend_path().is_some()
}

/// A packaged app carries an OTP release. Development builds use the shared
/// workspace containing sibling `findex`, `rust_client`, and `desktop` trees.
fn backend_command(project_root: &Path) -> Command {
    if let Some(release) = packaged_backend_path() {
        let mut command = Command::new(release);
        command.arg("eval").arg("FindexRust.Bridge.run()");
        return command;
    }
    development_command(project_root)
}

fn run(options: BackendOptions, events: &mut Sender<BackendEvent>) -> Result<(), String> {
    let mut client = Client::spawn(backend_command(&options.project_root))
        .map_err(|error| format!("could not start Findex backend: {error}"))?;

    let scan_options = scan_options(&options);

    let index = match client.start_scan(&options.root, &scan_options) {
        Ok(index) => index,
        Err(error) => {
            let _ = client.shutdown();
            return Err(format!("could not start traversal: {error}"));
        }
    };

    let session_result = (|| {
        send_event(
            events,
            BackendEvent::Started {
                root: index.root.clone(),
                concurrency: options.concurrency,
                mount_policy: options.mount_policy,
            },
        )?;
        drive_index(&mut client, index.index_id, &options, events)
    })();

    let release_result = client
        .release_index(index.index_id)
        .map_err(|error| format!("could not release index: {error}"));
    let shutdown_result = client
        .shutdown()
        .map_err(|error| format!("could not stop Findex backend: {error}"));

    session_result?;
    release_result?;
    shutdown_result
}

fn scan_options(options: &BackendOptions) -> ScanOptions {
    ScanOptions {
        fields: options.fields.clone(),
        concurrency: options.concurrency,
        ranking: DEFAULT_RANKING,
        mount_policy: options.mount_policy,
        failure_sample_limit: Some(20),
        ..ScanOptions::default()
    }
}

fn drive_index(
    client: &mut Client,
    index_id: IndexId,
    options: &BackendOptions,
    events: &mut Sender<BackendEvent>,
) -> Result<(), String> {
    let started_at = Instant::now();
    let mut next_progress_at = started_at;
    let mut cursor = 0_u64;
    let mut buffered_summaries = Vec::with_capacity(COMPLETION_BATCH_SIZE as usize);
    let mut allocated_bytes = 0_u64;
    let mut histogram = SizeHistogram::default();

    loop {
        let completion = client
            .completed_directories(index_id, cursor, COMPLETION_BATCH_SIZE)
            .map_err(|error| format!("could not read completion journal: {error}"))?;
        cursor = completion.cursor;

        if !completion.directory_ids.is_empty() {
            let stats = client
                .summarize_directories(
                    index_id,
                    &completion.directory_ids,
                    SIZE_FIELD,
                    LARGEST_PER_DIRECTORY,
                )
                .map_err(|error| format!("could not summarize directories: {error}"))?;
            for stat in stats {
                allocated_bytes = allocated_bytes.saturating_add(stat.size_bytes);
                histogram.merge(&stat.histogram);
                buffered_summaries.push(summary_from_stats(stat));
            }
        }

        let now = Instant::now();
        let should_publish =
            now >= next_progress_at || buffered_summaries.len() >= COMPLETION_BATCH_SIZE as usize;
        if should_publish {
            let status = client
                .index_status(index_id)
                .map_err(|error| format!("could not read index status: {error}"))?;
            let journal_drained =
                cursor >= status.store.completion_count && status.state == IndexState::Finished;
            send_event(
                events,
                BackendEvent::Update {
                    status,
                    elapsed_ms: started_at.elapsed().as_secs_f64() * 1_000.0,
                    allocated_bytes,
                    directories: std::mem::take(&mut buffered_summaries),
                    histogram: Box::new(histogram.clone()),
                },
            )?;
            next_progress_at = now + options.progress_interval;

            if journal_drained {
                let result = client
                    .await_scan(index_id)
                    .map_err(|error| format!("could not read terminal report: {error}"))?;
                send_event(
                    events,
                    BackendEvent::Finished {
                        result,
                        allocated_bytes,
                    },
                )?;
                return Ok(());
            }
        }

        if completion.directory_ids.is_empty() {
            thread::sleep(options.poll_interval);
        }
    }
}

fn summary_from_stats(stats: DirectoryStats) -> DirectorySummary {
    let mut largest_files: Vec<LargestFile> = stats
        .largest
        .iter()
        .map(|file| LargestFile {
            name: binary_text(&file.name),
            size: file.size,
        })
        .collect();
    largest_files.sort_by_key(|file| std::cmp::Reverse(file.size));

    DirectorySummary {
        id: stats.directory_id,
        parent_id: stats.parent_id,
        name: binary_text(&stats.name),
        state: stats.state,
        entries: stats.entry_count,
        children: stats.child_count,
        error: value_text(stats.error.as_ref()),
        allocated_bytes: stats.size_bytes,
        largest_files,
    }
}

fn binary_text(value: &EncodedBinary) -> String {
    match value {
        EncodedBinary::Utf8(value) => value.clone(),
        EncodedBinary::Base64 { base64 } => format!("<bytes:{base64}>"),
    }
}

fn value_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
    }
}

fn send_event(events: &mut Sender<BackendEvent>, event: BackendEvent) -> Result<(), String> {
    iced::futures::executor::block_on(events.send(event))
        .map_err(|_| "UI event receiver is closed".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ScanModel;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn histogram_merges_sparse_bridge_contributions() {
        let mut histogram = SizeHistogram::default();
        histogram.merge(&[[0, 1, 0], [11, 2, 3_000]]);
        histogram.merge(&[[11, 1, 1_500], [999, 4, 8]]);

        assert_eq!(histogram.counts[0], 1);
        assert_eq!(histogram.counts[11], 3);
        assert_eq!(histogram.bytes[11], 4_500);
        assert_eq!(histogram.counts[HISTOGRAM_BUCKETS - 1], 4);
        assert_eq!(SizeHistogram::bucket_floor(0), 0);
        assert_eq!(SizeHistogram::bucket_floor(11), 1_024);
    }

    #[test]
    fn desktop_scan_options_use_macos_ranking_by_default() {
        let options = BackendOptions {
            project_root: PathBuf::from("project"),
            root: PathBuf::from("/"),
            fields: vec!["type".to_owned(), SIZE_FIELD.to_owned()],
            concurrency: Some(8),
            poll_interval: Duration::from_millis(1),
            progress_interval: Duration::from_millis(100),
            mount_policy: MountPolicy::StayOnFilesystem,
        };

        let scan_options = scan_options(&options);

        assert_eq!(scan_options.ranking, Ranking::Macos);
        assert_eq!(scan_options.fields, options.fields);
        assert_eq!(scan_options.concurrency, options.concurrency);
        assert_eq!(scan_options.mount_policy, options.mount_policy);
    }

    /// Manual end-to-end consumer benchmark:
    /// `FINDEX_BENCH_ROOT=/some/tree cargo test --release drain_throughput -- --ignored --nocapture`.
    #[test]
    #[ignore = "manual benchmark; requires FINDEX_BENCH_ROOT"]
    fn drain_throughput_benchmark() {
        let Some(root) = std::env::var_os("FINDEX_BENCH_ROOT") else {
            eprintln!("set FINDEX_BENCH_ROOT to run this benchmark");
            return;
        };
        let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("desktop must be inside the shared workspace")
            .to_path_buf();
        let mut backend = spawn(BackendOptions {
            project_root,
            root: PathBuf::from(root),
            fields: vec!["type".to_owned(), SIZE_FIELD.to_owned()],
            concurrency: None,
            poll_interval: Duration::from_millis(1),
            progress_interval: Duration::from_millis(100),
            mount_policy: MountPolicy::StayOnFilesystem,
        });
        let mut app = ScanModel::new(256);
        let started = Instant::now();
        let mut updates = 0_u64;
        let deadline = Instant::now() + Duration::from_secs(600);
        while Instant::now() < deadline {
            match backend.events.try_recv() {
                Ok(event @ BackendEvent::Finished { .. }) => {
                    let (indexer_seconds, entries) = match &event {
                        BackendEvent::Finished { result, .. } => {
                            (result.report.elapsed_ms / 1_000.0, result.report.entries)
                        }
                        _ => unreachable!(),
                    };
                    app.apply_event(event);
                    let pipeline_seconds = started.elapsed().as_secs_f64();
                    eprintln!(
                        "entries {entries} - indexer {indexer_seconds:.2}s ({:.0}/s) - \
                         desktop model {pipeline_seconds:.2}s ({:.0}/s) - lag x{:.1} - \
                         {updates} update events",
                        entries as f64 / indexer_seconds.max(0.001),
                        entries as f64 / pipeline_seconds.max(0.001),
                        pipeline_seconds / indexer_seconds.max(0.001),
                    );
                    backend.join().expect("join benchmark backend");
                    return;
                }
                Ok(event @ BackendEvent::Update { .. }) => {
                    updates += 1;
                    app.apply_event(event);
                }
                Ok(event) => app.apply_event(event),
                Err(error) if error.is_closed() => panic!("backend events closed early"),
                Err(_empty) => thread::sleep(Duration::from_millis(2)),
            }
        }
        panic!("benchmark did not finish within the deadline");
    }

    #[test]
    fn desktop_backend_streams_batched_model_events() {
        let tree = TestTree::new();
        let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("desktop must be inside the shared workspace")
            .to_path_buf();
        let mut backend = spawn(BackendOptions {
            project_root,
            root: tree.0.clone(),
            fields: vec!["type".to_owned(), SIZE_FIELD.to_owned()],
            concurrency: Some(2),
            poll_interval: Duration::from_millis(1),
            progress_interval: Duration::from_millis(5),
            mount_policy: MountPolicy::Cross,
        });

        let mut app = ScanModel::new(16);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !app.finished {
            match backend.events.try_recv() {
                Ok(event) => app.apply_event(event),
                Err(error) if error.is_closed() => panic!("backend events closed early"),
                Err(_empty) => thread::sleep(Duration::from_millis(5)),
            }
        }
        assert!(app.finished);
        assert_eq!(app.counters.entries, 4);
        assert_eq!(app.size_tree.children(0).len(), 2);
        backend.join().expect("join desktop backend");
    }

    struct TestTree(PathBuf);

    impl TestTree {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "findex-desktop-test-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(root.join("ordinary")).expect("create ordinary directory");
            fs::create_dir_all(root.join("target")).expect("create target directory");
            fs::write(root.join("ordinary/file.txt"), b"one").expect("create ordinary file");
            fs::write(root.join("target/generated.bin"), b"two").expect("create generated file");
            Self(root)
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
