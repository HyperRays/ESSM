# Findex

Findex is a macOS filesystem indexer built around `getattrlistbulk(2)`. 


Recursive traversal first attempts a naive path open for speed,
then falls back on `ENAMETOOLONG` or a symlinked path component to
descriptor relative resolution through the native store's parent/name links.
It therefore continues through valid trees whose printable paths exceed macOS
`PATH_MAX`, where conventional tools such as `du` stop, without following a
directory replaced by a symlink during traversal.

## Index a tree

```elixir
{:ok, result} =
  Findex.Indexer.run("/some/directory",
    fields: [:type, :file_id, :data_size, :modified_at]
  )

IO.inspect(result.report)
IO.inspect(Findex.Store.stats(result.store))
```

`result.report.complete?` is true only when every scheduled directory was
published and no entry returned a metadata error. Errors return `{:error, %Findex.Indexer.Failure{}}` with the partial tree and
report attached.

The default `mount_policy: :stay_on_filesystem` skips mount points and automount
triggers. This avoids double traversal of macOS's APFS Data volume.


Traversal policies are implemented directly in Elixir and are passed to the scheduler. `:default` is the unbiased native
order, `:name_biased` delays generated and dependency trees, and `:macos`
prioritizes some common space consuming directories on macos:

```elixir
Findex.Indexer.run("/", ranking: :macos)
```

For live rank changes, start the coordinator asynchronously:

```elixir
rank = fn task, read -> {read.(:direction) * task.depth, -task.id} end

{:ok, indexer} =
  Findex.Indexer.start_link("/some/directory",
    rank: rank,
    rank_data: %{direction: 1}
  )

:ok = Findex.Indexer.put_rank_data(indexer, :direction, -1)
{:ok, result} = Findex.Indexer.await(indexer)
```

## Read while indexing

The native store is readable while traversal is still running. Each reader
keeps its own integer completion cursor and pulls bounded pages from the
append-only completion journal:

```elixir
{:ok, indexer} = Findex.Indexer.start_link("/some/directory", fields: [:type])
store = Findex.Indexer.store(indexer)

{:ok, directory_ids, next_cursor} =
  Findex.Store.completed_since(store, 0, limit: 256)

nodes =
  Enum.map(directory_ids, fn directory_id ->
    {:ok, node} = Findex.Store.fetch_directory(store, directory_id)
    node
  end)

{:ok, result} = Findex.Indexer.await(indexer)
```

## Enumerate one directory

`Findex.Directory` is the lower-level, non-recursive API:

```elixir
{:ok, cursor} =
  Findex.Directory.open("/some/directory",
    fields: [:type, :filesystem_id, :file_id, :data_size],
    format: :packed
  )

try do
  Stream.repeatedly(fn -> Findex.Directory.next_batch(cursor) end)
  |> Enum.reduce_while(:ok, fn
    {:ok, batch}, :ok ->
      IO.inspect(Findex.Batch.type_counts(batch))
      {:cont, :ok}

    :done, :ok ->
      {:halt, :ok}

    {:error, reason}, :ok ->
      raise "enumeration failed: #{inspect(reason)}"
  end)
after
  Findex.Directory.close(cursor)
end
```

Use `fields: :fast`, `fields: :full`, or one explicit list. `Findex.Batch.value/3` decodes one
value, `to_entries/1` performs the optional allocation heavy conversion.

## Build and verify

```sh
mix compile
mix test
(cd rust_client/backend && mix compile && mix test)
cargo test --manifest-path rust_client/Cargo.toml
cargo test --manifest-path tui/Cargo.toml
cargo test --manifest-path desktop/Cargo.toml
make -C native analyze
make -C native sanitize
make -C native sanitize-thread
```

## Benchmarks

```sh
mix run bench/directory_benchmark.exs -- findex-packed-store-concurrent /path/to/tree 8
mix run bench/directory_benchmark.exs -- findex-packed-store-name-ranked /path/to/tree 8
make -C native benchmark
./bench/readdir_baseline /path/to/tree
cargo bench --manifest-path rust_client/Cargo.toml --bench findex_client
```
