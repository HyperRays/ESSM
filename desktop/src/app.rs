//! UI-framework-agnostic presentation model for one scan.
//!
//! Consumes batched [`BackendEvent`]s and maintains bounded views of the index:
//! counters, the completion feed, size leaderboards, failure records, the
//! size tree behind the visualizations, and the throughput history. This is
//! the only layer that turns transport data into UI-ready state.

use std::collections::{BTreeMap, VecDeque};

use findex_client::{DirectoryState, EncodedBinary, IndexState, MountPolicy, ScanOutcome};

use crate::backend::{BackendEvent, DirectorySummary, SizeHistogram};

/// Instantaneous-rate samples retained for the throughput graph.
pub const THROUGHPUT_HISTORY: usize = 240;
/// Fixed time base of one throughput sample; with [`THROUGHPUT_HISTORY`]
/// samples the graph retains one minute of rate history.
const RATE_SAMPLE_MS: f64 = 250.0;
/// Global size leaderboard capacity for files and directories.
const TOP_LIMIT: usize = 100;
/// Failed directories retained for the failures view before the final report.
const FAILURE_LIMIT: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tab {
    Treemap,
    Sunburst,
    Graph,
    Diagnostics,
}

impl Tab {
    pub const ALL: [Self; 4] = [
        Self::Treemap,
        Self::Sunburst,
        Self::Graph,
        Self::Diagnostics,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Treemap => "Treemap",
            Self::Sunburst => "Sunburst",
            Self::Graph => "Graph",
            Self::Diagnostics => "Diagnostics",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecentDirectory {
    pub id: u32,
    pub state: String,
    pub entries: u64,
    pub children: u64,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct SizedEntry {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct FailedDirectory {
    pub id: u32,
    pub path: String,
    pub phase: String,
    pub category: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Counters {
    pub elapsed_ms: f64,
    pub entries_per_second: f64,
    pub entries: u64,
    pub allocated_bytes: u64,
    pub directories_reserved: u64,
    pub directories_completed: u64,
    pub directories_published: u64,
    pub directories_failed: u64,
    pub directories_pending: u64,
    pub scheduler_pending: u64,
    pub in_flight: u64,
    pub regular_files: u64,
    pub directory_entries: u64,
    pub symlinks: u64,
    pub other: u64,
    pub metadata_errors: u64,
    pub skipped_mounts: u64,
    pub native_bytes: u64,
    pub block_bytes: u64,
    pub payload_bytes: u64,
    pub directory_table_bytes: u64,
    pub journal_bytes: u64,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NoticeLevel {
    Info,
    Error,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub level: NoticeLevel,
}

const NO_PARENT: u32 = u32::MAX;
/// Hard bound on ancestor walks; guards against a corrupt parent cycle.
const MAX_DEPTH: usize = 512;

#[derive(Clone, Debug)]
struct SizeNode {
    parent: u32,
    subtree_bytes: u64,
    /// Bytes of the directory's immediate (non-directory) entries.
    own_bytes: u64,
    /// Count of the directory's immediate non-directory entries.
    own_entries: u64,
    /// The directory's own name (the root stores its full path). Empty
    /// until the directory has been seen.
    name: String,
    children: Vec<u32>,
    children_dirty: bool,
}

/// Recursive directory sizes and names, propagated up the tree as
/// completions arrive.
///
/// Directory IDs are dense (the native store allocates them sequentially
/// from 0) and a parent always completes before its children, so a flat
/// vector indexed by ID suffices. This is the one deliberately unbounded
/// structure in the app — roughly 64 bytes plus the name per directory —
/// and in exchange every path reconstructs exactly, with no cache
/// eviction ever degrading a label to an ID.
#[derive(Debug, Default)]
pub struct SizeTree {
    nodes: Vec<SizeNode>,
    dirty_children: Vec<u32>,
}

impl SizeTree {
    fn ensure(&mut self, id: u32) {
        let required = id as usize + 1;
        if self.nodes.len() < required {
            self.nodes.resize_with(required, || SizeNode {
                parent: NO_PARENT,
                subtree_bytes: 0,
                own_bytes: 0,
                own_entries: 0,
                name: String::new(),
                children: Vec::new(),
                children_dirty: false,
            });
        }
    }

    fn mark_children_dirty(&mut self, id: u32) {
        let node = &mut self.nodes[id as usize];
        if !node.children_dirty {
            node.children_dirty = true;
            self.dirty_children.push(id);
        }
    }

    /// Records the parent link and child adjacency without touching sizes.
    pub fn link(&mut self, id: u32, parent: u32) {
        self.ensure(id);
        self.ensure(parent);
        if self.nodes[id as usize].parent == NO_PARENT && id != parent {
            self.nodes[id as usize].parent = parent;
            self.nodes[parent as usize].children.push(id);
            self.mark_children_dirty(parent);
        }
    }

    /// Adds a directory's immediate bytes to itself and every ancestor.
    pub fn add_bytes(&mut self, id: u32, bytes: u64) {
        self.ensure(id);
        let mut current = id as usize;
        for _ in 0..MAX_DEPTH {
            self.nodes[current].subtree_bytes =
                self.nodes[current].subtree_bytes.saturating_add(bytes);
            let parent = self.nodes[current].parent;
            if parent == NO_PARENT {
                break;
            }
            self.mark_children_dirty(parent);
            current = parent as usize;
        }
    }

    /// Restores the size-descending child order once after a whole backend
    /// batch. Views can then reuse it without repeatedly sorting the same
    /// siblings during one redraw.
    fn finish_batch(&mut self) {
        for parent in std::mem::take(&mut self.dirty_children) {
            self.nodes[parent as usize].children_dirty = false;
            let mut children = std::mem::take(&mut self.nodes[parent as usize].children);
            children
                .sort_by_key(|&child| std::cmp::Reverse(self.nodes[child as usize].subtree_bytes));
            self.nodes[parent as usize].children = children;
        }
    }

    /// Records the directory's own name; empty names never overwrite.
    pub fn set_name(&mut self, id: u32, name: String) {
        if name.is_empty() {
            return;
        }
        self.ensure(id);
        self.nodes[id as usize].name = name;
    }

    pub fn name(&self, id: u32) -> Option<&str> {
        self.nodes
            .get(id as usize)
            .map(|node| node.name.as_str())
            .filter(|name| !name.is_empty())
    }

    /// Records the directory's immediate file bytes and entry count.
    pub fn set_own(&mut self, id: u32, bytes: u64, entries: u64) {
        self.ensure(id);
        self.nodes[id as usize].own_bytes = bytes;
        self.nodes[id as usize].own_entries = entries;
    }

    /// Bytes held by files directly inside the directory itself.
    pub fn own_bytes(&self, id: u32) -> u64 {
        self.nodes.get(id as usize).map_or(0, |node| node.own_bytes)
    }

    /// Count of non-directory entries directly inside the directory.
    pub fn own_entries(&self, id: u32) -> u64 {
        self.nodes
            .get(id as usize)
            .map_or(0, |node| node.own_entries)
    }

    pub fn subtree_bytes(&self, id: u32) -> u64 {
        self.nodes
            .get(id as usize)
            .map_or(0, |node| node.subtree_bytes)
    }

    pub fn children(&self, id: u32) -> &[u32] {
        self.nodes
            .get(id as usize)
            .map_or(&[], |node| node.children.as_slice())
    }

    pub fn parent(&self, id: u32) -> Option<u32> {
        self.nodes
            .get(id as usize)
            .map(|node| node.parent)
            .filter(|&parent| parent != NO_PARENT)
    }
}

#[derive(Debug)]
pub struct ScanModel {
    pub root: String,
    pub phase: String,
    pub mount_policy: String,
    pub concurrency: Option<u32>,
    pub counters: Counters,
    pub recent: VecDeque<RecentDirectory>,
    /// Newest sample first; the chart pins the newest sample to the right edge.
    pub throughput_history: VecDeque<u64>,
    /// Scheduler queue depth (pending + in flight), same time base.
    pub queue_history: VecDeque<u64>,
    /// Native store bytes, same time base.
    pub memory_history: VecDeque<u64>,
    pub size_histogram: SizeHistogram,
    pub tab: Tab,
    pub filter: String,
    pub notice: Option<Notice>,
    pub finished: bool,
    pub complete: bool,
    pub outcome: Option<String>,
    pub top_files: Vec<SizedEntry>,
    pub size_tree: SizeTree,
    /// Directory every visualization is rooted at.
    pub focus: u32,
    /// Directory highlighted in the inspector, if any.
    pub selected: Option<u32>,
    pub failed_directories: Vec<FailedDirectory>,
    pub metadata_error_counts: BTreeMap<String, u64>,
    pub directory_failure_counts: BTreeMap<String, u64>,
    pub directory_failure_reasons: BTreeMap<String, u64>,
    recent_limit: usize,
    last_rate_sample: Option<(f64, u64)>,
}

impl ScanModel {
    pub fn new(recent_limit: usize) -> Self {
        Self {
            root: String::new(),
            phase: "starting".to_owned(),
            mount_policy: String::new(),
            concurrency: None,
            counters: Counters::default(),
            recent: VecDeque::with_capacity(recent_limit),
            throughput_history: VecDeque::with_capacity(THROUGHPUT_HISTORY),
            queue_history: VecDeque::with_capacity(THROUGHPUT_HISTORY),
            memory_history: VecDeque::with_capacity(THROUGHPUT_HISTORY),
            size_histogram: SizeHistogram::default(),
            tab: Tab::Treemap,
            filter: String::new(),
            notice: None,
            finished: false,
            complete: false,
            outcome: None,
            top_files: Vec::new(),
            size_tree: SizeTree::default(),
            focus: 0,
            selected: None,
            failed_directories: Vec::new(),
            metadata_error_counts: BTreeMap::new(),
            directory_failure_counts: BTreeMap::new(),
            directory_failure_reasons: BTreeMap::new(),
            recent_limit,
            last_rate_sample: None,
        }
    }

    pub fn apply_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::Started {
                root,
                concurrency,
                mount_policy,
            } => {
                self.root = root;
                self.concurrency = concurrency;
                self.mount_policy = mount_policy_text(mount_policy).to_owned();
                self.phase = "running".to_owned();
            }
            BackendEvent::Update {
                status,
                elapsed_ms,
                allocated_bytes,
                directories,
                histogram,
            } => {
                for directory in directories {
                    self.add_directory(directory);
                }
                self.size_tree.finish_batch();
                self.size_histogram = *histogram;
                self.phase = match status.state {
                    IndexState::Running => "running",
                    IndexState::Finished => "finishing",
                }
                .to_owned();
                self.record_rate(
                    elapsed_ms,
                    status.store.entry_count,
                    status.pending + status.in_flight,
                    status.store.native_bytes,
                );
                let elapsed_seconds = (elapsed_ms / 1_000.0).max(0.001);
                self.counters = Counters {
                    elapsed_ms,
                    entries_per_second: status.store.entry_count as f64 / elapsed_seconds,
                    entries: status.store.entry_count,
                    allocated_bytes,
                    directories_reserved: status.store.directory_count,
                    directories_completed: status.store.completion_count,
                    directories_published: status.store.published_directory_count,
                    directories_failed: status.store.failed_directory_count,
                    directories_pending: status.store.pending_directory_count,
                    scheduler_pending: status.pending,
                    in_flight: status.in_flight,
                    regular_files: status.counters.regular_files,
                    directory_entries: status.counters.directories,
                    symlinks: status.counters.symlinks,
                    other: status.counters.other,
                    metadata_errors: status.counters.metadata_errors,
                    skipped_mounts: status.counters.skipped_mounts,
                    native_bytes: status.store.native_bytes,
                    block_bytes: status.store.block_bytes,
                    payload_bytes: status.store.payload_bytes,
                    directory_table_bytes: status.store.directory_table_bytes,
                    journal_bytes: status.store.completion_journal_bytes,
                };
                self.metadata_error_counts = status.counters.metadata_error_counts;
                self.directory_failure_counts = status.counters.directory_failure_counts;
                self.directory_failure_reasons = status.counters.directory_failure_reasons;
            }
            BackendEvent::Finished {
                result,
                allocated_bytes,
            } => {
                self.finished = true;
                self.complete = result.report.complete;
                self.outcome = Some(
                    match result.outcome {
                        ScanOutcome::Ok => "ok",
                        ScanOutcome::Fatal => "fatal",
                    }
                    .to_owned(),
                );
                self.phase = if result.outcome == ScanOutcome::Fatal {
                    "fatal"
                } else if result.report.complete {
                    "complete"
                } else {
                    "incomplete"
                }
                .to_owned();
                self.counters.elapsed_ms = result.report.elapsed_ms;
                self.counters.entries = result.report.entries;
                self.counters.entries_per_second =
                    result.report.entries as f64 / (result.report.elapsed_ms / 1_000.0).max(0.001);
                self.counters.allocated_bytes = allocated_bytes;
                self.counters.directories_reserved = result.report.store.directory_count;
                self.counters.directories_completed = result.report.store.completion_count;
                self.counters.directories_published = result.report.store.published_directory_count;
                self.counters.directories_failed = result.report.store.failed_directory_count;
                self.counters.directories_pending = result.report.store.pending_directory_count;
                self.counters.scheduler_pending = 0;
                self.counters.in_flight = 0;
                self.counters.regular_files = result.report.regular_files;
                self.counters.directory_entries = result.report.directories;
                self.counters.symlinks = result.report.symlinks;
                self.counters.other = result.report.other;
                self.counters.metadata_errors = result.report.metadata_errors;
                self.counters.skipped_mounts = result.report.skipped_mounts;
                self.counters.native_bytes = result.report.store.native_bytes;
                self.counters.block_bytes = result.report.store.block_bytes;
                self.counters.payload_bytes = result.report.store.payload_bytes;
                self.counters.directory_table_bytes = result.report.store.directory_table_bytes;
                self.counters.journal_bytes = result.report.store.completion_journal_bytes;
                self.metadata_error_counts = result.report.metadata_error_counts;
                self.directory_failure_counts = result.report.directory_failure_counts;
                self.directory_failure_reasons = result.report.directory_failure_reasons;
                for sample in result.report.directory_failure_samples {
                    let Ok(id) = u32::try_from(sample.id) else {
                        continue;
                    };
                    let failure = FailedDirectory {
                        id,
                        path: binary_text(&sample.path),
                        phase: sample.phase,
                        category: sample.category,
                        reason: sample.reason,
                    };
                    match self
                        .failed_directories
                        .iter()
                        .position(|known| known.id == id)
                    {
                        Some(position) => self.failed_directories[position] = failure,
                        None => self.failed_directories.push(failure),
                    }
                }
                self.failed_directories.truncate(FAILURE_LIMIT);
                if let Some(failure) = result.failure {
                    self.notice = Some(Notice {
                        text: format!("{}: {}", failure.kind, failure.reason),
                        level: NoticeLevel::Error,
                    });
                } else if !result.report.complete {
                    self.notice = Some(Notice {
                        text:
                            "The index is incomplete \u{2014} see Diagnostics for failure details."
                                .to_owned(),
                        level: NoticeLevel::Info,
                    });
                }
            }
            BackendEvent::Error(error) => {
                if !self.finished {
                    self.phase = "disconnected".to_owned();
                }
                self.notice = Some(Notice {
                    text: error,
                    level: NoticeLevel::Error,
                });
            }
        }
    }

    pub fn filtered_recent(&self) -> Vec<(&RecentDirectory, String)> {
        let filter = self.filter.to_lowercase();
        self.recent
            .iter()
            .filter_map(|directory| {
                let path = self.full_path(directory.id);
                (filter.is_empty() || path.to_lowercase().contains(&filter))
                    .then_some((directory, path))
            })
            .collect()
    }

    /// Roots every visualization at `id` and clears the selection.
    pub fn focus_directory(&mut self, id: u32) {
        self.focus = id;
        self.selected = None;
    }

    /// The inspector's subject: the selection, falling back to the focus.
    pub fn inspected(&self) -> u32 {
        self.selected.unwrap_or(self.focus)
    }

    /// Children of `id` with accumulated bytes, largest first. Ordering is
    /// maintained once per ingested backend batch by [`SizeTree::finish_batch`].
    pub fn children_by_size(&self, id: u32) -> Vec<u32> {
        self.size_tree
            .children(id)
            .iter()
            .copied()
            .filter(|&child| self.size_tree.subtree_bytes(child) > 0)
            .collect()
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
    }

    /// Emits at most one averaged sample per [`RATE_SAMPLE_MS`] window.
    ///
    /// Updates arrive on every backend tick while completions are flowing —
    /// far faster than the progress interval — so sampling per update would
    /// scroll the graph through its whole window in under a second.
    fn record_rate(&mut self, elapsed_ms: f64, entries: u64, queue: u64, native_bytes: u64) {
        let Some((window_start_ms, window_start_entries)) = self.last_rate_sample else {
            self.last_rate_sample = Some((elapsed_ms, entries));
            return;
        };

        let delta_ms = elapsed_ms - window_start_ms;
        if delta_ms < RATE_SAMPLE_MS {
            return;
        }
        let delta_entries = entries.saturating_sub(window_start_entries);
        let rate = (delta_entries as f64 / (delta_ms / 1_000.0)).max(0.0) as u64;
        self.throughput_history.push_front(rate);
        self.throughput_history.truncate(THROUGHPUT_HISTORY);
        self.queue_history.push_front(queue);
        self.queue_history.truncate(THROUGHPUT_HISTORY);
        self.memory_history.push_front(native_bytes);
        self.memory_history.truncate(THROUGHPUT_HISTORY);
        self.last_rate_sample = Some((elapsed_ms, entries));
    }

    fn add_directory(&mut self, directory: DirectorySummary) {
        if let Some(parent_id) = directory.parent_id {
            self.size_tree.link(directory.id, parent_id);
        }
        self.size_tree
            .set_name(directory.id, directory.name.clone());
        self.size_tree.set_own(
            directory.id,
            directory.allocated_bytes,
            directory.entries.saturating_sub(directory.children),
        );
        self.size_tree
            .add_bytes(directory.id, directory.allocated_bytes);
        if directory.state == DirectoryState::Failed
            && self.failed_directories.len() < FAILURE_LIMIT
            && !self
                .failed_directories
                .iter()
                .any(|failure| failure.id == directory.id)
        {
            self.failed_directories.push(FailedDirectory {
                id: directory.id,
                path: self.full_path(directory.id),
                phase: String::new(),
                category: String::new(),
                reason: directory.error.clone(),
            });
        }

        let mut directory_path = None;
        for file in &directory.largest_files {
            if !top_candidate(&self.top_files, file.size) {
                // Per-directory candidates are size-descending, so no later
                // entry can qualify once this one misses the global cutoff.
                break;
            }
            let path = directory_path.get_or_insert_with(|| self.full_path(directory.id));
            merge_top(
                &mut self.top_files,
                SizedEntry {
                    path: join_path(path, &file.name),
                    bytes: file.size,
                },
            );
        }

        if self.recent_limit > 0 {
            self.recent.push_front(RecentDirectory {
                id: directory.id,
                state: directory_state_text(directory.state).to_owned(),
                entries: directory.entries,
                children: directory.children,
                error: directory.error,
            });
            self.recent.truncate(self.recent_limit);
        }
    }

    /// Root-first ancestor chain for a directory, ending with itself.
    fn ancestor_chain(&self, id: u32) -> Vec<u32> {
        let mut chain = vec![id];
        let mut current = id;
        while let Some(parent) = self.size_tree.parent(current) {
            if chain.len() >= MAX_DEPTH || chain.contains(&parent) {
                break;
            }
            chain.push(parent);
            current = parent;
        }
        chain.reverse();
        chain
    }

    /// Breadcrumb segments for a directory, root-first, ending with the
    /// directory itself. Each segment is `(id, display name)`.
    pub fn ancestors(&self, id: u32) -> Vec<(u32, String)> {
        self.ancestor_chain(id)
            .into_iter()
            .map(|segment| (segment, self.directory_name(segment)))
            .collect()
    }

    /// Exact full path, reconstructed from the size tree's names and
    /// parent links; unknown segments render as `#id`.
    pub fn full_path(&self, id: u32) -> String {
        let mut path = String::new();
        for (position, segment) in self.ancestor_chain(id).into_iter().enumerate() {
            let name = self.directory_name(segment);
            if position == 0 {
                path = name;
            } else {
                path = join_path(&path, &name);
            }
        }
        path
    }

    /// A short display name: the final path component even for the scan
    /// root, whose stored name is its full path.
    pub fn display_name(&self, id: u32) -> String {
        let name = self.directory_name(id);
        name.rsplit('/')
            .next()
            .filter(|tail| !tail.is_empty())
            .unwrap_or(&name)
            .to_owned()
    }

    /// A directory's own name — the root stores its full path — or `#id`
    /// for a directory the app has not seen yet.
    pub fn directory_name(&self, id: u32) -> String {
        self.size_tree
            .name(id)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("#{id}"))
    }
}

/// Inserts into a size-descending leaderboard bounded at [`TOP_LIMIT`].
fn merge_top(list: &mut Vec<SizedEntry>, entry: SizedEntry) {
    if !top_candidate(list, entry.bytes) {
        return;
    }
    let position = list.partition_point(|existing| existing.bytes >= entry.bytes);
    list.insert(position, entry);
    list.truncate(TOP_LIMIT);
}

fn top_candidate(list: &[SizedEntry], bytes: u64) -> bool {
    list.len() < TOP_LIMIT || list.last().is_some_and(|smallest| bytes > smallest.bytes)
}

fn mount_policy_text(policy: MountPolicy) -> &'static str {
    match policy {
        MountPolicy::StayOnFilesystem => "stay_on_filesystem",
        MountPolicy::Cross => "cross",
    }
}

fn directory_state_text(state: DirectoryState) -> &'static str {
    match state {
        DirectoryState::Pending => "pending",
        DirectoryState::Published => "published",
        DirectoryState::Failed => "failed",
    }
}

fn binary_text(value: &EncodedBinary) -> String {
    match value {
        EncodedBinary::Utf8(value) => value.clone(),
        EncodedBinary::Base64 { base64 } => format!("<bytes:{base64}>"),
    }
}

fn join_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(id: u32, parent_id: Option<u32>, name: &str) -> DirectorySummary {
        DirectorySummary {
            id,
            parent_id,
            name: name.to_owned(),
            state: DirectoryState::Published,
            entries: 1,
            children: 0,
            error: String::new(),
            allocated_bytes: 0,
            largest_files: Vec::new(),
        }
    }

    #[test]
    fn reconstructs_recent_paths_from_parent_completions() {
        let mut app = ScanModel::new(4);
        app.add_directory(directory(0, None, "/tmp"));
        app.add_directory(directory(1, Some(0), "source"));
        app.add_directory(directory(2, Some(1), "lib"));

        assert_eq!(
            app.full_path(app.recent.front().unwrap().id),
            "/tmp/source/lib"
        );
    }

    #[test]
    fn path_and_recent_storage_are_bounded() {
        let mut app = ScanModel::new(2);
        app.add_directory(directory(0, None, "/tmp"));
        app.add_directory(directory(1, Some(0), "one"));
        app.add_directory(directory(2, Some(1), "two"));
        app.add_directory(directory(3, Some(0), "three"));

        assert_eq!(app.recent.len(), 2);
        // The feed is bounded, but paths stay exact: names live in the
        // size tree, not in an evictable cache.
        assert_eq!(app.full_path(app.recent.front().unwrap().id), "/tmp/three");
        assert_eq!(app.full_path(2), "/tmp/one/two");
    }

    #[test]
    fn filtering_changes_navigation_without_discarding_history() {
        let mut app = ScanModel::new(4);
        app.add_directory(directory(0, None, "/tmp"));
        app.add_directory(directory(1, Some(0), "source"));
        app.add_directory(directory(2, Some(1), "target"));

        app.set_filter("source".to_owned());
        assert_eq!(app.filtered_recent().len(), 2);
        assert_eq!(app.recent.len(), 3);
    }

    #[test]
    fn throughput_samples_use_a_fixed_time_base() {
        let mut app = ScanModel::new(4);
        app.record_rate(0.0, 0, 0, 0);

        for tick in 1..=24_u64 {
            app.record_rate(tick as f64 * 10.0, tick * 10, 5, 100);
        }
        assert!(app.throughput_history.is_empty());

        app.record_rate(250.0, 250, 7, 4_096);
        assert_eq!(app.throughput_history.len(), 1);
        assert_eq!(app.throughput_history.front(), Some(&1_000));
    }

    #[test]
    fn size_leaderboards_are_sorted_and_bounded() {
        let mut app = ScanModel::new(4);
        let mut root = directory(0, None, "/tmp");
        root.allocated_bytes = 10;
        root.largest_files = vec![crate::backend::LargestFile {
            name: "a.bin".to_owned(),
            size: 10,
        }];
        app.add_directory(root);

        for id in 1..300_u32 {
            let mut child = directory(id, Some(0), "child");
            child.allocated_bytes = u64::from(id);
            child.largest_files = vec![crate::backend::LargestFile {
                name: format!("file-{id}"),
                size: u64::from(id),
            }];
            app.add_directory(child);
        }

        assert_eq!(app.top_files.len(), TOP_LIMIT);
        assert_eq!(app.top_files.first().unwrap().bytes, 299);
        assert_eq!(app.top_files.first().unwrap().path, "/tmp/child/file-299");

        // Every child's bytes also propagated into the root's subtree.
        let children_total: u64 = (1..300_u64).sum();
        assert_eq!(app.size_tree.subtree_bytes(0), 10 + children_total);
        assert_eq!(app.size_tree.children(0).len(), 299);
    }

    #[test]
    fn subtree_sizes_propagate_to_every_ancestor() {
        let mut app = ScanModel::new(4);
        let mut root = directory(0, None, "/tmp");
        root.allocated_bytes = 10;
        app.add_directory(root);
        let mut middle = directory(1, Some(0), "middle");
        middle.allocated_bytes = 20;
        app.add_directory(middle);
        let mut leaf = directory(2, Some(1), "leaf");
        leaf.allocated_bytes = 30;
        app.add_directory(leaf);

        assert_eq!(app.size_tree.subtree_bytes(2), 30);
        assert_eq!(app.size_tree.subtree_bytes(1), 50);
        assert_eq!(app.size_tree.subtree_bytes(0), 60);
        // Own bytes stay distinct from accumulated subtree bytes.
        assert_eq!(app.size_tree.own_bytes(1), 20);
        assert_eq!(app.size_tree.own_entries(1), 1);
        assert_eq!(app.size_tree.children(0), [1]);
        assert_eq!(app.size_tree.parent(1), Some(0));
        assert_eq!(app.size_tree.parent(0), None);
        // Late-arriving bytes still reach the whole chain.
        app.size_tree.add_bytes(2, 5);
        assert_eq!(app.size_tree.subtree_bytes(0), 65);
    }

    #[test]
    fn ancestors_form_clickable_breadcrumbs() {
        let mut app = ScanModel::new(4);
        app.add_directory(directory(0, None, "/tmp"));
        app.add_directory(directory(1, Some(0), "source"));
        app.add_directory(directory(2, Some(1), "lib"));

        let chain = app.ancestors(2);
        assert_eq!(
            chain,
            vec![
                (0, "/tmp".to_owned()),
                (1, "source".to_owned()),
                (2, "lib".to_owned()),
            ]
        );
        // A directory with no cached parent is its own chain.
        assert_eq!(app.ancestors(9).len(), 1);
    }

    #[test]
    fn children_sort_by_subtree_size_and_focus_clears_selection() {
        let mut app = ScanModel::new(4);
        app.add_directory(directory(0, None, "/tmp"));
        let mut small = directory(1, Some(0), "small");
        small.allocated_bytes = 5;
        app.add_directory(small);
        let mut big = directory(2, Some(0), "big");
        big.allocated_bytes = 50;
        app.add_directory(big);
        let empty = directory(3, Some(0), "empty");
        app.add_directory(empty);
        app.size_tree.finish_batch();

        assert_eq!(app.children_by_size(0), [2, 1]);

        app.selected = Some(2);
        assert_eq!(app.inspected(), 2);
        app.focus_directory(2);
        assert_eq!(app.focus, 2);
        assert_eq!(app.selected, None);
        assert_eq!(app.inspected(), 2);
    }

    #[test]
    fn failed_directories_are_recorded_live_and_deduplicated() {
        let mut app = ScanModel::new(4);
        let mut failed = directory(7, None, "/tmp/locked");
        failed.state = DirectoryState::Failed;
        failed.error = "eacces".to_owned();
        app.add_directory(failed.clone());
        app.add_directory(failed);

        assert_eq!(app.failed_directories.len(), 1);
        assert_eq!(app.failed_directories[0].reason, "eacces");
    }
}
