use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use findex_client::{
    Client, DirectoryState, EncodedBinary, IndexId, ScanOptions, development_command,
};
use serde::Serialize;
use serde_json::Value;

const PAGE_SIZE: u32 = 4_096;
const TRACE_SCHEMA_VERSION: u32 = 1;
const RECENT_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug, Default, Serialize)]
struct Observation {
    allocated_bytes: u64,
    logical_bytes: u64,
    entry_count: Option<u64>,
    modified_seconds: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct ImmediateStats {
    entry_count: u64,
    regular_count: u64,
    directory_count: u64,
    symlink_count: u64,
    other_count: u64,
    allocated_bytes: u64,
    logical_bytes: u64,
    writable_count: u64,
    hidden_count: u64,
    recent_count: u64,
    code_bytes: u64,
    config_bytes: u64,
    media_bytes: u64,
    archive_bytes: u64,
    recent_code_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
struct TraceDirectory {
    id: u32,
    parent_id: Option<u32>,
    name: String,
    state: &'static str,
    depth: u32,
    observation: Observation,
    children: Vec<u32>,
    immediate: ImmediateStats,
}

#[derive(Debug, Serialize)]
struct Trace {
    schema_version: u32,
    root: String,
    captured_unix_seconds: u64,
    complete: bool,
    elapsed_ms: f64,
    entries: u64,
    directory_failures: u64,
    directories: Vec<TraceDirectory>,
}

fn binary_label(value: &EncodedBinary) -> String {
    match value {
        EncodedBinary::Utf8(value) => value.clone(),
        EncodedBinary::Base64 { base64 } => format!("<base64:{base64}>"),
    }
}

fn string_value(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn unsigned_value(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

fn timestamp_seconds(value: Option<&Value>) -> Option<i64> {
    value
        .and_then(Value::as_array)
        .and_then(|parts| parts.first())
        .and_then(Value::as_i64)
}

fn entry_name(values: &BTreeMap<String, Value>) -> &str {
    string_value(values.get("name")).unwrap_or("<non-utf8>")
}

fn observation(values: &BTreeMap<String, Value>) -> Observation {
    Observation {
        allocated_bytes: unsigned_value(values.get("allocated_size")).unwrap_or(0),
        logical_bytes: unsigned_value(values.get("total_size")).unwrap_or(0),
        entry_count: unsigned_value(values.get("directory_entry_count")),
        modified_seconds: timestamp_seconds(values.get("modified_at")),
    }
}

fn extension_group(name: &str) -> Option<&'static str> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    match extension.as_str() {
        "c" | "cc" | "cpp" | "cxx" | "ex" | "exs" | "go" | "h" | "hh" | "hpp" | "java" | "js"
        | "jsx" | "kt" | "kts" | "lean" | "lua" | "m" | "mm" | "py" | "rb" | "rs" | "sh"
        | "swift" | "ts" | "tsx" => Some("code"),
        "conf" | "config" | "ini" | "json" | "json5" | "plist" | "toml" | "xml" | "yaml"
        | "yml" => Some("config"),
        "aac" | "avi" | "flac" | "gif" | "heic" | "jpeg" | "jpg" | "m4a" | "mkv" | "mov"
        | "mp3" | "mp4" | "png" | "tif" | "tiff" | "wav" | "webm" | "webp" => Some("media"),
        "7z" | "bz2" | "dmg" | "gz" | "iso" | "rar" | "tar" | "tgz" | "xz" | "zip" | "zst" => {
            Some("archive")
        }
        _ => None,
    }
}

fn note_entry(stats: &mut ImmediateStats, values: &BTreeMap<String, Value>, captured: i64) {
    stats.entry_count = stats.entry_count.saturating_add(1);
    let name = entry_name(values);
    if name.starts_with('.') {
        stats.hidden_count = stats.hidden_count.saturating_add(1);
    }
    if unsigned_value(values.get("mode")).is_some_and(|mode| mode & 0o222 != 0) {
        stats.writable_count = stats.writable_count.saturating_add(1);
    }
    let recent = timestamp_seconds(values.get("modified_at")).is_some_and(|modified| {
        modified <= captured && captured.saturating_sub(modified) <= RECENT_SECONDS
    });
    if recent {
        stats.recent_count = stats.recent_count.saturating_add(1);
    }

    match string_value(values.get("type")) {
        Some("regular") => {
            stats.regular_count = stats.regular_count.saturating_add(1);
            let allocated = unsigned_value(values.get("allocated_size")).unwrap_or(0);
            let logical = unsigned_value(values.get("total_size")).unwrap_or(0);
            stats.allocated_bytes = stats.allocated_bytes.saturating_add(allocated);
            stats.logical_bytes = stats.logical_bytes.saturating_add(logical);
            match extension_group(name) {
                Some("code") => {
                    stats.code_bytes = stats.code_bytes.saturating_add(allocated);
                    if recent {
                        stats.recent_code_bytes = stats.recent_code_bytes.saturating_add(allocated);
                    }
                }
                Some("config") => stats.config_bytes = stats.config_bytes.saturating_add(allocated),
                Some("media") => stats.media_bytes = stats.media_bytes.saturating_add(allocated),
                Some("archive") => {
                    stats.archive_bytes = stats.archive_bytes.saturating_add(allocated)
                }
                _ => {}
            }
        }
        Some("directory") => stats.directory_count = stats.directory_count.saturating_add(1),
        Some("symlink") => stats.symlink_count = stats.symlink_count.saturating_add(1),
        _ => stats.other_count = stats.other_count.saturating_add(1),
    }
}

fn state_name(state: DirectoryState) -> &'static str {
    match state {
        DirectoryState::Pending => "pending",
        DirectoryState::Published => "published",
        DirectoryState::Failed => "failed",
    }
}

fn completion_ids(
    client: &mut Client,
    index_id: IndexId,
) -> Result<Vec<u32>, findex_client::Error> {
    let mut cursor = 0;
    let mut ids = Vec::new();
    loop {
        let page = client.completed_directories(index_id, cursor, PAGE_SIZE)?;
        cursor = page.cursor;
        if page.directory_ids.is_empty() {
            return Ok(ids);
        }
        ids.extend(page.directory_ids);
    }
}

fn capture_directory(
    client: &mut Client,
    index_id: IndexId,
    directory_id: u32,
    captured: i64,
    child_observations: &mut BTreeMap<u32, Observation>,
) -> Result<TraceDirectory, findex_client::Error> {
    let mut offset = 0;
    let mut metadata = None;
    let mut children = Vec::new();
    let mut immediate = ImmediateStats::default();

    loop {
        let page = client.fetch_directory(index_id, directory_id, offset, PAGE_SIZE)?;
        if metadata.is_none() {
            metadata = Some((
                page.parent_id,
                binary_label(&page.name),
                state_name(page.state),
            ));
        }
        for row in page.entries {
            note_entry(&mut immediate, &row.values, captured);
            if let Some(child_id) = row.child_directory_id {
                children.push(child_id);
                child_observations.insert(child_id, observation(&row.values));
            }
        }
        if page.done {
            break;
        }
        offset = page.next_offset;
    }

    let (parent_id, name, state) = metadata.expect("every fetch returns a first page");
    Ok(TraceDirectory {
        id: directory_id,
        parent_id,
        name,
        state,
        depth: 0,
        observation: Observation::default(),
        children,
        immediate,
    })
}

fn capture(root: &Path, destination: &Path, concurrency: u32) -> Result<(), Box<dyn StdError>> {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rust_client must live directly below the shared workspace root");
    let mut client = Client::spawn(development_command(project_root))?;
    let options = ScanOptions {
        fields: vec![
            "type".to_owned(),
            "allocated_size".to_owned(),
            "total_size".to_owned(),
            "directory_entry_count".to_owned(),
            "modified_at".to_owned(),
            "mode".to_owned(),
        ],
        concurrency: Some(concurrency),
        ..ScanOptions::default()
    };
    eprintln!(
        "capture-trace: scanning {} with {concurrency} workers",
        root.display()
    );
    let index = client.start_scan(root, &options)?;
    let result = client.await_scan(index.index_id)?;
    let captured = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let ids = completion_ids(&mut client, index.index_id)?;
    eprintln!(
        "capture-trace: aggregating {} terminal directories and {} entries",
        ids.len(),
        result.report.entries
    );

    let mut child_observations = BTreeMap::new();
    let mut directories = BTreeMap::new();
    for (position, directory_id) in ids.into_iter().enumerate() {
        let directory = capture_directory(
            &mut client,
            index.index_id,
            directory_id,
            captured as i64,
            &mut child_observations,
        )?;
        directories.insert(directory_id, directory);
        if position > 0 && position % 10_000 == 0 {
            eprintln!("capture-trace: aggregated {position} directories");
        }
    }

    let ordered_ids = directories.keys().copied().collect::<Vec<_>>();
    for directory_id in &ordered_ids {
        let parent_id = directories[directory_id].parent_id;
        let depth = match parent_id {
            Some(parent_id) => directories
                .get(&parent_id)
                .map_or(0, |parent| parent.depth.saturating_add(1)),
            None => 0,
        };
        let directory = directories
            .get_mut(directory_id)
            .expect("directory ID came from this map");
        directory.depth = depth;
        directory.observation = child_observations.remove(directory_id).unwrap_or_default();
    }

    let trace = Trace {
        schema_version: TRACE_SCHEMA_VERSION,
        root: result.report.root.clone(),
        captured_unix_seconds: captured,
        complete: result.report.complete,
        elapsed_ms: result.report.elapsed_ms,
        entries: result.report.entries,
        directory_failures: result.report.store.failed_directory_count,
        directories: directories.into_values().collect(),
    };
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = BufWriter::new(File::create(destination)?);
    serde_json::to_writer(output, &trace)?;
    client.release_index(index.index_id)?;
    client.shutdown()?;
    eprintln!("capture-trace: wrote {}", destination.display());
    Ok(())
}

fn main() -> Result<(), Box<dyn StdError>> {
    let mut arguments = std::env::args_os().skip(1);
    let Some(root) = arguments.next() else {
        eprintln!("usage: capture_trace DIRECTORY OUTPUT.json [CONCURRENCY]");
        std::process::exit(64);
    };
    let Some(destination) = arguments.next() else {
        eprintln!("usage: capture_trace DIRECTORY OUTPUT.json [CONCURRENCY]");
        std::process::exit(64);
    };
    let concurrency = match arguments.next() {
        Some(value) => value
            .to_str()
            .ok_or("CONCURRENCY must be UTF-8")?
            .parse::<u32>()
            .map_err(|_| "CONCURRENCY must be a positive integer")?,
        None => 16,
    };
    if concurrency == 0 || arguments.next().is_some() {
        return Err("usage: capture_trace DIRECTORY OUTPUT.json [CONCURRENCY]".into());
    }
    capture(
        &PathBuf::from(root),
        &PathBuf::from(destination),
        concurrency,
    )
}
