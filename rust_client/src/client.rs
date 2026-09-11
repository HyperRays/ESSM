//! Child-process ownership and the synchronous request/event lifecycle.

use std::collections::VecDeque;
use std::io::{self, BufReader, BufWriter};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::*;
use crate::{Error, wire};

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
