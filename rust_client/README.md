# Findex Rust client

`findex-client` is the minimal process boundary for embedding a long-lived
Findex BEAM instance in a Rust application. It launches one child, validates a
protocol-version handshake, runs any number of targeted traversals in that
same VM, and shuts the child down cleanly.

The protocol is private framed binary data over the child's stdin and stdout.
Each frame has a fixed magic/version/length header and an Erlang external-term
payload. It opens no network listener and does not use Erlang distribution.
Backend stderr remains attached to the Rust application's stderr.

## Development use

Keep `findex/` and `rust_client/` as sibling directories under one workspace
root. Compile the companion backend first; its path dependency compiles core
Findex and the native library as part of the same build:

```sh
cd /path/to/workspace
cd rust_client/backend
mix compile
mix test
cd ../..
cargo test --manifest-path rust_client/Cargo.toml
```

Then retain and inspect an index from Rust:

```rust,no_run
use findex_client::{Client, ScanOptions, development_command};

# fn run() -> Result<(), findex_client::Error> {
let workspace_root = "/path/to/workspace";
let mut client = Client::spawn(development_command(workspace_root))?;

let index = client.start_scan(
    "/Users/me/Documents",
    &ScanOptions::default(),
)?;

// These reads are valid while traversal is still publishing directories.
let status = client.index_status(index.index_id)?;
let completions = client.completed_directories(index.index_id, 0, 256)?;

for directory_id in completions.directory_ids.iter().copied() {
    let page = client.fetch_directory(index.index_id, directory_id, 0, 256)?;
    println!("directory {} has {} entries", directory_id, page.entry_count);
}

// Aggregates for many directories in one round trip: byte totals, the
// largest regular files, and a log2 size histogram, computed inside the
// BEAM from the packed blocks. Use this instead of paging every entry
// when only summaries are needed. The size field must be retained by
// the scan (`ScanOptions::fields`).
let stats = client.summarize_directories(
    index.index_id,
    &completions.directory_ids,
    "data_size",
    8,
)?;
for directory in &stats {
    println!("directory {} holds {} bytes", directory.directory_id, directory.size_bytes);
}

let result = client.await_scan(index.index_id)?;
println!("{} entries", result.report.entries);

// The native store remains queryable until this explicit release.
client.release_index(index.index_id)?;

client.shutdown()?;
# Ok(())
# }
```

For a packaged application, assemble an OTP release from `rust_client/backend`
that starts `FindexRust.Bridge`, bundle it with the Rust application, and pass
its foreground command to `Client::spawn`. `development_command` exists only
for working from a source checkout.

## Current boundary

Protocol version 6 supports:

- readiness and `ping` checks;
- asynchronous `start_scan` returning an independent index handle;
- live status, counters, queue state, and native memory measurements;
- selection of built-in ranking policies implemented by the Elixir scheduler;
- independently cursor-paged completion journals;
- row-paged immutable directory blocks with every requested metadata field;
- pushed terminal reports, with `await_scan` consuming the completion event;
- explicit cancellation/release of running or completed indexes;
- repeated convenience `scan` calls that await and release automatically;
- field, concurrency, buffer, mount, and failure-sample options;
- graceful shutdown, with forced child cleanup if the Rust client is dropped.

Directory rows expose their selected fields in a string-keyed
`BTreeMap<String, serde_json::Value>`, so adding a Findex metadata field does
not require another Rust struct layout. This is only the Rust API's dynamic
value representation: the wire carries raw binaries without JSON or base64.
At the public API boundary, valid UTF-8 becomes a string and arbitrary bytes
use `{ "base64": "..." }`.

Each response is bounded to one completion or directory page. A retained index
continues consuming its reported `native_bytes` until `release_index`, bridge
shutdown, stdin EOF, or termination of the Rust-owned child process.

## Ranking

Rust selects a named policy; Findex evaluates it in the Elixir coordinator and
puts each discovered directory straight into the scheduler.

```rust,no_run
use findex_client::{Client, Ranking, ScanOptions, development_command};

# fn run() -> Result<(), findex_client::Error> {
let mut client = Client::spawn(development_command("/path/to/workspace"))?;
let mut options = ScanOptions::default();
options.ranking = Ranking::Macos;
let result = client.scan("/", &options)?;
client.shutdown()?;
# println!("{}", result.report.entries);
# Ok(())
# }
```

`Ranking::Default` preserves the unbiased native order. `NameBiased`
deprioritizes generated, dependency, cache, configuration, and repository
subtrees. `Macos` uses bounded path priors tuned for likely large consumers in
macOS root, home-library, developer, package-manager, cache, and application
data trees. These are traversal-order policies only; every reachable directory
is still indexed and the final store is identical apart from live completion
order.

Elixir callers that need runtime feedback can pass a custom `rank` function and
`rank_data` directly to `Findex.Indexer`. That intentionally remains an
in-process API rather than a Rust bridge feature.

## Criterion benchmarks

Compile the backend, then run the Rust-side benchmarks:

```sh
(cd rust_client/backend && mix compile)
cargo bench --manifest-path rust_client/Cargo.toml --bench findex_client
```

The target contains four suites: BEAM spawn/handshake/shutdown, repeated
default traversal, macOS-ranked traversal, and framed binary reads from a
completed retained index.
It creates a small temporary tree by default. Environment variables select a
suite or a real traversal target:

```sh
FINDEX_BENCH_SUITE=traversal \
FINDEX_BENCH_ROOT="$HOME/Documents/programming" \
FINDEX_BENCH_CONCURRENCY=8 \
cargo bench --manifest-path rust_client/Cargo.toml --bench findex_client
```

`FINDEX_BENCH_SUITE` accepts `startup`, `traversal`, `ranking`, `reads`, or `all`.
`FINDEX_BENCH_SAMPLE_SIZE` overrides Criterion's suite-specific sample count
and must be at least 10. Real trees are scanned once as an untimed probe and
then at least 10 times, so use a focused target for routine measurements.
