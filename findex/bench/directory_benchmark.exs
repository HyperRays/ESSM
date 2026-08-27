defmodule Findex.DirectoryBenchmark do
  @moduledoc false

  import Bitwise

  alias Findex.{Batch, Directory, Indexer, Scheduler, Store}

  @checksum_mask 0xFFFFFFFFFFFFFFFF
  defmodule Stats do
    @moduledoc false

    defstruct entries: 0,
              directories: 0,
              regular_files: 0,
              symlinks: 0,
              other: 0,
              logical_bytes: 0,
              directory_errors: 0,
              metadata_errors: 0,
              checksum: 0
  end

  def run(mode, root, concurrency \\ System.schedulers_online()) do
    root = Path.expand(root)
    :erlang.garbage_collect()
    memory_before = :erlang.memory()
    started_at = System.monotonic_time()

    {stats, store} =
      case mode do
        "findex" ->
          {scan_findex([root], %Stats{}, mode, :fast), nil}

        "findex-full" ->
          {scan_findex([root], %Stats{}, mode, :full), nil}

        "findex-minimal" ->
          {scan_findex([root], %Stats{}, mode, [:type]), nil}

        "findex-packed" ->
          {scan_findex_packed([root], %Stats{}, mode), nil}

        "findex-concurrent" ->
          {scan_findex_concurrent([root], %Stats{}, mode, [:type], concurrency), nil}

        "findex-packed-concurrent" ->
          {scan_findex_packed_concurrent([root], %Stats{}, mode, concurrency), nil}

        "findex-packed-store-concurrent" ->
          scan_findex_packed_store(root, concurrency, [])

        "findex-packed-store-name-ranked" ->
          scan_findex_packed_store(root, concurrency, ranking: :name_biased)

        "findex-size" ->
          {scan_findex(
             [root],
             %Stats{},
             mode,
             [:type, :file_id, :data_size, :modified_at]
           ), nil}

        "elixir" ->
          {scan_elixir([root], %Stats{}, mode), nil}
      end

    elapsed_native = System.monotonic_time() - started_at
    elapsed_ms = System.convert_time_unit(elapsed_native, :native, :microsecond) / 1_000
    throughput = if elapsed_ms > 0, do: stats.entries / elapsed_ms * 1_000, else: 0.0
    memory_after = :erlang.memory()

    concurrency_result =
      if mode in [
           "findex-concurrent",
           "findex-packed-concurrent",
           "findex-packed-store-concurrent",
           "findex-packed-store-name-ranked"
         ],
         do: " concurrency=#{concurrency}",
         else: ""

    storage_result =
      case store do
        %Store{} ->
          usage = Store.memory_usage(store)

          " stored_entries=#{usage.entry_count}" <>
            " stored_directories=#{usage.directory_count}" <>
            " published_directories=#{usage.published_directory_count}" <>
            " failed_directories=#{usage.failed_directory_count}" <>
            " pending_directories=#{usage.pending_directory_count}" <>
            " completed_directories=#{usage.completion_count}" <>
            " store_native_bytes=#{usage.native_bytes}" <>
            " store_block_bytes=#{usage.block_bytes}" <>
            " store_payload_bytes=#{usage.payload_bytes}" <>
            " store_directory_table_bytes=#{usage.directory_table_bytes}" <>
            " store_completion_journal_bytes=#{usage.completion_journal_bytes}"

        nil ->
          ""
      end

    memory_result =
      " memory_total_bytes=#{memory_after[:total]}" <>
        " memory_total_delta_bytes=#{memory_after[:total] - memory_before[:total]}" <>
        " memory_processes_bytes=#{memory_after[:processes]}" <>
        " memory_binary_bytes=#{memory_after[:binary]}" <>
        " memory_ets_bytes=#{memory_after[:ets]}"

    IO.puts(
      "RESULT mode=#{mode}#{concurrency_result} elapsed_ms=#{Float.round(elapsed_ms, 3)} " <>
        "entries_per_second=#{Float.round(throughput, 1)} " <>
        "entries=#{stats.entries} directories=#{stats.directories} " <>
        "regular_files=#{stats.regular_files} symlinks=#{stats.symlinks} " <>
        "other=#{stats.other} logical_bytes=#{stats.logical_bytes} " <>
        "directory_errors=#{stats.directory_errors} " <>
        "metadata_errors=#{stats.metadata_errors} checksum=#{stats.checksum}" <>
        memory_result <> storage_result
    )

    :ok
  end

  defp scan_findex_packed_store(root, concurrency, ranking_options) do
    options =
      [
        fields: [:type],
        concurrency: concurrency,
        mount_policy: :cross
      ] ++ ranking_options

    case Indexer.run(root, options) do
      {:ok, %{store: store, report: report}} ->
        stats = %Stats{
          entries: report.entries,
          directories: report.directories,
          regular_files: report.regular_files,
          symlinks: report.symlinks,
          other: report.other,
          directory_errors: report.store.failed_directory_count,
          metadata_errors: report.metadata_errors
        }

        {stats, store}

      {:error, failure} ->
        raise "production indexer aborted: #{inspect(failure)}"
    end
  end

  defp scan_findex([], stats, _mode, _fields), do: stats

  defp scan_findex([directory | remaining], stats, mode, fields) do
    case Directory.open(directory, fields: fields) do
      {:ok, cursor} ->
        {discovered, stats} =
          try do
            read_findex_batches(cursor, directory, [], stats)
          after
            Directory.close(cursor)
          end

        scan_findex(Enum.reverse(discovered, remaining), stats, mode, fields)

      {:error, _reason} ->
        scan_findex(
          remaining,
          %{stats | directory_errors: stats.directory_errors + 1},
          mode,
          fields
        )
    end
  end

  defp scan_findex_concurrent(directories, stats, mode, fields, concurrency) do
    scan_concurrently(directories, stats, mode, concurrency, fn directory ->
      scan_findex_directory(directory, mode, fields)
    end)
  end

  defp scan_findex_directory(directory, _mode, fields) do
    case Directory.open(directory, fields: fields) do
      {:ok, cursor} ->
        try do
          read_findex_batches(cursor, directory, [], %Stats{})
        after
          Directory.close(cursor)
        end

      {:error, _reason} ->
        {[], %Stats{directory_errors: 1}}
    end
  end

  defp merge_stats(stats, addition) do
    %{
      stats
      | entries: stats.entries + addition.entries,
        directories: stats.directories + addition.directories,
        regular_files: stats.regular_files + addition.regular_files,
        symlinks: stats.symlinks + addition.symlinks,
        other: stats.other + addition.other,
        logical_bytes: stats.logical_bytes + addition.logical_bytes,
        directory_errors: stats.directory_errors + addition.directory_errors,
        metadata_errors: stats.metadata_errors + addition.metadata_errors,
        checksum: band(stats.checksum + addition.checksum, @checksum_mask)
    }
  end

  defp scan_findex_packed([], stats, _mode), do: stats

  defp scan_findex_packed([directory | remaining], stats, mode) do
    {discovered, directory_stats} = scan_findex_packed_directory(directory, mode)
    stats = merge_stats(stats, directory_stats)
    scan_findex_packed(Enum.reverse(discovered, remaining), stats, mode)
  end

  defp scan_findex_packed_concurrent(directories, stats, mode, concurrency) do
    scan_concurrently(directories, stats, mode, concurrency, fn directory ->
      scan_findex_packed_directory(directory, mode)
    end)
  end

  defp scan_concurrently(directories, stats, mode, concurrency, worker) do
    scheduler = Scheduler.new(rank: fn _directory, _read -> 0 end)
    {:ok, task_supervisor} = Task.Supervisor.start_link()

    try do
      scheduler = enqueue_directories(scheduler, directories)

      {stats, _scheduler} =
        scan_scheduled(scheduler, task_supervisor, %{}, stats, mode, concurrency, worker)

      stats
    after
      if Process.alive?(task_supervisor), do: Supervisor.stop(task_supervisor)
    end
  end

  defp scan_scheduled(
         scheduler,
         task_supervisor,
         in_flight,
         stats,
         mode,
         concurrency,
         worker
       ) do
    {scheduler, in_flight} =
      dispatch_scheduled(
        scheduler,
        task_supervisor,
        in_flight,
        concurrency,
        worker
      )

    if map_size(in_flight) == 0 do
      {stats, scheduler}
    else
      receive do
        {reference, {discovered, %Stats{} = directory_stats}}
        when is_map_key(in_flight, reference) ->
          Process.demonitor(reference, [:flush])
          in_flight = Map.delete(in_flight, reference)
          scheduler = enqueue_directories(scheduler, discovered)
          stats = merge_stats(stats, directory_stats)

          scan_scheduled(
            scheduler,
            task_supervisor,
            in_flight,
            stats,
            mode,
            concurrency,
            worker
          )

        {:DOWN, reference, :process, _pid, reason} when is_map_key(in_flight, reference) ->
          directory = Map.fetch!(in_flight, reference)

          IO.puts(
            :stderr,
            "#{mode}: worker for #{inspect(directory)} exited: #{inspect(reason)}"
          )

          scan_scheduled(
            scheduler,
            task_supervisor,
            Map.delete(in_flight, reference),
            %{stats | directory_errors: stats.directory_errors + 1},
            mode,
            concurrency,
            worker
          )
      end
    end
  end

  defp dispatch_scheduled(
         scheduler,
         _task_supervisor,
         in_flight,
         concurrency,
         _worker
       )
       when map_size(in_flight) >= concurrency,
       do: {scheduler, in_flight}

  defp dispatch_scheduled(scheduler, task_supervisor, in_flight, concurrency, worker) do
    available_slots = concurrency - map_size(in_flight)
    {scheduled, scheduler} = Scheduler.pop_many(scheduler, available_slots)

    in_flight =
      Enum.reduce(scheduled, in_flight, fn {_id, directory, _rank}, tasks ->
        task = Task.Supervisor.async_nolink(task_supervisor, fn -> worker.(directory) end)
        Map.put(tasks, task.ref, directory)
      end)

    {scheduler, in_flight}
  end

  defp enqueue_directories(scheduler, []), do: scheduler

  defp enqueue_directories(scheduler, directories) do
    tasks =
      Enum.map(directories, fn
        %{id: id} = directory -> {id, directory}
        directory -> {directory, directory}
      end)

    case Scheduler.put_tasks(scheduler, tasks) do
      {:ok, scheduler} -> scheduler
      {:error, reason} -> raise "could not rank discovered directories: #{inspect(reason)}"
    end
  end

  defp scan_findex_packed_directory(directory, _mode) do
    case Directory.open(directory, fields: [:type], format: :packed) do
      {:ok, cursor} ->
        try do
          read_findex_packed_batches(cursor, directory, [], %Stats{})
        after
          Directory.close(cursor)
        end

      {:error, _reason} ->
        {[], %Stats{directory_errors: 1}}
    end
  end

  defp read_findex_packed_batches(
         cursor,
         directory,
         discovered,
         stats
       ) do
    case Directory.next_batch(cursor) do
      {:ok, batch} ->
        counts = Batch.type_counts(batch)

        stats = %{
          stats
          | entries: stats.entries + batch.count,
            directories: stats.directories + counts.directories,
            regular_files: stats.regular_files + counts.regular_files,
            symlinks: stats.symlinks + counts.symlinks,
            other: stats.other + counts.other,
            metadata_errors: stats.metadata_errors + Batch.valid_count(batch, :error)
        }

        discovered =
          Enum.reduce(Batch.directory_names(batch), discovered, fn name, directories ->
            [Path.join(directory, name) | directories]
          end)

        read_findex_packed_batches(
          cursor,
          directory,
          discovered,
          stats
        )

      :done ->
        {discovered, stats}

      {:error, _reason} ->
        {discovered, %{stats | directory_errors: stats.directory_errors + 1}}
    end
  end

  defp read_findex_batches(cursor, directory, discovered, stats) do
    case Directory.next_batch(cursor) do
      {:ok, entries} ->
        {discovered, stats} =
          Enum.reduce(entries, {discovered, stats}, fn entry, {directories, accumulator} ->
            path = Path.join(directory, entry.name)

            accumulator =
              add_entry(
                accumulator,
                entry.type,
                entry.data_size,
                entry.file_id,
                entry.modified_at
              )

            accumulator =
              if is_nil(entry.error) do
                accumulator
              else
                %{accumulator | metadata_errors: accumulator.metadata_errors + 1}
              end

            if entry.type == :directory and is_nil(entry.error) do
              {[path | directories], accumulator}
            else
              {directories, accumulator}
            end
          end)

        read_findex_batches(cursor, directory, discovered, stats)

      :done ->
        {discovered, stats}

      {:error, _reason} ->
        {discovered, %{stats | directory_errors: stats.directory_errors + 1}}
    end
  end

  defp scan_elixir([], stats, _mode), do: stats

  defp scan_elixir([directory | remaining], stats, mode) do
    case File.ls(directory) do
      {:ok, names} ->
        {discovered, stats} =
          Enum.reduce(names, {[], stats}, fn name, {directories, accumulator} ->
            path = Path.join(directory, name)

            case File.lstat(path, time: :posix) do
              {:ok, file_stat} ->
                accumulator =
                  add_entry(
                    accumulator,
                    file_stat.type,
                    file_stat.size,
                    file_stat.inode,
                    file_stat.mtime
                  )

                if file_stat.type == :directory do
                  {[path | directories], accumulator}
                else
                  {directories, accumulator}
                end

              {:error, _reason} ->
                {directories,
                 %{
                   accumulator
                   | entries: accumulator.entries + 1,
                     metadata_errors: accumulator.metadata_errors + 1
                 }}
            end
          end)

        scan_elixir(Enum.reverse(discovered, remaining), stats, mode)

      {:error, _reason} ->
        scan_elixir(remaining, %{stats | directory_errors: stats.directory_errors + 1}, mode)
    end
  end

  defp add_entry(stats, type, size, file_id, modified_at) do
    size = if type == :regular and is_integer(size), do: size, else: 0
    file_id = if is_integer(file_id), do: file_id, else: 0

    modified_seconds =
      case modified_at do
        {seconds, _nanoseconds} when is_integer(seconds) -> seconds
        seconds when is_integer(seconds) -> seconds
        _other -> 0
      end

    stats = %{
      stats
      | entries: stats.entries + 1,
        logical_bytes: stats.logical_bytes + size,
        checksum:
          band(
            bxor(stats.checksum, file_id) + size + modified_seconds,
            @checksum_mask
          )
    }

    case type do
      :directory -> %{stats | directories: stats.directories + 1}
      :regular -> %{stats | regular_files: stats.regular_files + 1}
      :symlink -> %{stats | symlinks: stats.symlinks + 1}
      _other -> %{stats | other: stats.other + 1}
    end
  end
end

run_concurrent = fn mode, root, concurrency_string ->
  case Integer.parse(concurrency_string) do
    {concurrency, ""} when concurrency > 0 ->
      Findex.DirectoryBenchmark.run(mode, root, concurrency)

    _other ->
      IO.puts(:stderr, "CONCURRENCY must be a positive integer")
      System.halt(64)
  end
end

case System.argv() do
  ["--", "findex-concurrent", root, concurrency] ->
    run_concurrent.("findex-concurrent", root, concurrency)

  ["findex-concurrent", root, concurrency] ->
    run_concurrent.("findex-concurrent", root, concurrency)

  ["--", "findex-packed-concurrent", root, concurrency] ->
    run_concurrent.("findex-packed-concurrent", root, concurrency)

  ["findex-packed-concurrent", root, concurrency] ->
    run_concurrent.("findex-packed-concurrent", root, concurrency)

  ["--", "findex-packed-store-concurrent", root, concurrency] ->
    run_concurrent.("findex-packed-store-concurrent", root, concurrency)

  ["findex-packed-store-concurrent", root, concurrency] ->
    run_concurrent.("findex-packed-store-concurrent", root, concurrency)

  ["--", "findex-packed-store-name-ranked", root, concurrency] ->
    run_concurrent.("findex-packed-store-name-ranked", root, concurrency)

  ["findex-packed-store-name-ranked", root, concurrency] ->
    run_concurrent.("findex-packed-store-name-ranked", root, concurrency)

  ["--", mode, root]
  when mode in [
         "findex",
         "findex-full",
         "findex-minimal",
         "findex-packed",
         "findex-concurrent",
         "findex-packed-concurrent",
         "findex-packed-store-concurrent",
         "findex-packed-store-name-ranked",
         "findex-size",
         "elixir"
       ] ->
    Findex.DirectoryBenchmark.run(mode, root)

  [mode, root]
  when mode in [
         "findex",
         "findex-full",
         "findex-minimal",
         "findex-packed",
         "findex-concurrent",
         "findex-packed-concurrent",
         "findex-packed-store-concurrent",
         "findex-packed-store-name-ranked",
         "findex-size",
         "elixir"
       ] ->
    Findex.DirectoryBenchmark.run(mode, root)

  _other ->
    IO.puts(
      :stderr,
      "usage: mix run bench/directory_benchmark.exs -- MODE DIRECTORY [CONCURRENCY]\n" <>
        "modes: findex, findex-full, findex-minimal, findex-packed, " <>
        "findex-concurrent, findex-packed-concurrent, " <>
        "findex-packed-store-concurrent, findex-packed-store-name-ranked, " <>
        "findex-size, elixir"
    )

    System.halt(64)
end
