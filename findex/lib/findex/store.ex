defmodule Findex.Store do
  @moduledoc """
  A concurrent, append-only filesystem tree.

  Directories have compact numeric IDs. A traversal worker privately gathers
  all packed batches for one directory and publishes them once as a single
  immutable native block. Publishing also reserves a contiguous ID range for
  the child directories found in that block.

  Directory names are not duplicated in a second path table. Every non-root
  directory points to the corresponding name row in its parent's packed block;
  complete paths are reconstructed only when requested.

  The native resource owns the directory table and packed blocks. Concurrent
  workers may call `publish_directory/4` for different directory IDs. A
  directory ID has one terminal transition: either `:published` or `:failed`
  with an exact POSIX reason. Until then its state is `:pending`.
  """

  alias Findex.{Batch, Directory, Nif}

  defmodule DirectoryNode do
    @moduledoc "A pending, published, or failed directory node from the native tree."

    @enforce_keys [:id, :state, :parent_id, :name, :entries, :children, :error]
    defstruct [:id, :state, :parent_id, :name, :entries, :children, :error]

    @type t :: %__MODULE__{
            id: Findex.Store.directory_id(),
            state: Findex.Store.directory_state(),
            parent_id: Findex.Store.directory_id() | nil,
            name: binary(),
            entries: Batch.t() | nil,
            children: [{non_neg_integer(), Findex.Store.directory_id()}],
            error: atom() | pos_integer() | nil
          }
  end

  @always_fields [:name, :error, :returned_attributes]
  @root_id 0

  @enforce_keys [:resource]
  defstruct [:resource]

  @opaque t :: %__MODULE__{resource: reference()}
  @type directory_id :: non_neg_integer()
  @type directory_state :: :pending | :published | :failed
  @type child_entry :: {non_neg_integer(), binary()}
  @type published_child :: {directory_id(), non_neg_integer(), binary()}
  @type completion_cursor :: non_neg_integer()
  @type stats :: %{
          directory_count: non_neg_integer(),
          published_directory_count: non_neg_integer(),
          failed_directory_count: non_neg_integer(),
          pending_directory_count: non_neg_integer(),
          completion_count: non_neg_integer(),
          entry_count: non_neg_integer(),
          block_bytes: non_neg_integer(),
          payload_bytes: non_neg_integer(),
          directory_table_bytes: non_neg_integer(),
          completion_journal_bytes: non_neg_integer(),
          root_name_bytes: non_neg_integer(),
          native_bytes: non_neg_integer()
        }

  @doc "Creates a native tree rooted at `root_path` for one fixed field schema."
  @spec new(Path.t(), fields: [Directory.field()]) :: t()
  def new(root_path, options) when is_binary(root_path) and is_list(options) do
    fields = options |> Keyword.fetch!(:fields) |> normalize_fields!()
    root_path = Path.expand(root_path)

    case Nif.index_store_new(root_path, fields) do
      {:ok, resource} -> %__MODULE__{resource: resource}
      {:error, reason} -> raise "could not create index store: #{inspect(reason)}"
    end
  end

  @doc "Returns the stable ID assigned to the root directory."
  @spec root_id(t()) :: 0
  def root_id(%__MODULE__{}), do: @root_id

  @doc """
  Atomically publishes one directory and reserves its child directory IDs.

  `child_entries` contains `{row_index, basename}` pairs for directory rows in
  `batches`. The basenames are returned to the caller for constructing
  ephemeral traversal paths; only the row indexes and numeric IDs are retained
  by the store.
  """
  @spec publish_directory(t(), directory_id(), [Batch.t()], [child_entry()]) ::
          {:ok, [published_child()]} | {:error, term()}
  def publish_directory(
        %__MODULE__{resource: resource},
        directory_id,
        batches,
        child_entries
      )
      when is_integer(directory_id) and directory_id >= 0 and is_list(batches) and
             is_list(child_entries) do
    child_entry_indices = Enum.map(child_entries, fn {entry_index, _name} -> entry_index end)

    case Nif.index_store_publish(resource, directory_id, batches, child_entry_indices) do
      {:ok, child_ids} ->
        published =
          Enum.zip_with(child_ids, child_entries, fn child_id, {entry_index, name} ->
            {child_id, entry_index, name}
          end)

        {:ok, published}

      {:error, _reason} = error ->
        error
    end
  end

  @doc false
  @spec scan_and_publish(
          t(),
          directory_id(),
          Path.t(),
          [Directory.field()],
          pos_integer(),
          :cross | :stay_on_filesystem
        ) ::
          {:ok, [published_child()], map()}
          | {:error, :open | :read | :store, term()}
  def scan_and_publish(
        %__MODULE__{resource: resource},
        directory_id,
        path,
        fields,
        buffer_size,
        mount_policy
      )
      when is_integer(directory_id) and directory_id >= 0 and is_binary(path) and
             is_list(fields) and is_integer(buffer_size) and buffer_size > 0 and
             mount_policy in [:cross, :stay_on_filesystem] do
    mount_policy_code = if mount_policy == :cross, do: 0, else: 1

    case Nif.index_store_scan_and_publish(
           resource,
           directory_id,
           path,
           fields,
           buffer_size,
           mount_policy_code
         ) do
      {:ok, children,
       {entries, directories, regular_files, symlinks, other, metadata_errors,
        metadata_error_counts, skipped_mounts}} ->
        {:ok, children,
         %{
           entries: entries,
           directories: directories,
           regular_files: regular_files,
           symlinks: symlinks,
           other: other,
           metadata_errors: metadata_errors,
           metadata_error_counts: Map.new(metadata_error_counts),
           skipped_mounts: skipped_mounts
         }}

      {:error, phase, _reason} = error when phase in [:open, :read, :store] ->
        error
    end
  end

  @doc "Marks a reserved directory as failed with its exact POSIX reason."
  @spec fail_directory(t(), directory_id(), atom() | pos_integer()) ::
          :ok | {:error, term()}
  def fail_directory(%__MODULE__{resource: resource}, directory_id, reason)
      when is_integer(directory_id) and directory_id >= 0 and
             (is_atom(reason) or (is_integer(reason) and reason > 0)) do
    Nif.index_store_fail(resource, directory_id, reason)
  end

  @doc """
  Returns terminal directory IDs appended after `cursor` and the next cursor.

  Both published and failed directory IDs appear exactly once in atomic
  completion order. Cursors are independent immutable offsets, so multiple
  consumers may advance at their own rates while traversal continues.
  """
  @spec completed_since(t(), completion_cursor(), keyword()) ::
          {:ok, [directory_id()], completion_cursor()} | {:error, term()}
  def completed_since(%__MODULE__{resource: resource}, cursor, options \\ [])
      when is_integer(cursor) and cursor >= 0 and is_list(options) do
    case Keyword.keys(options) -- [:limit] do
      [] ->
        limit = Keyword.get(options, :limit, 256)

        if is_integer(limit) and limit > 0 do
          Nif.index_store_completed_since(resource, cursor, limit)
        else
          raise ArgumentError, ":limit must be a positive integer"
        end

      unknown ->
        raise ArgumentError, "unknown completion options: #{inspect(unknown)}"
    end
  end

  @doc "Fetches a directory and a zero-copy view of its consolidated block."
  @spec fetch_directory(t(), directory_id()) ::
          {:ok, DirectoryNode.t()} | :error | {:error, term()}
  def fetch_directory(%__MODULE__{resource: resource}, directory_id)
      when is_integer(directory_id) and directory_id >= 0 do
    case Nif.index_store_fetch(resource, directory_id) do
      {:ok, {state, parent_id, name, entries, children, error}} ->
        {:ok,
         %DirectoryNode{
           id: directory_id,
           state: state,
           parent_id: parent_id,
           name: name,
           entries: entries,
           children: children,
           error: error
         }}

      :error ->
        :error

      {:error, _reason} = error ->
        error
    end
  end

  @doc "Reconstructs a directory path by following numeric parent IDs."
  @spec path(t(), directory_id()) :: {:ok, Path.t()} | :error | {:error, term()}
  def path(%__MODULE__{} = store, directory_id)
      when is_integer(directory_id) and directory_id >= 0 do
    with {:ok, components} <- path_components(store, directory_id, []) do
      {:ok, Path.join(components)}
    end
  end

  @doc "Returns native tree counters and retained byte counts."
  @spec stats(t()) :: stats()
  def stats(%__MODULE__{resource: resource}), do: Nif.index_store_stats(resource)

  @doc false
  @spec close_traversal(t()) :: :ok
  def close_traversal(%__MODULE__{resource: resource}),
    do: Nif.index_store_close_traversal(resource)

  @doc "Counts reserved directory nodes, including the root."
  @spec directory_count(t()) :: non_neg_integer()
  def directory_count(%__MODULE__{} = store), do: stats(store).directory_count

  @doc "Counts atomically published directory blocks."
  @spec published_directory_count(t()) :: non_neg_integer()
  def published_directory_count(%__MODULE__{} = store),
    do: stats(store).published_directory_count

  @doc "Counts filesystem entries in all published directory blocks."
  @spec entry_count(t()) :: non_neg_integer()
  def entry_count(%__MODULE__{} = store), do: stats(store).entry_count

  @doc "Returns native allocation and logical payload measurements."
  @spec memory_usage(t()) :: stats()
  def memory_usage(%__MODULE__{} = store), do: stats(store)

  defp path_components(store, directory_id, components) do
    case fetch_directory(store, directory_id) do
      {:ok, %DirectoryNode{parent_id: nil, name: root}} ->
        {:ok, [root | components]}

      {:ok, %DirectoryNode{parent_id: parent_id, name: name}} ->
        path_components(store, parent_id, [name | components])

      :error ->
        :error

      {:error, _reason} = error ->
        error
    end
  end

  defp normalize_fields!(fields) when is_list(fields) do
    supported = Directory.supported_fields()

    if Enum.all?(fields, &(&1 in supported)) do
      fields
      |> Enum.reject(&(&1 in @always_fields))
      |> Enum.uniq()
    else
      raise ArgumentError, ":fields contains an unsupported filesystem field"
    end
  end

  defp normalize_fields!(_fields), do: raise(ArgumentError, ":fields must be a list")
end
