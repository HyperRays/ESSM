defmodule Findex.IndexerTest do
  use ExUnit.Case, async: true

  alias Findex.{Directory, Indexer, PosixError, Store}
  alias Findex.Indexer.{DirectoryFailure, Failure, Report, Result}
  alias Findex.Store.DirectoryNode

  setup do
    root =
      Path.join(
        System.tmp_dir!(),
        "findex-indexer-test-#{System.os_time(:nanosecond)}-#{System.unique_integer([:positive])}"
      )

    File.mkdir_p!(Path.join(root, "alpha/beta"))
    File.write!(Path.join(root, "root-file"), "root")
    File.write!(Path.join(root, "alpha/child-file"), "child")
    on_exit(fn -> File.rm_rf!(root) end)

    %{root: root}
  end

  test "publishes a complete concurrent tree and report", %{root: root} do
    assert {:ok, %Result{store: store, report: %Report{} = report}} =
             Indexer.run(root,
               fields: [:type, :file_id],
               concurrency: 2,
               mount_policy: :cross
             )

    assert report.complete?
    assert report.entries == 4
    assert report.directories == 2
    assert report.regular_files == 2
    assert report.metadata_errors == 0
    assert report.directory_failure_counts == %{}
    assert report.directory_failure_reasons == %{}
    assert report.directory_failure_samples == []
    assert report.store.directory_count == 3
    assert report.store.published_directory_count == 3
    assert report.store.failed_directory_count == 0
    assert report.store.pending_directory_count == 0

    assert {:ok, %DirectoryNode{state: :published, entries: entries}} =
             Store.fetch_directory(store, Store.root_id(store))

    assert entries.count == 2
  end

  test "retains an exact failed root instead of silently returning an empty index", %{root: root} do
    missing = Path.join(root, "does-not-exist")

    assert {:ok, %Result{store: store, report: report}} =
             Indexer.run(missing, mount_policy: :cross)

    refute report.complete?
    assert report.entries == 0
    assert report.directory_failure_counts == %{filesystem_changed: 1}
    assert report.directory_failure_reasons == %{enoent: 1}

    assert [%DirectoryFailure{phase: :open, reason: :enoent, category: :filesystem_changed}] =
             report.directory_failure_samples

    assert report.store.failed_directory_count == 1
    assert report.store.pending_directory_count == 0

    assert {:ok, %DirectoryNode{state: :failed, error: :enoent}} =
             Store.fetch_directory(store, Store.root_id(store))
  end

  test "accepts atomic live rank-data changes", %{root: root} do
    rank = fn task, read -> {read.(:direction) * task.depth, -task.id} end

    assert {:ok, indexer} =
             Indexer.start_link(root,
               concurrency: 1,
               mount_policy: :cross,
               rank: rank,
               rank_data: %{direction: 1}
             )

    assert :ok = Indexer.put_rank_data(indexer, :direction, -1)
    assert {:ok, %Result{report: %Report{complete?: true}}} = Indexer.await(indexer)
    GenServer.stop(indexer)
  end

  test "defaults to twice the dirty I/O scheduler count", %{root: root} do
    assert Indexer.default_concurrency() ==
             2 * :erlang.system_info(:dirty_io_schedulers)

    assert {:ok, indexer} =
             Indexer.start_link(root,
               mount_policy: :cross
             )

    state = :sys.get_state(indexer)
    assert state.config.concurrency == Indexer.default_concurrency()

    GenServer.stop(indexer)
  end

  test "an idle persistent worker crash aborts and stops the whole pool", %{root: root} do
    crash_root = Path.join(root, "worker-crash")
    File.mkdir_p!(crash_root)

    Enum.each(1..512, fn index ->
      File.mkdir!(Path.join(crash_root, Integer.to_string(index)))
    end)

    assert {:ok, indexer} =
             Indexer.start_link(crash_root,
               concurrency: 3,
               mount_policy: :cross
             )

    state = await_running_workers(indexer, 1_000)
    workers = Map.values(state.worker_references)
    Process.exit(hd(state.idle_workers), :kill)

    assert {:error, %Failure{kind: :worker, reason: :killed, task: nil}} =
             Indexer.await(indexer)

    refute Enum.any?(workers, &Process.alive?/1)
    GenServer.stop(indexer)
  end

  test "dispatches built-in name-biased ranks inside the scheduler", %{root: root} do
    ranked_root = Path.join(root, "ranked-order")
    File.mkdir_p!(Path.join(ranked_root, "target"))
    File.mkdir_p!(Path.join(ranked_root, "source"))

    assert {:ok, indexer} =
             Indexer.start_link(ranked_root,
               concurrency: 1,
               mount_policy: :cross,
               ranking: :name_biased
             )

    store = Indexer.store(indexer)
    assert {:ok, %Result{report: report}} = Indexer.await(indexer)

    assert {:ok, [0, source_id, target_id], 3} =
             Store.completed_since(store, 0, limit: report.store.completion_count)

    assert {:ok, source_path} = Store.path(store, source_id)
    assert {:ok, target_path} = Store.path(store, target_id)
    assert Path.basename(source_path) == "source"
    assert Path.basename(target_path) == "target"
    GenServer.stop(indexer)
  end

  test "exposes a live store to an independent completion consumer", %{root: root} do
    assert {:ok, indexer} =
             Indexer.start_link(root,
               fields: [:type, :file_id],
               concurrency: 1,
               mount_policy: :cross
             )

    store = Indexer.store(indexer)

    reader =
      Task.async(fn ->
        assert {:ok, [0], 1} = await_completions(store, 0, 100)

        assert {:ok, %DirectoryNode{state: :published, entries: entries}} =
                 Store.fetch_directory(store, 0)

        entries.count
      end)

    assert Task.await(reader) == 2
    assert {:ok, %Result{store: result_store, report: report}} = Indexer.await(indexer)
    assert result_store.resource == store.resource

    assert {:ok, completed_ids, cursor} =
             Store.completed_since(store, 0, limit: report.store.completion_count)

    assert cursor == report.store.directory_count
    assert MapSet.new(completed_ids) == MapSet.new(0..(report.store.directory_count - 1))
    GenServer.stop(indexer)
  end

  test "continues after a permission denial and proves the index is incomplete", %{root: root} do
    restricted = Path.join(root, "restricted")
    File.mkdir!(restricted)
    File.write!(Path.join(restricted, "hidden"), "hidden")
    File.chmod!(restricted, 0o000)

    try do
      assert {:ok, %Result{store: store, report: report}} =
               Indexer.run(root, concurrency: 2, mount_policy: :cross)

      refute report.complete?
      assert report.directory_failure_counts == %{access_denied: 1}
      assert report.directory_failure_reasons == %{eacces: 1}
      assert report.store.failed_directory_count == 1
      assert report.store.pending_directory_count == 0

      failed_id =
        Store.fetch_directory(store, Store.root_id(store))
        |> then(fn {:ok, root_node} -> root_node.children end)
        |> Enum.find_value(fn {_row, child_id} ->
          case Store.fetch_directory(store, child_id) do
            {:ok, %DirectoryNode{name: "restricted"}} -> child_id
            _other -> nil
          end
        end)

      assert {:ok, %DirectoryNode{state: :failed, error: :eacces}} =
               Store.fetch_directory(store, failed_id)
    after
      File.chmod!(restricted, 0o700)
    end
  end

  test "traverses directory trees whose display paths exceed PATH_MAX", %{root: root} do
    long_root = Path.join(root, "long-path-root")
    depth = 180
    File.mkdir!(long_root)
    create_deep_tree!(long_root, depth)

    deepest_path =
      Enum.reduce(1..depth, long_root, fn _index, path ->
        Path.join(path, "child_0")
      end)

    try do
      assert byte_size(deepest_path) > 1_024
      assert {:error, :enametoolong} = Directory.open(deepest_path, fields: [:type])

      assert {:ok, %Result{store: store, report: report}} =
               Indexer.run(long_root,
                 fields: [:type],
                 concurrency: 4,
                 mount_policy: :cross
               )

      assert report.complete?
      assert report.entries == depth + 1
      assert report.directories == depth
      assert report.regular_files == 1
      assert report.store.directory_count == depth + 1
      assert report.store.failed_directory_count == 0
      assert {:ok, ^deepest_path} = Store.path(store, depth)
    after
      remove_deep_tree!(long_root)
    end
  end

  test "uses descriptor traversal when the root path contains a symlink", %{root: root} do
    link = root <> "-link"
    File.ln_s!(root, link)
    on_exit(fn -> File.rm(link) end)

    assert {:error, :eloop} = Directory.open_traversal(link, fields: [:type])

    assert {:ok, %Result{store: store, report: report}} =
             Indexer.run(link,
               fields: [:type],
               concurrency: 2,
               mount_policy: :cross
             )

    assert report.complete?
    assert report.entries == 4
    assert report.directories == 2
    assert {:ok, ^link} = Store.path(store, Store.root_id(store))
  end

  test "turns a child ranking exception into a structured fatal result", %{root: root} do
    rank = fn task, _read ->
      if task.depth == 0, do: 0, else: raise("ranking failed")
    end

    assert {:error, %Failure{kind: :ranking, reason: {:rank_error, _id, _exception}} = failure} =
             Indexer.run(root, rank: rank, mount_policy: :cross)

    refute failure.report.complete?
    assert failure.report.store.published_directory_count == 1
    assert failure.report.store.pending_directory_count == 1
  end

  test "rejects configurations that cannot traverse directories", %{root: root} do
    assert {:error, {:invalid_option, :fields, :type_required}} =
             Indexer.start_link(root, fields: [:file_id])

    assert {:error, {:unknown_options, [:mystery]}} =
             Indexer.start_link(root, mystery: true)

    assert {:error, {:rank_error, 0, _exception}} =
             Indexer.start_link(root, rank: fn _task, _read -> raise("initial rank failed") end)

    assert {:error, {:conflicting_options, :macos, [:rank]}} =
             Indexer.start_link(root,
               ranking: :macos,
               rank: fn _task, _read -> 0 end
             )

    assert {:error, {:invalid_option, :ranking, :external}} =
             Indexer.start_link(root, ranking: :external)
  end

  test "classifies failures into deliberate continue-or-abort groups" do
    assert PosixError.classify(:eperm) == {:recoverable, :access_denied}
    assert PosixError.classify(:edeadlk) == {:recoverable, :dataless}
    assert PosixError.classify(:eio) == {:recoverable, :io}
    assert PosixError.classify(:emfile) == {:fatal, :resource_exhausted}
    assert PosixError.classify(:efault) == {:fatal, :native_invariant}
    assert PosixError.classify(999) == {:fatal, :unexpected}
  end

  defp await_completions(_store, _cursor, 0), do: flunk("completion was not published")

  defp await_completions(store, cursor, attempts) do
    case Store.completed_since(store, cursor, limit: 1) do
      {:ok, [], ^cursor} ->
        Process.sleep(1)
        await_completions(store, cursor, attempts - 1)

      completion ->
        completion
    end
  end

  defp await_running_workers(_indexer, 0), do: flunk("worker pool did not become observable")

  defp await_running_workers(indexer, attempts) do
    state = :sys.get_state(indexer)

    if map_size(state.worker_references) > 0 and state.idle_workers != [] do
      state
    else
      Process.sleep(1)
      await_running_workers(indexer, attempts - 1)
    end
  end

  defp create_deep_tree!(root, depth) do
    script = """
    set -eu
    index=0
    while [ "$index" -lt "$1" ]; do
      /bin/mkdir child_0
      cd child_0
      index=$((index + 1))
    done
    /usr/bin/touch leaf
    """

    assert {"", 0} =
             System.cmd("/bin/sh", ["-c", script, "create-deep-tree", "#{depth}"], cd: root)
  end

  defp remove_deep_tree!(root) do
    script = """
    set -eu
    depth=0
    while [ -d child_0 ]; do
      cd child_0
      depth=$((depth + 1))
    done
    /bin/rm -f leaf
    while [ "$depth" -gt 0 ]; do
      cd ..
      /bin/rmdir child_0
      depth=$((depth - 1))
    done
    """

    assert {"", 0} = System.cmd("/bin/sh", ["-c", script], cd: root)
  end
end
