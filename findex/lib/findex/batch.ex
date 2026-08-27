defmodule Findex.Batch do
  @moduledoc """
  A packed, column-oriented batch returned by `Findex.Directory`.

  Each selected field is stored once as a native-width binary column. Missing
  values are represented by per-field validity bitmaps rather than allocated
  `nil` slots. Variable-length values reference the shared `storage` binary by
  `{offset, length}` pairs.

  Use `value/3` for occasional random access, `reduce/3` for streaming access,
  and `to_entries/1` only when the allocation-heavy struct representation is
  explicitly required.
  """

  import Bitwise

  alias Findex.{Entry, PosixError}

  @enforce_keys [:count, :fields, :storage, :columns, :validity]
  defstruct [:count, :fields, :storage, :columns, :validity]

  @type field :: Findex.Directory.field()
  @type t :: %__MODULE__{
          count: non_neg_integer(),
          fields: [field()],
          storage: binary(),
          columns: %{field() => binary()},
          validity: %{field() => binary()}
        }

  @reference_fields [:name, :acl]
  @u32_fields [
    :object_tag,
    :owner_id,
    :group_id,
    :mode,
    :flags,
    :user_access,
    :data_protection_flags,
    :generation_count,
    :document_id,
    :link_count,
    :io_block_size,
    :device_type,
    :directory_entry_count,
    :mount_status,
    :clone_reference_count
  ]
  @u64_fields [
    :device,
    :file_id,
    :parent_id,
    :link_id,
    :real_device,
    :clone_id,
    :extended_flags,
    :recursive_generation_count,
    :attribution_tag
  ]
  @i64_fields [
    :total_size,
    :allocated_size,
    :data_size,
    :data_allocated_size,
    :resource_fork_size,
    :resource_fork_allocated_size,
    :private_size
  ]
  @timestamp_fields [
    :created_at,
    :modified_at,
    :changed_at,
    :accessed_at,
    :backed_up_at,
    :added_at
  ]
  @fsid_fields [:filesystem_id, :real_filesystem_id]
  @uuid_fields [:owner_uuid, :group_uuid]

  @doc "Returns whether `field` has a value at `index`."
  @spec valid?(t(), field(), non_neg_integer()) :: boolean()
  def valid?(%__MODULE__{} = batch, field, index) do
    validate_index!(batch, index)
    bitmap = Map.fetch!(batch.validity, field)
    (byte_at(bitmap, div(index, 8)) &&& 1 <<< rem(index, 8)) != 0
  end

  @doc "Counts values present in a field's validity bitmap."
  @spec valid_count(t(), field()) :: non_neg_integer()
  def valid_count(%__MODULE__{} = batch, field) do
    bitmap = Map.fetch!(batch.validity, field)
    count_valid_bits(bitmap, batch.count, 0)
  end

  @doc "Counts exact per-entry POSIX errors without materializing rows."
  @spec error_counts(t()) :: %{PosixError.reason() => pos_integer()}
  def error_counts(%__MODULE__{} = batch) do
    errors = Map.fetch!(batch.columns, :error)
    validity = Map.fetch!(batch.validity, :error)
    count_errors(errors, validity, 0, %{})
  end

  @doc "Returns a decoded field value, or `nil` when it was not returned."
  @spec value(t(), field(), non_neg_integer()) :: term()
  def value(%__MODULE__{} = batch, field, index) do
    validate_index!(batch, index)
    column = Map.fetch!(batch.columns, field)

    if valid?(batch, field, index) do
      decode(batch, field, column, index)
    end
  end

  @doc "Reduces over row indexes without constructing maps or structs."
  @spec reduce(t(), accumulator, (non_neg_integer(), accumulator -> accumulator)) :: accumulator
        when accumulator: term()
  def reduce(%__MODULE__{count: count}, accumulator, reducer)
      when is_function(reducer, 2) do
    reduce_indexes(0, count, accumulator, reducer)
  end

  @doc "Returns names of the directory entries in this batch."
  @spec directory_names(t()) :: [binary()]
  def directory_names(%__MODULE__{} = batch) do
    Enum.map(directory_entries(batch), fn {_index, name} -> name end)
  end

  @doc "Returns `{row_index, name}` for traversable directory entries."
  @spec directory_entries(t()) :: [{non_neg_integer(), binary()}]
  def directory_entries(%__MODULE__{} = batch) do
    types = Map.fetch!(batch.columns, :type)
    type_validity = Map.fetch!(batch.validity, :type)
    error_validity = Map.fetch!(batch.validity, :error)

    collect_directory_entries(batch, types, type_validity, error_validity, 0, [])
  end

  @doc "Counts entry types directly from the packed one-byte type column."
  @spec type_counts(t()) :: %{
          directories: non_neg_integer(),
          regular_files: non_neg_integer(),
          symlinks: non_neg_integer(),
          other: non_neg_integer()
        }
  def type_counts(%__MODULE__{} = batch) do
    types = Map.fetch!(batch.columns, :type)
    validity = Map.fetch!(batch.validity, :type)

    {directories, regular_files, symlinks, other} =
      count_types(types, validity, 0, 0, 0, 0, 0)

    %{
      directories: directories,
      regular_files: regular_files,
      symlinks: symlinks,
      other: other
    }
  end

  @doc "Materializes the packed batch as `%Findex.Entry{}` structs."
  @spec to_entries(t()) :: [Entry.t()]
  def to_entries(%__MODULE__{} = batch) do
    for index <- indexes(batch.count) do
      Enum.reduce(batch.fields, %Entry{}, fn field, entry ->
        Map.put(entry, field, value(batch, field, index))
      end)
    end
  end

  defp decode(batch, field, column, index) when field in @reference_fields do
    <<offset::unsigned-native-32, length::unsigned-native-32>> =
      binary_part(column, index * 8, 8)

    binary_part(batch.storage, offset, length)
  end

  defp decode(_batch, :type, column, index) do
    case byte_at(column, index) do
      1 -> :regular
      2 -> :directory
      3 -> :symlink
      4 -> :block_device
      5 -> :character_device
      6 -> :socket
      7 -> :fifo
      _other -> :unknown
    end
  end

  defp decode(_batch, :error, column, index) do
    <<error_number::unsigned-native-32>> = binary_part(column, index * 4, 4)
    error_reason(error_number)
  end

  defp decode(_batch, field, column, index) when field in @u32_fields do
    <<value::unsigned-native-32>> = binary_part(column, index * 4, 4)
    value
  end

  defp decode(_batch, field, column, index) when field in @u64_fields do
    <<value::unsigned-native-64>> = binary_part(column, index * 8, 8)
    value
  end

  defp decode(_batch, field, column, index) when field in @i64_fields do
    <<value::signed-native-64>> = binary_part(column, index * 8, 8)
    value
  end

  defp decode(_batch, field, column, index) when field in @timestamp_fields do
    <<seconds::signed-native-64, nanoseconds::signed-native-64>> =
      binary_part(column, index * 16, 16)

    {seconds, nanoseconds}
  end

  defp decode(_batch, field, column, index) when field in @fsid_fields do
    <<first::signed-native-32, second::signed-native-32>> =
      binary_part(column, index * 8, 8)

    {first, second}
  end

  defp decode(_batch, field, column, index) when field in @uuid_fields do
    binary_part(column, index * 16, 16)
  end

  defp decode(_batch, :finder_info, column, index) do
    binary_part(column, index * 32, 32)
  end

  defp decode(_batch, :returned_attributes, column, index) do
    <<common::unsigned-native-32, directory::unsigned-native-32, file::unsigned-native-32,
      extended::unsigned-native-32>> =
      binary_part(column, index * 16, 16)

    {common, directory, file, extended}
  end

  defp collect_directory_entries(
         _batch,
         <<>>,
         _type_validity,
         _error_validity,
         _index,
         entries
       ),
       do: Enum.reverse(entries)

  defp collect_directory_entries(
         batch,
         <<type, remaining::binary>>,
         type_validity,
         error_validity,
         index,
         entries
       ) do
    entries =
      if type == 2 and bit_set?(type_validity, index) and
           not bit_set?(error_validity, index) do
        [{index, value(batch, :name, index)} | entries]
      else
        entries
      end

    collect_directory_entries(
      batch,
      remaining,
      type_validity,
      error_validity,
      index + 1,
      entries
    )
  end

  defp count_types(<<>>, _validity, _index, directories, regular_files, symlinks, other),
    do: {directories, regular_files, symlinks, other}

  defp count_types(
         <<type, remaining::binary>>,
         validity,
         index,
         directories,
         regular_files,
         symlinks,
         other
       ) do
    {directories, regular_files, symlinks, other} =
      if bit_set?(validity, index) do
        case type do
          1 -> {directories, regular_files + 1, symlinks, other}
          2 -> {directories + 1, regular_files, symlinks, other}
          3 -> {directories, regular_files, symlinks + 1, other}
          _other -> {directories, regular_files, symlinks, other + 1}
        end
      else
        {directories, regular_files, symlinks, other + 1}
      end

    count_types(
      remaining,
      validity,
      index + 1,
      directories,
      regular_files,
      symlinks,
      other
    )
  end

  defp count_errors(<<>>, _validity, _index, counts), do: counts

  defp count_errors(
         <<error_number::unsigned-native-32, remaining::binary>>,
         validity,
         index,
         counts
       ) do
    counts =
      if bit_set?(validity, index) do
        Map.update(counts, PosixError.reason(error_number), 1, &(&1 + 1))
      else
        counts
      end

    count_errors(remaining, validity, index + 1, counts)
  end

  defp count_valid_bits(_bitmap, 0, count), do: count

  defp count_valid_bits(<<byte, remaining::binary>>, bits_remaining, count) do
    bits_in_byte = min(bits_remaining, 8)
    mask = (1 <<< bits_in_byte) - 1

    count_valid_bits(
      remaining,
      bits_remaining - bits_in_byte,
      count + popcount(byte &&& mask)
    )
  end

  defp popcount(byte) do
    byte = byte - (byte >>> 1 &&& 0x55)
    byte = (byte &&& 0x33) + (byte >>> 2 &&& 0x33)
    byte + (byte >>> 4) &&& 0x0F
  end

  defp reduce_indexes(index, count, accumulator, _reducer) when index == count,
    do: accumulator

  defp reduce_indexes(index, count, accumulator, reducer) do
    reduce_indexes(index + 1, count, reducer.(index, accumulator), reducer)
  end

  defp indexes(0), do: []
  defp indexes(count), do: 0..(count - 1)

  defp bit_set?(bitmap, index) do
    (byte_at(bitmap, div(index, 8)) &&& 1 <<< rem(index, 8)) != 0
  end

  defp byte_at(binary, index), do: :binary.at(binary, index)

  defp validate_index!(%__MODULE__{count: count}, index)
       when is_integer(index) and index >= 0 and index < count,
       do: :ok

  defp validate_index!(%__MODULE__{count: count}, index) do
    raise ArgumentError, "batch index #{inspect(index)} is outside 0..#{max(count - 1, 0)}"
  end

  defp error_reason(error_number), do: PosixError.reason(error_number)
end
