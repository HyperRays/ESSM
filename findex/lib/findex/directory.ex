defmodule Findex.Directory do
  @moduledoc """
  Fast, non-recursive macOS directory enumeration using `getattrlistbulk(2)`.

  A cursor owns an open directory descriptor. Consume it in bounded batches
  and close it when finished. The resource is also closed automatically when
  garbage-collected.

  The default `:fast` field preset requests useful indexing metadata without
  the attributes Apple identifies as potentially expensive. The `:full`
  preset requests every supported field. Pass an explicit field list when a
  scan needs a smaller or specialized metadata set.
  """

  alias Findex.Nif

  @default_buffer_size 256 * 1024
  @min_buffer_size 4 * 1024
  @max_buffer_size 16 * 1024 * 1024

  @always_fields [:name, :error, :returned_attributes]
  @selectable_fields [
    :type,
    :object_tag,
    :device,
    :filesystem_id,
    :file_id,
    :parent_id,
    :created_at,
    :modified_at,
    :changed_at,
    :accessed_at,
    :backed_up_at,
    :added_at,
    :owner_id,
    :group_id,
    :mode,
    :flags,
    :user_access,
    :finder_info,
    :owner_uuid,
    :group_uuid,
    :acl,
    :data_protection_flags,
    :generation_count,
    :document_id,
    :link_count,
    :total_size,
    :allocated_size,
    :io_block_size,
    :device_type,
    :data_size,
    :data_allocated_size,
    :resource_fork_size,
    :resource_fork_allocated_size,
    :directory_entry_count,
    :mount_status,
    :private_size,
    :link_id,
    :real_device,
    :real_filesystem_id,
    :clone_id,
    :extended_flags,
    :recursive_generation_count,
    :attribution_tag,
    :clone_reference_count
  ]

  @supported_fields @always_fields ++ @selectable_fields

  @fast_fields [
    :type,
    :object_tag,
    :device,
    :filesystem_id,
    :file_id,
    :parent_id,
    :created_at,
    :modified_at,
    :changed_at,
    :accessed_at,
    :added_at,
    :owner_id,
    :group_id,
    :mode,
    :flags,
    :user_access,
    :link_count,
    :total_size,
    :allocated_size,
    :io_block_size,
    :device_type,
    :data_size,
    :data_allocated_size,
    :mount_status
  ]

  defmodule Cursor do
    @moduledoc false
    @enforce_keys [:resource, :buffer_size, :fields, :format]
    defstruct [:resource, :buffer_size, :fields, :format]

    @type t :: %__MODULE__{
            resource: reference(),
            buffer_size: pos_integer(),
            fields: [Findex.Directory.field()],
            format: Findex.Directory.format()
          }
  end

  @type profile :: :fast | :full
  @type field ::
          :name
          | :type
          | :object_tag
          | :error
          | :device
          | :filesystem_id
          | :file_id
          | :parent_id
          | :created_at
          | :modified_at
          | :changed_at
          | :accessed_at
          | :backed_up_at
          | :added_at
          | :owner_id
          | :group_id
          | :mode
          | :flags
          | :user_access
          | :finder_info
          | :owner_uuid
          | :group_uuid
          | :acl
          | :data_protection_flags
          | :generation_count
          | :document_id
          | :link_count
          | :total_size
          | :allocated_size
          | :io_block_size
          | :device_type
          | :data_size
          | :data_allocated_size
          | :resource_fork_size
          | :resource_fork_allocated_size
          | :directory_entry_count
          | :mount_status
          | :private_size
          | :link_id
          | :real_device
          | :real_filesystem_id
          | :clone_id
          | :extended_flags
          | :recursive_generation_count
          | :attribution_tag
          | :clone_reference_count
          | :returned_attributes
  @type field_selection :: profile() | [field()]
  @type format :: :entries | :packed
  @type option ::
          {:fields, field_selection()}
          | {:format, format()}
          | {:buffer_size, pos_integer()}

  @doc "Returns every field accepted by the `:fields` option."
  @spec supported_fields() :: [field()]
  def supported_fields, do: @supported_fields

  @doc """
  Opens `path` for enumeration.

  Options:

    * `:fields` — `:fast` (default), `:full`, or a list of individual fields
    * `:format` — `:entries` (default) or allocation-efficient `:packed`
    * `:buffer_size` — bytes fetched per batch; defaults to 256 KiB

  `:name`, `:error`, and `:returned_attributes` are always requested because
  `getattrlistbulk(2)` requires them or needs them for safe decoding.
  """
  @spec open(Path.t(), [option()]) :: {:ok, Cursor.t()} | {:error, atom() | integer()}
  def open(path, options \\ []) do
    open_cursor(options, fn fields, format_code ->
      Nif.open_directory(path, fields, format_code, 0)
    end)
  end

  @doc false
  @spec open_traversal(Path.t(), [option()]) ::
          {:ok, Cursor.t()} | {:error, atom() | integer()}
  def open_traversal(path, options \\ []) do
    open_cursor(options, fn fields, format_code ->
      Nif.open_directory(path, fields, format_code, 1)
    end)
  end

  @doc false
  @spec open_store(reference(), non_neg_integer(), [option()]) ::
          {:ok, Cursor.t()} | {:error, term()}
  def open_store(store_resource, directory_id, options \\ [])
      when is_reference(store_resource) and is_integer(directory_id) and directory_id >= 0 do
    open_cursor(options, fn fields, format_code ->
      Nif.index_store_open_directory(store_resource, directory_id, fields, format_code)
    end)
  end

  @doc """
  Retrieves one batch from `cursor`.

  Returns `{:ok, entries}`, `:done`, or `{:error, reason}`.
  """
  @spec next_batch(Cursor.t()) ::
          {:ok, [Findex.Entry.t()] | Findex.Batch.t()} | :done | {:error, term()}
  def next_batch(%Cursor{resource: resource, buffer_size: buffer_size}) do
    Nif.next_directory_batch(resource, buffer_size)
  end

  @doc "Closes the cursor. Calling this more than once is safe."
  @spec close(Cursor.t()) :: :ok
  def close(%Cursor{resource: resource}), do: Nif.close_directory(resource)

  @doc """
  Enumerates all immediate children of `path` and closes the cursor.

  Prefer the cursor API for very large directories so entries can be processed
  without retaining the entire result in memory.
  """
  @spec list(Path.t(), [option()]) ::
          {:ok, [Findex.Entry.t()] | [Findex.Batch.t()]} | {:error, term()}
  def list(path, options \\ []) do
    case open(path, options) do
      {:ok, cursor} ->
        try do
          collect(cursor, [])
        after
          close(cursor)
        end

      {:error, reason} ->
        {:error, reason}
    end
  end

  defp collect(%Cursor{format: :entries} = cursor, batches) do
    case next_batch(cursor) do
      {:ok, entries} -> collect(cursor, [entries | batches])
      :done -> {:ok, batches |> Enum.reverse() |> Enum.flat_map(& &1)}
      {:error, reason} -> {:error, reason}
    end
  end

  defp collect(%Cursor{format: :packed} = cursor, batches) do
    case next_batch(cursor) do
      {:ok, batch} -> collect(cursor, [batch | batches])
      :done -> {:ok, Enum.reverse(batches)}
      {:error, reason} -> {:error, reason}
    end
  end

  defp open_cursor(options, opener) do
    buffer_size = Keyword.get(options, :buffer_size, @default_buffer_size)
    format = Keyword.get(options, :format, :entries)

    with :ok <- validate_option_names(options),
         {:ok, fields} <- resolve_fields(options),
         {:ok, format_code} <- format_code(format),
         :ok <- validate_buffer_size(buffer_size),
         {:ok, resource} <- opener.(fields, format_code) do
      {:ok,
       %Cursor{
         resource: resource,
         buffer_size: buffer_size,
         fields: fields,
         format: format
       }}
    end
  end

  defp resolve_fields(options) do
    case Keyword.fetch(options, :fields) do
      {:ok, selection} -> normalize_fields(selection)
      :error -> {:ok, @fast_fields}
    end
  end

  defp normalize_fields(:fast), do: {:ok, @fast_fields}
  defp normalize_fields(:full), do: {:ok, @selectable_fields}

  defp normalize_fields(fields) when is_list(fields) do
    if Enum.all?(fields, &(&1 in @supported_fields)) do
      fields = fields |> Enum.reject(&(&1 in @always_fields)) |> Enum.uniq()
      {:ok, fields}
    else
      {:error, :invalid_fields}
    end
  end

  defp normalize_fields(_fields), do: {:error, :invalid_fields}

  defp validate_option_names(options) do
    case Keyword.keys(options) -- [:fields, :format, :buffer_size] do
      [] -> :ok
      unknown -> {:error, {:unknown_options, unknown}}
    end
  end

  defp format_code(:entries), do: {:ok, 0}
  defp format_code(:packed), do: {:ok, 1}
  defp format_code(_format), do: {:error, :invalid_format}

  defp validate_buffer_size(size)
       when is_integer(size) and size >= @min_buffer_size and size <= @max_buffer_size,
       do: :ok

  defp validate_buffer_size(_size), do: {:error, :invalid_buffer_size}
end
