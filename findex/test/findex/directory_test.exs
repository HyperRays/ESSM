defmodule Findex.DirectoryTest do
  use ExUnit.Case, async: true

  alias Findex.Directory
  alias Findex.Batch
  alias Findex.Entry

  setup do
    root =
      Path.join(
        System.tmp_dir!(),
        "findex-test-#{System.os_time(:nanosecond)}-#{System.unique_integer([:positive])}"
      )

    File.mkdir_p!(Path.join(root, "empty_directory"))
    File.write!(Path.join(root, "hello.txt"), "hello")
    File.ln_s!("hello.txt", Path.join(root, "hello-link"))

    on_exit(fn -> File.rm_rf!(root) end)
    %{root: root}
  end

  test "enumerates files, directories, symlinks, and indexing metadata", %{root: root} do
    assert {:ok, entries} = Directory.list(root)
    assert Enum.all?(entries, &match?(%Entry{}, &1))

    entries_by_name = Map.new(entries, &{&1.name, &1})

    assert Map.keys(entries_by_name) |> Enum.sort() ==
             ["empty_directory", "hello-link", "hello.txt"]

    file = entries_by_name["hello.txt"]
    assert file.type == :regular
    assert file.error == nil
    assert file.total_size == 5
    assert file.data_size == 5
    assert is_integer(file.file_id)
    assert is_integer(file.parent_id)

    assert match?(
             {seconds, nanoseconds} when is_integer(seconds) and is_integer(nanoseconds),
             file.modified_at
           )

    assert match?({_, _, _, _}, file.returned_attributes)

    assert entries_by_name["empty_directory"].type == :directory
    assert entries_by_name["hello-link"].type == :symlink
  end

  test "full field preset returns expensive and APFS-specific fields without entry errors", %{
    root: root
  } do
    assert {:ok, entries} = Directory.list(root, fields: :full)
    assert Enum.all?(entries, &is_nil(&1.error))

    entries_by_name = Map.new(entries, &{&1.name, &1})
    file = entries_by_name["hello.txt"]
    directory = entries_by_name["empty_directory"]

    assert file.resource_fork_size == 0
    assert is_integer(file.private_size)
    assert is_integer(file.link_id)
    assert is_integer(file.clone_id)
    assert is_integer(file.clone_reference_count)
    assert directory.directory_entry_count == 0
  end

  test "requests only explicitly selected metadata fields", %{root: root} do
    assert {:ok, cursor} =
             Directory.open(root, fields: [:type, :file_id, :data_size])

    assert cursor.fields == [:type, :file_id, :data_size]

    entries =
      try do
        assert {:ok, entries} = Directory.next_batch(cursor)
        entries
      after
        Directory.close(cursor)
      end

    entries_by_name = Map.new(entries, &{&1.name, &1})
    file = entries_by_name["hello.txt"]

    assert file.type == :regular
    assert is_integer(file.file_id)
    assert file.data_size == 5
    assert file.total_size == nil
    assert file.modified_at == nil
    assert file.private_size == nil
    assert match?({_, _, _, _}, file.returned_attributes)
  end

  test "internally requests object type without exposing an unselected field", %{root: root} do
    assert {:ok, entries} = Directory.list(root, fields: [:allocated_size])

    assert Enum.all?(entries, fn entry ->
             is_nil(entry.type) and is_integer(entry.allocated_size)
           end)
  end

  test "always returns the fields required for safe bulk decoding", %{root: root} do
    assert {:ok, entries} = Directory.list(root, fields: [])

    assert Enum.sort(Enum.map(entries, & &1.name)) ==
             ["empty_directory", "hello-link", "hello.txt"]

    assert Enum.all?(entries, fn entry ->
             is_nil(entry.error) and is_nil(entry.type) and
               match?({_, _, _, _}, entry.returned_attributes)
           end)
  end

  test "every advertised field can be requested independently", %{root: root} do
    for field <- Directory.supported_fields() do
      assert {:ok, entries} = Directory.list(root, fields: [field])
      assert length(entries) == 3
      assert Enum.all?(entries, &is_nil(&1.error)), "field #{inspect(field)} returned an error"
    end
  end

  test "packed batches decode to the same entries as the full struct format", %{root: root} do
    assert {:ok, entries} = Directory.list(root, fields: :full)
    assert {:ok, batches} = Directory.list(root, fields: :full, format: :packed)
    assert Enum.all?(batches, &match?(%Batch{}, &1))

    packed_entries = Enum.flat_map(batches, &Batch.to_entries/1)

    assert Map.new(packed_entries, &{&1.name, &1}) ==
             Map.new(entries, &{&1.name, &1})
  end

  test "packed batches expose direct type counts and directory names", %{root: root} do
    assert {:ok, [batch]} =
             Directory.list(root, fields: [:type], format: :packed, buffer_size: 4 * 1024)

    assert batch.count == 3
    assert batch.fields == [:name, :type, :error, :returned_attributes]
    assert Batch.directory_names(batch) == ["empty_directory"]

    assert Batch.type_counts(batch) == %{
             directories: 1,
             regular_files: 1,
             symlinks: 1,
             other: 0
           }

    assert Batch.valid?(batch, :name, 0)
    refute Batch.valid?(batch, :error, 0)
    assert Batch.valid_count(batch, :error) == 0
  end

  test "cursor closes explicitly and rejects further reads", %{root: root} do
    assert {:ok, cursor} = Directory.open(root, buffer_size: 4 * 1024)
    assert {:ok, [_ | _]} = Directory.next_batch(cursor)
    assert :ok = Directory.close(cursor)
    assert :ok = Directory.close(cursor)
    assert {:error, :closed} = Directory.next_batch(cursor)
  end

  test "small batches enumerate every entry exactly once", %{root: root} do
    for index <- 1..200 do
      File.write!(Path.join(root, "generated-#{index}"), "")
    end

    assert {:ok, entries} = Directory.list(root, buffer_size: 4 * 1024)
    assert length(entries) == 203
    assert entries |> Enum.map(& &1.name) |> MapSet.new() |> MapSet.size() == 203
  end

  test "serializes concurrent reads from one native cursor", %{root: root} do
    for index <- 1..500 do
      File.write!(Path.join(root, "concurrent-#{index}"), "")
    end

    assert {:ok, cursor} =
             Directory.open(root,
               fields: [:type],
               format: :packed,
               buffer_size: 4 * 1024
             )

    names =
      1..8
      |> Task.async_stream(
        fn _reader -> consume_names(cursor, []) end,
        max_concurrency: 8,
        ordered: false
      )
      |> Enum.flat_map(fn {:ok, names} -> names end)

    assert :ok = Directory.close(cursor)
    assert length(names) == 503
    assert MapSet.size(MapSet.new(names)) == 503
  end

  test "validates options and reports path errors", %{root: root} do
    assert {:error, :invalid_fields} = Directory.open(root, fields: :unknown)
    assert {:error, :invalid_fields} = Directory.open(root, fields: [:type, :unknown])
    assert {:error, :invalid_format} = Directory.open(root, format: :unknown)

    assert {:error, {:unknown_options, [:profile]}} =
             Directory.open(root, profile: :full)

    assert {:error, :invalid_buffer_size} = Directory.open(root, buffer_size: 1)
    assert {:error, :enoent} = Directory.open(Path.join(root, "missing"))
    assert {:error, :enotdir} = Directory.open(Path.join(root, "hello.txt"))
  end

  defp consume_names(cursor, names) do
    case Directory.next_batch(cursor) do
      {:ok, batch} ->
        names =
          Batch.reduce(batch, names, fn index, names ->
            [Batch.value(batch, :name, index) | names]
          end)

        consume_names(cursor, names)

      :done ->
        names
    end
  end
end
