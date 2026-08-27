use std::env;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use criterion::{Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use findex_client::{Client, Ranking, ScanOptions, development_command};

const ROOT_DIRECTORY_ID: u32 = 0;
const MAXIMUM_PAGE_SIZE: u32 = 256;

fn benchmark_findex(criterion: &mut Criterion) {
    match env::var("FINDEX_BENCH_SUITE").as_deref() {
        Ok("startup") => benchmark_startup(criterion),
        Ok("traversal") => benchmark_traversal(criterion),
        Ok("ranking") => benchmark_ranked_traversal(criterion),
        Ok("reads") => benchmark_retained_reads(criterion),
        Ok("all") | Err(env::VarError::NotPresent) => {
            benchmark_startup(criterion);
            benchmark_traversal(criterion);
            benchmark_ranked_traversal(criterion);
            benchmark_retained_reads(criterion);
        }
        Ok(suite) => panic!(
            "invalid FINDEX_BENCH_SUITE={suite:?}; expected startup, traversal, ranking, reads, or all"
        ),
        Err(error) => panic!("could not read FINDEX_BENCH_SUITE: {error}"),
    }
}

fn benchmark_ranked_traversal(criterion: &mut Criterion) {
    let project_root = project_root();
    let target = BenchmarkTarget::load();
    let mut options = scan_options();
    options.ranking = Ranking::Macos;
    let mut client =
        Client::spawn(development_command(&project_root)).expect("start the Findex BEAM backend");

    let probe = client
        .scan(&target.root, &options)
        .expect("probe the in-process ranked traversal");
    let entry_count = probe.report.entries;
    eprintln!(
        "Criterion in-process ranked target: {} ({} entries)",
        target.root.display(),
        entry_count
    );

    let mut group = criterion.benchmark_group("rust_client/ranked_traversal");
    group.sample_size(sample_size(20));
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(entry_count));

    group.bench_function(&target.label, |bencher| {
        bencher.iter(|| {
            let result = client
                .scan(black_box(&target.root), black_box(&options))
                .expect("run the in-process ranked traversal");
            black_box(result.report.entries)
        });
    });

    group.finish();
    client.shutdown().expect("shut down the Findex backend");
}

fn benchmark_startup(criterion: &mut Criterion) {
    let project_root = project_root();
    let mut group = criterion.benchmark_group("rust_client/startup");
    group.sample_size(sample_size(10));
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("spawn_handshake_shutdown", |bencher| {
        bencher.iter(|| {
            let client = Client::spawn(development_command(black_box(&project_root)))
                .expect("start the Findex BEAM backend");
            black_box(client.beam_pid());
            client.shutdown().expect("shut down the Findex backend");
        });
    });

    group.finish();
}

fn benchmark_traversal(criterion: &mut Criterion) {
    let project_root = project_root();
    let target = BenchmarkTarget::load();
    let options = scan_options();
    let mut client =
        Client::spawn(development_command(&project_root)).expect("start the Findex BEAM backend");

    let probe = client
        .scan(&target.root, &options)
        .expect("probe the traversal target");
    let entry_count = probe.report.entries;
    eprintln!(
        "Criterion traversal target: {} ({} entries)",
        target.root.display(),
        entry_count
    );

    let mut group = criterion.benchmark_group("rust_client/traversal");
    group.sample_size(sample_size(20));
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(entry_count));

    group.bench_function(&target.label, |bencher| {
        bencher.iter(|| {
            let result = client
                .scan(black_box(&target.root), black_box(&options))
                .expect("scan the traversal target");
            black_box(result.report.entries)
        });
    });

    group.finish();
    client.shutdown().expect("shut down the Findex backend");
}

fn benchmark_retained_reads(criterion: &mut Criterion) {
    let project_root = project_root();
    let target = BenchmarkTarget::load();
    let options = scan_options();
    let mut client =
        Client::spawn(development_command(&project_root)).expect("start the Findex BEAM backend");
    let index = client
        .start_scan(&target.root, &options)
        .expect("start the retained benchmark scan");
    let result = client
        .await_scan(index.index_id)
        .expect("finish the retained benchmark scan");

    let status = client
        .index_status(index.index_id)
        .expect("read retained index status");
    let completion_limit = bounded_page_size(status.store.completion_count);
    let root = client
        .fetch_directory(index.index_id, ROOT_DIRECTORY_ID, 0, 1)
        .expect("read retained root directory");
    let directory_limit = bounded_page_size(root.entry_count);

    eprintln!(
        "Criterion retained-read target: {} ({} entries, {} completed directories)",
        target.root.display(),
        result.report.entries,
        status.store.completion_count
    );

    let mut group = criterion.benchmark_group("rust_client/retained_reads");
    group.sample_size(sample_size(50));
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(8));

    group.bench_function("ping", |bencher| {
        bencher.iter(|| black_box(client.ping().expect("ping the Findex backend")));
    });

    group.bench_function("index_status", |bencher| {
        bencher.iter(|| {
            black_box(
                client
                    .index_status(index.index_id)
                    .expect("read retained index status"),
            )
        });
    });

    group.throughput(Throughput::Elements(u64::from(completion_limit)));
    group.bench_function("completed_directories", |bencher| {
        bencher.iter(|| {
            black_box(
                client
                    .completed_directories(index.index_id, 0, completion_limit)
                    .expect("read the completion journal"),
            )
        });
    });

    group.throughput(Throughput::Elements(u64::from(directory_limit)));
    group.bench_function("fetch_root_directory", |bencher| {
        bencher.iter(|| {
            black_box(
                client
                    .fetch_directory(index.index_id, ROOT_DIRECTORY_ID, 0, directory_limit)
                    .expect("read a retained directory page"),
            )
        });
    });

    group.finish();
    client
        .release_index(index.index_id)
        .expect("release the retained benchmark index");
    client.shutdown().expect("shut down the Findex backend");
}

fn project_root() -> PathBuf {
    env::var_os("FINDEX_PROJECT_ROOT").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("rust_client must be inside the shared workspace")
                .to_owned()
        },
        PathBuf::from,
    )
}

fn scan_options() -> ScanOptions {
    ScanOptions {
        fields: vec![
            "type".to_owned(),
            "file_id".to_owned(),
            "data_size".to_owned(),
            "modified_at".to_owned(),
        ],
        concurrency: env_u32("FINDEX_BENCH_CONCURRENCY"),
        ..ScanOptions::default()
    }
}

fn sample_size(default: usize) -> usize {
    env::var("FINDEX_BENCH_SAMPLE_SIZE").map_or(default, |value| {
        value
            .parse::<usize>()
            .ok()
            .filter(|samples| *samples >= 10)
            .unwrap_or_else(|| panic!("FINDEX_BENCH_SAMPLE_SIZE must be an integer of at least 10"))
    })
}

fn env_u32(name: &str) -> Option<u32> {
    env::var(name).ok().map(|value| {
        value
            .parse::<u32>()
            .ok()
            .filter(|number| *number > 0)
            .unwrap_or_else(|| panic!("{name} must be a positive integer"))
    })
}

fn bounded_page_size(count: u64) -> u32 {
    count.clamp(1, u64::from(MAXIMUM_PAGE_SIZE)) as u32
}

struct BenchmarkTarget {
    root: PathBuf,
    label: String,
    _fixture: Option<Fixture>,
}

impl BenchmarkTarget {
    fn load() -> Self {
        match env::var_os("FINDEX_BENCH_ROOT") {
            Some(root) => {
                let root = PathBuf::from(root);
                assert!(root.is_dir(), "FINDEX_BENCH_ROOT must name a directory");
                Self {
                    root,
                    label: "configured_root".to_owned(),
                    _fixture: None,
                }
            }
            None => {
                let fixture = Fixture::create();
                Self {
                    root: fixture.root.clone(),
                    label: "generated_fixture".to_owned(),
                    _fixture: Some(fixture),
                }
            }
        }
    }
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn create() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!("findex-rust-criterion-{}-{nonce}", process::id()));
        fs::create_dir(&root).expect("create Criterion fixture root");

        for directory_number in 0..128 {
            let directory = root.join(format!("directory-{directory_number:03}"));
            fs::create_dir(&directory).expect("create Criterion fixture directory");
            fs::write(
                root.join(format!("root-file-{directory_number:03}.json")),
                b"{\"benchmark\":true}\n",
            )
            .expect("write Criterion root fixture file");

            for file_number in 0..8 {
                fs::write(
                    directory.join(format!("file-{file_number:02}.yaml")),
                    b"benchmark: true\n",
                )
                .expect("write Criterion nested fixture file");
            }
        }

        Self { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.root) {
            eprintln!(
                "could not remove Criterion fixture {}: {error}",
                self.root.display()
            );
        }
    }
}

criterion_group!(benches, benchmark_findex);
criterion_main!(benches);
