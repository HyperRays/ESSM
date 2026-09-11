use findex_client::*;
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TestTree(PathBuf);

impl TestTree {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before Unix epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("findex-rust-client-{}-{nonce}", std::process::id()));
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
