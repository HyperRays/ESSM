//! Synchronous client for a long-lived Findex BEAM sidecar.
//!
//! The caller owns the child process. Requests, responses, and pushed events
//! use a private framed binary pipe; no listening socket or Erlang distribution
//! is exposed.

mod wire;

use std::collections::{BTreeMap, VecDeque};
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 6;

/// Produces a command for a development workspace containing sibling
/// `findex` and `rust_client` directories.
///
/// Run `mix compile` in `rust_client/backend` before spawning this command.
/// Packaged applications should instead pass a command for their bundled OTP
/// release to [`Client::spawn`].
pub fn development_command(workspace_root: impl AsRef<Path>) -> Command {
    let backend_root = workspace_root.as_ref().join("rust_client/backend");
    let mut command = Command::new("elixir");
    command
        .arg("-pa")
        .arg("_build/dev/lib/findex/ebin")
        .arg("-pa")
        .arg("_build/dev/lib/findex_rust_backend/ebin")
        .arg("backend.exs")
        .current_dir(backend_root);
    command
}

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

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Serialization(serde_json::Error),
    Protocol(String),
    Backend(BackendError),
    BackendExited(Option<i32>),
    BackendFailed(ExitStatus),
    NonUtf8Path(PathBuf),
    RequestIdExhausted,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "backend I/O failed: {error}"),
            Self::Serialization(error) => write!(formatter, "invalid bridge value: {error}"),
            Self::Protocol(message) => write!(formatter, "bridge protocol error: {message}"),
            Self::Backend(error) => {
                write!(
                    formatter,
                    "backend rejected request ({}): {}",
                    error.code, error.message
                )
            }
            Self::BackendExited(code) => write!(formatter, "backend exited unexpectedly: {code:?}"),
            Self::BackendFailed(status) => write!(formatter, "backend exited with {status}"),
            Self::NonUtf8Path(path) => {
                write!(formatter, "path is not valid UTF-8: {}", path.display())
            }
            Self::RequestIdExhausted => write!(formatter, "bridge request IDs are exhausted"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

/// A single long-lived Findex BEAM child.
///
/// Methods are synchronous by design. A GUI can place the client on a worker
/// thread without introducing an async runtime into this small library.
pub struct Client {
    child: Option<Child>,
    input: Option<BufWriter<ChildStdin>>,
    output: BufReader<ChildStdout>,
    next_request_id: u64,
    beam_pid: String,
    events: VecDeque<BridgeEvent>,
}

impl Client {
    /// Spawns a command that starts `FindexRust.Bridge` on stdin/stdout.
    pub fn spawn(mut command: Command) -> Result<Self, Error> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        let Some(input) = child.stdin.take() else {
            terminate(&mut child);
            return Err(Error::Protocol("backend stdin was not piped".to_owned()));
        };
        let Some(output) = child.stdout.take() else {
            terminate(&mut child);
            return Err(Error::Protocol("backend stdout was not piped".to_owned()));
        };
        let mut output = BufReader::new(output);

        let startup = match read_typed_frame::<Ready>(&mut output) {
            Ok(startup) => startup,
            Err(error) => {
                terminate(&mut child);
                return Err(error);
            }
        };

        if startup.event != "ready" {
            terminate(&mut child);
            return Err(Error::Protocol(format!(
                "expected ready event, received {}",
                startup.event
            )));
        }

        if startup.protocol != PROTOCOL_VERSION {
            terminate(&mut child);
            return Err(Error::Protocol(format!(
                "protocol {} is unsupported; expected {PROTOCOL_VERSION}",
                startup.protocol
            )));
        }

        Ok(Self {
            child: Some(child),
            input: Some(BufWriter::new(input)),
            output,
            next_request_id: 0,
            beam_pid: startup.pid,
            events: VecDeque::new(),
        })
    }

    /// Returns the OS process ID reported by BEAM during the handshake.
    pub fn beam_pid(&self) -> &str {
        &self.beam_pid
    }

    /// Returns the child process ID assigned by the operating system.
    pub fn process_id(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    /// Verifies that the same BEAM instance is responsive.
    pub fn ping(&mut self) -> Result<String, Error> {
        let id = self.reserve_request_id()?;
        let response: PingResult = self.request(&PingRequest { id, op: "ping" }, id)?;
        if response.protocol != PROTOCOL_VERSION {
            return Err(Error::Protocol(format!(
                "ping reported protocol {}",
                response.protocol
            )));
        }
        Ok(response.pid)
    }

    /// Starts a targeted traversal and retains its live native store in BEAM.
    pub fn start_scan(
        &mut self,
        root: impl AsRef<Path>,
        options: &ScanOptions,
    ) -> Result<IndexHandle, Error> {
        let root = root.as_ref();
        let root_text = root
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(root.to_path_buf()))?;
        let id = self.reserve_request_id()?;

        let handle: IndexHandle = self.request(
            &StartScanRequest {
                id,
                op: "start_scan",
                root: root_text,
                fields: &options.fields,
                concurrency: options.concurrency,
                buffer_size: options.buffer_size,
                ranking: options.ranking,
                mount_policy: options.mount_policy,
                failure_sample_limit: options.failure_sample_limit,
            },
            id,
        )?;
        Ok(handle)
    }

    /// Reads live queue, counters, and native-store measurements.
    pub fn index_status(&mut self, index_id: IndexId) -> Result<IndexStatus, Error> {
        let id = self.reserve_request_id()?;
        self.request(
            &IndexRequest {
                id,
                op: "index_status",
                index_id,
            },
            id,
        )
    }

    /// Pulls a bounded page from an index's independent completion cursor.
    pub fn completed_directories(
        &mut self,
        index_id: IndexId,
        cursor: u64,
        limit: u32,
    ) -> Result<CompletionPage, Error> {
        let id = self.reserve_request_id()?;
        self.request(
            &CompletionRequest {
                id,
                op: "completed_directories",
                index_id,
                cursor,
                limit,
            },
            id,
        )
    }

    /// Fetches one bounded row page from an immutable directory block.
    pub fn fetch_directory(
        &mut self,
        index_id: IndexId,
        directory_id: u32,
        offset: u64,
        limit: u32,
    ) -> Result<DirectoryPage, Error> {
        let id = self.reserve_request_id()?;
        self.request(
            &FetchDirectoryRequest {
                id,
                op: "fetch_directory",
                index_id,
                directory_id,
                offset,
                limit,
            },
            id,
        )
    }

    /// Summarizes completed directories in one round trip: byte totals,
    /// the largest regular files, and a log₂ size histogram, computed
    /// inside the BEAM from the packed native blocks. Far cheaper than
    /// paging every entry through [`Self::fetch_directory`].
    pub fn summarize_directories(
        &mut self,
        index_id: IndexId,
        directory_ids: &[u32],
        size_field: &str,
        largest_limit: u32,
    ) -> Result<Vec<DirectoryStats>, Error> {
        let id = self.reserve_request_id()?;
        let result: DirectoryStatsResult = self.request(
            &SummarizeDirectoriesRequest {
                id,
                op: "summarize_directories",
                index_id,
                directory_ids,
                size_field,
                largest_limit,
            },
            id,
        )?;
        Ok(result.summaries)
    }

    /// Waits for a retained traversal's pushed terminal result.
    pub fn await_scan(&mut self, index_id: IndexId) -> Result<ScanResult, Error> {
        loop {
            if let Some(event) = self.take_index_event(index_id) {
                return self.finished_event(event, index_id);
            }
            let event = self.read_event()?;
            if event.index_id() == Some(index_id) {
                return self.finished_event(event, index_id);
            }
            self.events.push_back(event);
        }
    }

    /// Cancels a running traversal if necessary and releases its native store.
    pub fn release_index(&mut self, index_id: IndexId) -> Result<(), Error> {
        let id = self.reserve_request_id()?;
        let response: ReleaseResult = self.request(
            &IndexRequest {
                id,
                op: "release_index",
                index_id,
            },
            id,
        )?;

        if response.released && response.index_id == index_id {
            self.events
                .retain(|event| event.index_id() != Some(index_id));
            Ok(())
        } else {
            Err(Error::Protocol("backend declined index release".to_owned()))
        }
    }

    /// Runs one traversal to completion and releases it after copying the report.
    pub fn scan(
        &mut self,
        root: impl AsRef<Path>,
        options: &ScanOptions,
    ) -> Result<ScanResult, Error> {
        let index = self.start_scan(root, options)?;
        let result = self.await_scan(index.index_id);
        let release = self.release_index(index.index_id);

        match result {
            Ok(result) => {
                release?;
                Ok(result)
            }
            Err(error) => {
                let _ = release;
                Err(error)
            }
        }
    }

    /// Requests graceful shutdown and waits for the BEAM child to exit.
    pub fn shutdown(mut self) -> Result<(), Error> {
        let id = self.reserve_request_id()?;
        let response: ShutdownResult = self.request(&ShutdownRequest { id, op: "shutdown" }, id)?;
        if !response.shutdown {
            return Err(Error::Protocol("backend declined shutdown".to_owned()));
        }

        self.input.take();
        let mut child = self
            .child
            .take()
            .ok_or_else(|| Error::Protocol("backend child is missing".to_owned()))?;
        let status = child.wait()?;

        if status.success() {
            Ok(())
        } else {
            Err(Error::BackendFailed(status))
        }
    }

    fn reserve_request_id(&mut self) -> Result<u64, Error> {
        let id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(Error::RequestIdExhausted)?;
        Ok(id)
    }

    fn request<T, R>(&mut self, request: &T, expected_id: u64) -> Result<R, Error>
    where
        T: Serialize,
        R: DeserializeOwned,
    {
        let request = wire::Value::from_json(serde_json::to_value(request)?)
            .map_err(|error| Error::Protocol(error.to_string()))?;
        self.request_value(&request, expected_id)
    }

    fn request_value<R>(&mut self, request: &wire::Value, expected_id: u64) -> Result<R, Error>
    where
        R: DeserializeOwned,
    {
        self.write_wire_value(request)?;

        loop {
            match self.read_message()? {
                Message::Event(event) => {
                    if self.register_event(&event)? {
                        self.events.push_back(event);
                    }
                }
                Message::Response(response) => {
                    if response.id != Some(expected_id) {
                        return Err(Error::Protocol(format!(
                            "response ID {:?} does not match request {expected_id}",
                            response.id
                        )));
                    }

                    return match response.status.as_str() {
                        "ok" => {
                            let result = response.result.ok_or_else(|| {
                                Error::Protocol("successful response has no result".to_owned())
                            })?;
                            Ok(serde_json::from_value(result)?)
                        }
                        "error" => Err(Error::Backend(response.error.ok_or_else(|| {
                            Error::Protocol("error response has no error object".to_owned())
                        })?)),
                        status => Err(Error::Protocol(format!(
                            "unknown response status: {status}"
                        ))),
                    };
                }
            }
        }
    }

    fn write_wire_value(&mut self, value: &wire::Value) -> Result<(), Error> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| Error::Protocol("backend stdin is closed".to_owned()))?;
        wire::write_frame(input, PROTOCOL_VERSION as u8, value)?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Message, Error> {
        let value = match wire::read_frame(&mut self.output, PROTOCOL_VERSION as u8) {
            Ok(value) => value,
            Err(error) if error.to_string() == "unexpected end of bridge stream" => {
                let exit_code = self
                    .child
                    .as_mut()
                    .and_then(|child| child.try_wait().ok().flatten())
                    .and_then(|status| status.code());
                return Err(Error::BackendExited(exit_code));
            }
            Err(error) => return Err(Error::Protocol(error.to_string())),
        };
        let value = value
            .into_json()
            .map_err(|error| Error::Protocol(error.to_string()))?;

        if value.get("event").is_some() {
            parse_event(value).map(Message::Event)
        } else {
            Ok(Message::Response(serde_json::from_value(value)?))
        }
    }

    fn read_event(&mut self) -> Result<BridgeEvent, Error> {
        loop {
            match self.read_message()? {
                Message::Event(event) => {
                    if self.register_event(&event)? {
                        return Ok(event);
                    }
                }
                Message::Response(response) => {
                    return Err(Error::Protocol(format!(
                        "unexpected response frame with ID {:?}",
                        response.id
                    )));
                }
            }
        }
    }

    fn register_event(&mut self, event: &BridgeEvent) -> Result<bool, Error> {
        match event {
            BridgeEvent::ProtocolError { message } => {
                return Err(Error::Protocol(message.clone()));
            }
            BridgeEvent::Ready => {
                return Err(Error::Protocol("duplicate ready event".to_owned()));
            }
            BridgeEvent::ScanFinished { .. } => {}
        }
        Ok(true)
    }

    fn take_index_event(&mut self, index_id: IndexId) -> Option<BridgeEvent> {
        let position = self
            .events
            .iter()
            .position(|event| event.index_id() == Some(index_id))?;
        self.events.remove(position)
    }

    fn finished_event(&self, event: BridgeEvent, index_id: IndexId) -> Result<ScanResult, Error> {
        match event {
            BridgeEvent::ScanFinished { result, error, .. } => {
                if let Some(error) = error {
                    Err(Error::Protocol(error))
                } else {
                    result
                        .map(|result| *result)
                        .ok_or_else(|| Error::Protocol("scan completion has no result".to_owned()))
                }
            }
            BridgeEvent::ProtocolError { message } => Err(Error::Protocol(message)),
            BridgeEvent::Ready => Err(Error::Protocol(format!(
                "duplicate ready event while awaiting index {}",
                index_id.0
            ))),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.input.take();
        if let Some(child) = self.child.as_mut() {
            terminate(child);
        }
    }
}

enum Message {
    Response(WireResponse),
    Event(BridgeEvent),
}

enum BridgeEvent {
    Ready,
    ScanFinished {
        index_id: IndexId,
        result: Option<Box<ScanResult>>,
        error: Option<String>,
    },
    ProtocolError {
        message: String,
    },
}

impl BridgeEvent {
    fn index_id(&self) -> Option<IndexId> {
        match self {
            Self::ScanFinished { index_id, .. } => Some(*index_id),
            Self::Ready | Self::ProtocolError { .. } => None,
        }
    }
}

fn parse_event(value: Value) -> Result<BridgeEvent, Error> {
    let event = value
        .get("event")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol("event frame has no string event name".to_owned()))?;

    match event {
        "ready" => Ok(BridgeEvent::Ready),
        "scan_finished" => {
            let event: ScanFinishedEvent = serde_json::from_value(value)?;
            Ok(BridgeEvent::ScanFinished {
                index_id: event.index_id,
                result: event.result.map(Box::new),
                error: event.error,
            })
        }
        "protocol_error" => {
            let event: ProtocolErrorEvent = serde_json::from_value(value)?;
            Ok(BridgeEvent::ProtocolError {
                message: event.message,
            })
        }
        event => Err(Error::Protocol(format!("unknown bridge event: {event}"))),
    }
}

#[derive(Deserialize)]
struct Ready {
    event: String,
    protocol: u32,
    pid: String,
}

#[derive(Deserialize)]
struct ScanFinishedEvent {
    index_id: IndexId,
    #[serde(default)]
    result: Option<ScanResult>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ProtocolErrorEvent {
    message: String,
}

#[derive(Deserialize)]
struct WireResponse {
    id: Option<u64>,
    status: String,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<BackendError>,
}

#[derive(Serialize)]
struct PingRequest {
    id: u64,
    op: &'static str,
}

#[derive(Deserialize)]
struct PingResult {
    pid: String,
    protocol: u32,
}

#[derive(Serialize)]
struct StartScanRequest<'a> {
    id: u64,
    op: &'static str,
    root: &'a str,
    fields: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    concurrency: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    buffer_size: Option<u32>,
    ranking: Ranking,
    mount_policy: MountPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_sample_limit: Option<u32>,
}

#[derive(Serialize)]
struct IndexRequest {
    id: u64,
    op: &'static str,
    index_id: IndexId,
}

#[derive(Serialize)]
struct CompletionRequest {
    id: u64,
    op: &'static str,
    index_id: IndexId,
    cursor: u64,
    limit: u32,
}

#[derive(Serialize)]
struct SummarizeDirectoriesRequest<'a> {
    id: u64,
    op: &'static str,
    index_id: IndexId,
    directory_ids: &'a [u32],
    size_field: &'a str,
    largest_limit: u32,
}

#[derive(Deserialize)]
struct DirectoryStatsResult {
    summaries: Vec<DirectoryStats>,
}

#[derive(Serialize)]
struct FetchDirectoryRequest {
    id: u64,
    op: &'static str,
    index_id: IndexId,
    directory_id: u32,
    offset: u64,
    limit: u32,
}

#[derive(Deserialize)]
struct ReleaseResult {
    index_id: IndexId,
    released: bool,
}

#[derive(Serialize)]
struct ShutdownRequest {
    id: u64,
    op: &'static str,
}

#[derive(Deserialize)]
struct ShutdownResult {
    shutdown: bool,
}

fn read_typed_frame<T: DeserializeOwned>(reader: &mut impl io::Read) -> Result<T, Error> {
    let value = wire::read_frame(reader, PROTOCOL_VERSION as u8)
        .map_err(|error| Error::Protocol(error.to_string()))?
        .into_json()
        .map_err(|error| Error::Protocol(error.to_string()))?;
    Ok(serde_json::from_value(value)?)
}

fn terminate(child: &mut Child) {
    match child.try_wait() {
        Ok(Some(_status)) => {}
        Ok(None) | Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestTree(PathBuf);

    impl TestTree {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is before Unix epoch")
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("findex-rust-client-{}-{nonce}", std::process::id()));
            fs::create_dir_all(root.join("directory")).expect("create test directories");
            fs::write(root.join("file.txt"), b"one").expect("create root file");
            fs::write(root.join("directory/nested.txt"), b"two").expect("create nested file");
            Self(root)
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn project_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("rust_client must be inside the shared workspace")
            .to_path_buf()
    }

    #[test]
    fn one_beam_instance_handles_repeated_targeted_scans() {
        let tree = TestTree::new();
        let mut client = Client::spawn(development_command(project_root()))
            .expect("start the development bridge; compile rust_client/backend first");

        let initial_pid = client.beam_pid().to_owned();
        assert_eq!(client.ping().expect("ping bridge"), initial_pid);

        let options = ScanOptions {
            fields: vec![
                "type".to_owned(),
                "file_id".to_owned(),
                "data_size".to_owned(),
            ],
            concurrency: Some(2),
            ..ScanOptions::default()
        };
        let index = client.start_scan(&tree.0, &options).expect("start scan");
        let status = client
            .index_status(index.index_id)
            .expect("read live status");
        assert_eq!(status.index_id, index.index_id);

        let mut first_completion = None;
        for _attempt in 0..1_000 {
            let page = client
                .completed_directories(index.index_id, 0, 1)
                .expect("read live completion journal");
            if !page.directory_ids.is_empty() {
                first_completion = Some(page);
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        let first_completion = first_completion.expect("root directory should be published");
        assert_eq!(first_completion.directory_ids, [0]);

        let first_page = client
            .fetch_directory(index.index_id, 0, 0, 1)
            .expect("fetch first root page");
        let second_page = client
            .fetch_directory(index.index_id, 0, first_page.next_offset, 1)
            .expect("fetch second root page");
        assert_eq!(first_page.entries.len(), 1);
        assert_eq!(second_page.entries.len(), 1);
        assert!(second_page.done);

        let stats = client
            .summarize_directories(index.index_id, &[0], "data_size", 8)
            .expect("summarize the root directory");
        assert_eq!(stats.len(), 1);
        let root_stats = &stats[0];
        assert_eq!(root_stats.directory_id, 0);
        assert_eq!(root_stats.entry_count, 2);
        assert_eq!(root_stats.child_count, 1);
        // Only `file.txt` (3 bytes) is an immediate regular file of the root.
        assert_eq!(root_stats.size_bytes, 3);
        assert_eq!(root_stats.largest.len(), 1);
        assert_eq!(root_stats.largest[0].size, 3);
        assert_eq!(root_stats.largest[0].name.as_utf8(), Some("file.txt"));
        // 3 bytes lands in the [2, 4) log2 bucket.
        assert_eq!(root_stats.histogram, vec![[2, 1, 3]]);

        let without_largest = client
            .summarize_directories(index.index_id, &[0], "data_size", 0)
            .expect("zero largest-file limit should still summarize bytes");
        assert_eq!(without_largest[0].size_bytes, 3);
        assert!(without_largest[0].largest.is_empty());

        assert!(matches!(
            client.summarize_directories(index.index_id, &[0], "modified_at", 8),
            Err(Error::Backend(BackendError { ref code, .. })) if code == "invalid_request"
        ));

        let pages = [&first_page, &second_page];
        let names = pages
            .iter()
            .flat_map(|page| page.entries.iter())
            .filter_map(|row| row.values.get("name").cloned())
            .collect::<Vec<_>>();
        assert!(names.contains(&Value::String("directory".to_owned())));
        assert!(names.contains(&Value::String("file.txt".to_owned())));
        assert!(
            pages
                .iter()
                .flat_map(|page| page.entries.iter())
                .all(|row| row.values["file_id"].is_u64())
        );

        let first = client.await_scan(index.index_id).expect("await first scan");

        assert_eq!(first.outcome, ScanOutcome::Ok);
        assert!(first.report.complete);
        assert_eq!(first.report.entries, 3);
        assert_eq!(first.report.store.directory_count, 2);

        let completions = client
            .completed_directories(index.index_id, 0, 16)
            .expect("read complete journal");
        assert_eq!(completions.cursor, 2);
        assert_eq!(completions.directory_ids.len(), 2);

        client
            .release_index(index.index_id)
            .expect("release retained native store");
        assert!(matches!(
            client.index_status(index.index_id),
            Err(Error::Backend(BackendError { ref code, .. })) if code == "unknown_index"
        ));

        let second = client.scan(&tree.0, &options).expect("convenience scan");
        assert_eq!(second.report.entries, first.report.entries);
        assert_eq!(client.ping().expect("ping after scans"), initial_pid);

        client.shutdown().expect("graceful shutdown");
    }

    #[test]
    fn named_rankings_run_to_completion_inside_findex() {
        let tree = TestTree::new();
        let mut client = Client::spawn(development_command(project_root()))
            .expect("start the development bridge; compile rust_client/backend first");
        for ranking in [Ranking::NameBiased, Ranking::Macos] {
            let options = ScanOptions {
                fields: vec!["type".to_owned(), "file_id".to_owned()],
                concurrency: Some(2),
                ranking,
                mount_policy: MountPolicy::Cross,
                ..ScanOptions::default()
            };

            let index = client
                .start_scan(&tree.0, &options)
                .expect("start internally ranked scan");
            let status = client
                .index_status(index.index_id)
                .expect("read ranking status");
            assert_eq!(status.ranking, ranking);

            let result = client
                .await_scan(index.index_id)
                .expect("await internally ranked scan");
            assert_eq!(result.outcome, ScanOutcome::Ok);
            assert_eq!(result.report.entries, 3);

            client
                .release_index(index.index_id)
                .expect("release internally ranked index");
        }

        client.shutdown().expect("graceful shutdown");
    }

    #[test]
    fn default_options_request_the_minimum_recursive_schema() {
        let options = ScanOptions::default();
        assert_eq!(options.fields, ["type"]);
        assert_eq!(options.ranking, Ranking::Default);
        assert_eq!(options.mount_policy, MountPolicy::StayOnFilesystem);
    }

    #[test]
    fn command_accepts_os_string_paths() {
        let root: &OsStr = OsStr::new("/tmp/findex-workspace");
        let command = development_command(root);
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new(root).join("rust_client/backend").as_path())
        );
    }
}
