defmodule Findex.StoreTest do
  use ExUnit.Case, async: true

  alias Findex.{Batch, Directory, Store}
  alias Findex.Store.DirectoryNode

  setup do
    root =
      Path.join(
        System.tmp_dir!(),
        "findex-store-test-#{System.os_time(:nanosecond)}-#{System.unique_integer([:positive])}"
      )

    File.mkdir_p!(root)

    for index <- 1..200 do
      File.write!(Path.join(root, "entry-#{index}"), "")
    end

    for index <- 1..8 do
      child = Path.join(root, "child-#{index}")
      File.mkdir!(child)
      File.write!(Path.join(child, "file"), "")
    end

    fields = [:type, :file_id]

    {:ok, batches} =
      Directory.list(root, fields: fields, format: :packed, buffer_size: 4 * 1024)

    store = Store.new(root, fields: fields)

    on_exit(fn -> File.rm_rf!(root) end)

    %{batches: batches, child_entries: directory_entries(batches), root: root, store: store}
  end

  test "publishes one consolidated immutable block", %{
    batches: batches,
    child_entries: child_entries,
    root: root,
    store: store
  } do
    assert {:ok, published_children} =
             Store.publish_directory(store, Store.root_id(store), batches, child_entries)

    assert {:ok,
            %DirectoryNode{
              id: 0,
              state: :published,
              parent_id: nil,
              name: ^root,
              entries: %Batch{} = entries,
              children: children,
              error: nil
            }} = Store.fetch_directory(store, 0)

    expected_entries = Enum.flat_map(batches, &Batch.to_entries/1)
    assert Batch.to_entries(entries) == expected_entries
    assert entries.count == length(expected_entries)
    assert length(children) == length(child_entries)

    assert Enum.map(published_children, fn {id, row, _name} -> {row, id} end) == children

    stats = Store.stats(store)
    assert stats.directory_count == length(child_entries) + 1
    assert stats.published_directory_count == 1
    assert stats.failed_directory_count == 0
    assert stats.pending_directory_count == length(child_entries)
    assert stats.completion_count == 1
    assert stats.entry_count == entries.count
    assert stats.block_bytes > stats.payload_bytes
    assert stats.native_bytes >= stats.block_bytes
    assert stats.directory_table_bytes > 0
    assert stats.completion_journal_bytes > 0

    assert {:ok, [0], 1} = Store.completed_since(store, 0)
    assert {:ok, [], 1} = Store.completed_since(store, 1)

    assert {:ok, ^root} = Store.path(store, 0)

    Enum.each(published_children, fn {id, _row, name} ->
      assert {:ok,
              %DirectoryNode{
                state: :pending,
                parent_id: 0,
                name: ^name,
                entries: nil,
                error: nil
              }} =
               Store.fetch_directory(store, id)

      expected_path = Path.join(root, name)
      assert {:ok, ^expected_path} = Store.path(store, id)
    end)
  end

  test "fused scan publishes the same consolidated block and counters", %{
    batches: batches,
    child_entries: child_entries,
    root: root
  } do
    fields = [:type, :file_id]
    store = Store.new(root, fields: fields)
    assert length(batches) > 1

    assert {:ok, published_children, counters} =
             Store.scan_and_publish(store, 0, root, fields, 4 * 1024, :cross)

    assert counters == %{
             entries: 208,
             directories: 8,
             regular_files: 200,
             symlinks: 0,
             other: 0,
             metadata_errors: 0,
             metadata_error_counts: %{},
             skipped_mounts: 0
           }

    assert Enum.map(published_children, fn {id, row, name} -> {row, id, name} end) ==
             Enum.zip_with(child_entries, 1..length(child_entries), fn {row, name}, id ->
               {row, id, name}
             end)

    assert {:ok, %DirectoryNode{entries: fused_entries}} = Store.fetch_directory(store, 0)
    expected_entries = Enum.flat_map(batches, &Batch.to_entries/1)
    assert Batch.to_entries(fused_entries) == expected_entries

    assert {:error, :store, :already_completed} =
             Store.scan_and_publish(store, 0, root, fields, 4 * 1024, :cross)
  end

  test "fused scan leaves a missing directory pending for failure publication", %{root: root} do
    missing = Path.join(root, "missing")
    store = Store.new(missing, fields: [:type])

    assert {:error, :open, :enoent} =
             Store.scan_and_publish(store, 0, missing, [:type], 4 * 1024, :cross)

    assert %{pending_directory_count: 1, published_directory_count: 0} = Store.stats(store)
    assert :ok = Store.fail_directory(store, 0, :enoent)
  end

  test "a directory has exactly one publisher", %{
    batches: batches,
    child_entries: child_entries,
    store: store
  } do
    assert {:ok, _children} = Store.publish_directory(store, 0, batches, child_entries)

    assert {:error, :already_completed} =
             Store.publish_directory(store, 0, batches, child_entries)

    assert Store.published_directory_count(store) == 1
    assert {:ok, [0], 1} = Store.completed_since(store, 0)
  end

  test "records failed directories without confusing them with pending work", %{
    batches: batches,
    child_entries: child_entries,
    root: root,
    store: store
  } do
    assert {:ok, [{child_id, _row, child_name} | _children]} =
             Store.publish_directory(store, 0, batches, child_entries)

    assert :ok = Store.fail_directory(store, child_id, :eacces)

    assert {:ok,
            %DirectoryNode{
              state: :failed,
              name: ^child_name,
              entries: nil,
              children: [],
              error: :eacces
            }} = Store.fetch_directory(store, child_id)

    assert {:ok, expected_path} = Store.path(store, child_id)
    assert expected_path == Path.join(root, child_name)

    stats = Store.stats(store)
    assert stats.published_directory_count == 1
    assert stats.failed_directory_count == 1
    assert stats.pending_directory_count == length(child_entries) - 1
    assert stats.completion_count == 2
    assert {:ok, [0, ^child_id], 2} = Store.completed_since(store, 0)

    assert {:error, :already_completed} = Store.fail_directory(store, child_id, :eperm)
    assert {:error, :already_completed} = Store.publish_directory(store, child_id, [], [])
  end

  test "accepts concurrent publication of independent directories", %{
    batches: root_batches,
    child_entries: child_entries,
    root: root,
    store: store
  } do
    assert {:ok, children} = Store.publish_directory(store, 0, root_batches, child_entries)

    results =
      children
      |> Task.async_stream(
        fn {directory_id, _row, name} ->
          {:ok, batches} =
            Directory.list(Path.join(root, name),
              fields: [:type, :file_id],
              format: :packed
            )

          Store.publish_directory(store, directory_id, batches, [])
        end,
        max_concurrency: 8,
        ordered: false
      )
      |> Enum.to_list()

    assert Enum.all?(results, &match?({:ok, {:ok, []}}, &1))
    assert Store.directory_count(store) == 9
    assert Store.published_directory_count(store) == 9
    assert Store.entry_count(store) == Enum.sum(Enum.map(root_batches, & &1.count)) + 8

    assert {:ok, [0 | completed_children], 9} =
             Store.completed_since(store, 0, limit: 32)

    assert MapSet.new(completed_children) ==
             MapSet.new(Enum.map(children, fn {directory_id, _row, _name} -> directory_id end))
  end

  test "paginates independent completion cursors in terminal order", %{
    batches: root_batches,
    child_entries: child_entries,
    store: store
  } do
    assert {:ok, [first, second, third | _children]} =
             Store.publish_directory(store, 0, root_batches, child_entries)

    {first_id, _row, _name} = first
    {second_id, _row, _name} = second
    {third_id, _row, _name} = third

    assert {:ok, []} = Store.publish_directory(store, third_id, [], [])
    assert :ok = Store.fail_directory(store, first_id, :eacces)
    assert {:ok, []} = Store.publish_directory(store, second_id, [], [])

    assert {:ok, [0, ^third_id], 2} = Store.completed_since(store, 0, limit: 2)
    assert {:ok, [^first_id, ^second_id], 4} = Store.completed_since(store, 2, limit: 2)
    assert {:ok, [], 4} = Store.completed_since(store, 4, limit: 2)

    assert {:ok, [0, ^third_id, ^first_id, ^second_id], 4} =
             Store.completed_since(store, 0, limit: 10)

    assert {:error, :invalid_cursor} = Store.completed_since(store, 5)
    assert_raise ArgumentError, fn -> Store.completed_since(store, 0, limit: 0) end
    assert_raise ArgumentError, fn -> Store.completed_since(store, 0, unknown: true) end
  end

  test "rejects child links that do not identify directory rows", %{
    batches: batches,
    store: store
  } do
    regular_file_index =
      batches
      |> Enum.with_index()
      |> Enum.reduce_while(0, fn {batch, _batch_index}, offset ->
        case Enum.find(0..(batch.count - 1), &(Batch.value(batch, :type, &1) == :regular)) do
          nil -> {:cont, offset + batch.count}
          index -> {:halt, offset + index}
        end
      end)

    assert {:error, :invalid_block} =
             Store.publish_directory(store, 0, batches, [{regular_file_index, "not-a-dir"}])

    assert Store.published_directory_count(store) == 0
  end

  test "rejects malformed packed blocks and unsafe stored basenames", %{
    store: store
  } do
    for name <- ["", ".", "..", "nested/name", <<"nul", 0, "name">>] do
      batch = synthetic_directory_batch(name)

      assert {:error, :invalid_block} =
               Store.publish_directory(store, 0, [batch], [{0, name}])
    end

    batch = synthetic_directory_batch("child")

    bad_reference =
      put_in(batch.columns.name, <<100::unsigned-native-32, 20::unsigned-native-32>>)

    assert {:error, :invalid_block} =
             Store.publish_directory(store, 0, [bad_reference], [{0, "child"}])

    bad_column = put_in(batch.columns.file_id, <<0>>)

    assert {:error, :invalid_block} =
             Store.publish_directory(store, 0, [bad_column], [{0, "child"}])

    bad_validity = put_in(batch.validity.name, <<>>)

    assert {:error, :invalid_block} =
             Store.publish_directory(store, 0, [bad_validity], [{0, "child"}])

    assert {:error, :invalid_block} =
             Store.publish_directory(store, 0, [batch], [{0, "child"}, {0, "child"}])

    assert {:error, :invalid_block} =
             Store.publish_directory(store, 0, [batch], [{4_294_967_295, "child"}])

    assert Store.published_directory_count(store) == 0
    assert Store.directory_count(store) == 1
  end

  test "one terminal transition wins under publication contention", %{store: store} do
    results =
      1..64
      |> Task.async_stream(
        fn index ->
          if rem(index, 2) == 0 do
            Store.publish_directory(store, 0, [], [])
          else
            Store.fail_directory(store, 0, :eacces)
          end
        end,
        max_concurrency: 16,
        ordered: false
      )
      |> Enum.map(fn {:ok, result} -> result end)

    assert Enum.count(results, &(&1 in [:ok, {:ok, []}])) == 1
    assert Enum.count(results, &(&1 == {:error, :already_completed})) == 63

    stats = Store.stats(store)
    assert stats.completion_count == 1
    assert stats.pending_directory_count == 0
    assert stats.published_directory_count + stats.failed_directory_count == 1
  end

  defp directory_entries(batches) do
    {entries, _offset} =
      Enum.map_reduce(batches, 0, fn batch, offset ->
        entries =
          Enum.map(Batch.directory_entries(batch), fn {index, name} ->
            {offset + index, name}
          end)

        {entries, offset + batch.count}
      end)

    List.flatten(entries)
  end

  defp synthetic_directory_batch(name) do
    %Batch{
      count: 1,
      fields: [:name, :type, :error, :file_id, :returned_attributes],
      storage: name,
      columns: %{
        name: <<0::unsigned-native-32, byte_size(name)::unsigned-native-32>>,
        type: <<2>>,
        error: <<0::unsigned-native-32>>,
        file_id: <<0::unsigned-native-64>>,
        returned_attributes: <<0::unsigned-native-128>>
      },
      validity: %{
        name: <<1>>,
        type: <<1>>,
        error: <<0>>,
        file_id: <<0>>,
        returned_attributes: <<1>>
      }
    }
  end
end
