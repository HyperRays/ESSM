defmodule FindexRust.Bridge do
  @moduledoc """
  Handle-based stdio bridge for a Rust process embedding Findex.

  The bridge exchanges length-prefixed Erlang external-term frames over its
  private standard-I/O pipes. A started traversal remains owned by the bridge
  until `release_index`, allowing bounded completion-journal and directory-page
  reads while workers continue publishing immutable blocks.

  Ranking policies execute inside the Elixir index coordinator. The bridge only
  transports lifecycle, status, and immutable-store read operations.
  """

  alias Findex.{Batch, Directory, Indexer, Store}
  alias Findex.Indexer.{Failure, Report}
  alias Findex.Store.DirectoryNode
  alias FindexRust.Wire

  import Bitwise

  @protocol_version Wire.protocol_version()
  @maximum_request_id 18_446_744_073_709_551_615
  @maximum_directory_id 4_294_967_295
  @maximum_page_size 4_096

  defmodule State do
    @moduledoc false
    defstruct indexes: %{}, next_index_id: 0
  end

  defmodule Index do
    @moduledoc false
    @enforce_keys [:id, :root, :indexer, :store, :waiter]
    defstruct [
      :id,
      :root,
      :indexer,
      :store,
      :waiter,
      :outcome,
      completion_emitted?: false
    ]
  end

  @doc "Runs the bridge until a shutdown request or end-of-file."
  @spec run(IO.device(), IO.device()) :: :ok
  def run(input \\ :stdio, output \\ :stdio) do
    :ok = set_binary_encoding(input)
    :ok = set_binary_encoding(output)
    previous_trap_exit = Process.flag(:trap_exit, true)
    session_key = {__MODULE__, make_ref()}
    state = %State{}
    Process.put(session_key, state)
    owner = self()
    {reader, reader_monitor} = spawn_monitor(fn -> input_loop(input, owner) end)

    try do
      emit(output, %{
        "event" => "ready",
        "protocol" => @protocol_version,
        "pid" => System.pid()
      })

      event_loop(output, state, session_key, reader, reader_monitor)
      :ok
    after
      if Process.alive?(reader), do: Process.exit(reader, :shutdown)
      cleanup_all(Process.get(session_key, %State{}))
      Process.delete(session_key)
      flush_owned_exits()
      Process.flag(:trap_exit, previous_trap_exit)
    end
  end

  defp set_binary_encoding(:stdio), do: :io.setopts(:standard_io, encoding: :latin1)
  defp set_binary_encoding(device), do: :io.setopts(device, encoding: :latin1)

  defp input_loop(input, owner) do
    case Wire.read(input) do
      {:ok, request} ->
        send(owner, {:wire_frame, self(), request})
        input_loop(input, owner)

      :eof ->
        send(owner, {:wire_eof, self()})

      {:error, reason} ->
        send(owner, {:wire_error, self(), reason})
    end
  end

  defp event_loop(output, state, session_key, reader, reader_monitor) do
    Process.put(session_key, state)

    receive do
      {:wire_frame, ^reader, request} when is_map(request) ->
        case dispatch_frame(request, output, state) do
          {:continue, state} ->
            event_loop(output, state, session_key, reader, reader_monitor)

          {:stop, state} ->
            Process.put(session_key, state)
            state
        end

      {:wire_frame, ^reader, _request} ->
        emit_error(output, nil, "invalid_request", "request frame must contain a map")
        event_loop(output, state, session_key, reader, reader_monitor)

      {:wire_eof, ^reader} ->
        state

      {:wire_error, ^reader, reason} ->
        emit(output, %{"event" => "protocol_error", "message" => inspect(reason)})
        state

      {reference, outcome} when is_reference(reference) ->
        case finish_index_by_waiter(state, reference, outcome, output) do
          {:ok, state} ->
            event_loop(output, state, session_key, reader, reader_monitor)

          :unknown ->
            event_loop(output, state, session_key, reader, reader_monitor)
        end

      {:DOWN, ^reader_monitor, :process, ^reader, _reason} ->
        state

      {:DOWN, _reference, :process, _pid, _reason} ->
        event_loop(output, state, session_key, reader, reader_monitor)

      {:EXIT, _pid, _reason} ->
        event_loop(output, state, session_key, reader, reader_monitor)

      _message ->
        event_loop(output, state, session_key, reader, reader_monitor)
    end
  end

  defp dispatch_frame(request, output, state), do: dispatch(request, output, state)

  defp dispatch(request, output, state) do
    id = Map.get(request, "id")

    with :ok <- validate_request_id(id),
         {:ok, operation} <- required_binary(request, "op") do
      dispatch_operation(operation, id, request, output, state)
    else
      {:error, message} ->
        emit_error(output, wire_id(id), "invalid_request", message)
        {:continue, state}
    end
  end

  defp dispatch_operation("ping", id, request, output, state) do
    case reject_unknown_keys(request, ["id", "op"]) do
      :ok -> emit_ok(output, id, %{"pid" => System.pid(), "protocol" => @protocol_version})
      {:error, message} -> emit_error(output, id, "invalid_request", message)
    end

    {:continue, state}
  end

  defp dispatch_operation("start_scan", id, request, output, state) do
    case scan_options(request, "start_scan") do
      {:ok, root, options} ->
        start_scan(output, id, root, options, state)

      {:error, message} ->
        emit_error(output, id, "invalid_request", message)
        {:continue, state}
    end
  end

  defp dispatch_operation("index_status", id, request, output, state) do
    with :ok <- reject_unknown_keys(request, ["id", "op", "index_id"]),
         {:ok, index_id} <- required_index_id(request),
         {:ok, index} <- fetch_index(state, index_id) do
      case index_status_wire(index) do
        {:ok, status} -> emit_ok(output, id, status)
        {:error, reason} -> emit_error(output, id, "index_unavailable", inspect(reason))
      end

      {:continue, state}
    else
      {:error, :unknown_index} ->
        emit_error(output, id, "unknown_index", "index handle does not exist")
        {:continue, state}

      {:error, message} ->
        emit_error(output, id, "invalid_request", message)
        {:continue, state}
    end
  end

  defp dispatch_operation("completed_directories", id, request, output, state) do
    with :ok <-
           reject_unknown_keys(request, ["id", "op", "index_id", "cursor", "limit"]),
         {:ok, index_id} <- required_index_id(request),
         {:ok, index} <- fetch_index(state, index_id),
         {:ok, cursor} <- optional_unsigned_64(request, "cursor", 0),
         {:ok, limit} <- page_limit(request),
         {:ok, directory_ids, next_cursor} <-
           Store.completed_since(index.store, cursor, limit: limit) do
      emit_ok(output, id, %{
        "index_id" => index_id,
        "from_cursor" => cursor,
        "cursor" => next_cursor,
        "directory_ids" => directory_ids
      })

      {:continue, state}
    else
      {:error, :unknown_index} ->
        emit_error(output, id, "unknown_index", "index handle does not exist")
        {:continue, state}

      {:error, message} when is_binary(message) ->
        emit_error(output, id, "invalid_request", message)
        {:continue, state}

      {:error, reason} ->
        emit_error(output, id, "store_error", inspect(reason))
        {:continue, state}
    end
  end

  defp dispatch_operation("fetch_directory", id, request, output, state) do
    with :ok <-
           reject_unknown_keys(request, [
             "id",
             "op",
             "index_id",
             "directory_id",
             "offset",
             "limit"
           ]),
         {:ok, index_id} <- required_index_id(request),
         {:ok, index} <- fetch_index(state, index_id),
         {:ok, directory_id} <- required_directory_id(request),
         {:ok, offset} <- optional_unsigned_64(request, "offset", 0),
         {:ok, limit} <- page_limit(request),
         {:ok, directory} <- Store.fetch_directory(index.store, directory_id),
         {:ok, page} <- directory_page(directory, offset, limit) do
      emit_ok(output, id, Map.put(page, "index_id", index_id))
      {:continue, state}
    else
      {:error, :unknown_index} ->
        emit_error(output, id, "unknown_index", "index handle does not exist")
        {:continue, state}

      :error ->
        emit_error(output, id, "unknown_directory", "directory ID does not exist")
        {:continue, state}

      {:error, message} when is_binary(message) ->
        emit_error(output, id, "invalid_request", message)
        {:continue, state}

      {:error, reason} ->
        emit_error(output, id, "store_error", inspect(reason))
        {:continue, state}
    end
  end

  defp dispatch_operation("summarize_directories", id, request, output, state) do
    with :ok <-
           reject_unknown_keys(request, [
             "id",
             "op",
             "index_id",
             "directory_ids",
             "size_field",
             "largest_limit"
           ]),
         {:ok, index_id} <- required_index_id(request),
         {:ok, index} <- fetch_index(state, index_id),
         {:ok, directory_ids} <- required_directory_id_list(request),
         {:ok, size_field} <- required_size_field(request),
         {:ok, largest_limit} <- optional_unsigned_64(request, "largest_limit", 8),
         {:ok, summaries} <-
           directory_summaries(index.store, directory_ids, size_field, largest_limit) do
      emit_ok(output, id, %{"index_id" => index_id, "summaries" => summaries})
      {:continue, state}
    else
      {:error, :unknown_index} ->
        emit_error(output, id, "unknown_index", "index handle does not exist")
        {:continue, state}

      :error ->
        emit_error(output, id, "unknown_directory", "directory ID does not exist")
        {:continue, state}

      {:error, message} when is_binary(message) ->
        emit_error(output, id, "invalid_request", message)
        {:continue, state}

      {:error, reason} ->
        emit_error(output, id, "store_error", inspect(reason))
        {:continue, state}
    end
  end

  defp dispatch_operation("await_scan", id, request, output, state) do
    with :ok <- reject_unknown_keys(request, ["id", "op", "index_id"]),
         {:ok, index_id} <- required_index_id(request),
         {:ok, index} <- fetch_index(state, index_id) do
      index = await_index(index)
      {index, state} = emit_scan_finished_once(index, put_index(state, index), output)

      case scan_result_wire(index.outcome) do
        {:ok, result} -> emit_ok(output, id, result)
        {:error, reason} -> emit_error(output, id, "scan_wait_failed", inspect(reason))
      end

      {:continue, state}
    else
      {:error, :unknown_index} ->
        emit_error(output, id, "unknown_index", "index handle does not exist")
        {:continue, state}

      {:error, message} ->
        emit_error(output, id, "invalid_request", message)
        {:continue, state}
    end
  end

  defp dispatch_operation("release_index", id, request, output, state) do
    with :ok <- reject_unknown_keys(request, ["id", "op", "index_id"]),
         {:ok, index_id} <- required_index_id(request),
         {index, indexes} when not is_nil(index) <- Map.pop(state.indexes, index_id) do
      cleanup_index(index)
      emit_ok(output, id, %{"index_id" => index_id, "released" => true})
      {:continue, %{state | indexes: indexes}}
    else
      {nil, _indexes} ->
        emit_error(output, id, "unknown_index", "index handle does not exist")
        {:continue, state}

      {:error, message} ->
        emit_error(output, id, "invalid_request", message)
        {:continue, state}
    end
  end

  defp dispatch_operation("shutdown", id, request, output, state) do
    case reject_unknown_keys(request, ["id", "op"]) do
      :ok ->
        emit_ok(output, id, %{"shutdown" => true})
        {:stop, state}

      {:error, message} ->
        emit_error(output, id, "invalid_request", message)
        {:continue, state}
    end
  end

  defp dispatch_operation(operation, id, _request, output, state) do
    emit_error(output, id, "unknown_operation", "unknown operation: #{operation}")
    {:continue, state}
  end

  defp start_scan(output, request_id, root, options, %State{} = state) do
    index_id = state.next_index_id

    with true <- index_id <= @maximum_request_id,
         {:ok, index} <- create_index(index_id, root, options) do
      emit_ok(output, request_id, %{"index_id" => index_id, "root" => index.root})

      {:continue,
       %{
         state
         | indexes: Map.put(state.indexes, index_id, index),
           next_index_id: index_id + 1
       }}
    else
      false ->
        emit_error(output, request_id, "index_id_exhausted", "index handles are exhausted")
        {:continue, state}

      {:error, reason} ->
        emit_error(output, request_id, "scan_start_failed", inspect(reason))
        {:continue, state}
    end
  rescue
    exception ->
      emit_error(
        output,
        request_id,
        "scan_start_failed",
        Exception.format(:error, exception, __STACKTRACE__)
      )

      {:continue, state}
  catch
    kind, reason ->
      emit_error(
        output,
        request_id,
        "scan_start_failed",
        Exception.format(kind, reason, __STACKTRACE__)
      )

      {:continue, state}
  end

  defp create_index(index_id, root, options) do
    case Indexer.start_link(root, options) do
      {:ok, indexer} ->
        try do
          expanded_root = Path.expand(root)
          store = Indexer.store(indexer)
          waiter = Task.async(fn -> Indexer.await(indexer) end)

          {:ok,
           %Index{
             id: index_id,
             root: expanded_root,
             indexer: indexer,
             store: store,
             waiter: waiter
           }}
        rescue
          exception ->
            stop_indexer(indexer)
            {:error, {:bridge_initialization_failed, exception}}
        catch
          kind, reason ->
            stop_indexer(indexer)
            {:error, {:bridge_initialization_failed, {kind, reason}}}
        end

      {:error, reason} ->
        {:error, reason}
    end
  end

  defp await_index(%Index{waiter: nil} = index), do: index

  defp await_index(%Index{waiter: waiter} = index) do
    case Task.yield(waiter, :infinity) do
      {:ok, outcome} -> %{index | waiter: nil, outcome: outcome}
      {:exit, reason} -> %{index | waiter: nil, outcome: {:bridge_waiter_exit, reason}}
      nil -> %{index | waiter: nil, outcome: {:bridge_waiter_exit, :unexpected_timeout}}
    end
  end

  defp finish_index_by_waiter(state, reference, outcome, output) do
    case Enum.find(state.indexes, fn
           {_id, %Index{waiter: %Task{ref: ^reference}}} -> true
           _entry -> false
         end) do
      nil ->
        :unknown

      {_id, index} ->
        Process.demonitor(reference, [:flush])
        index = %{index | waiter: nil, outcome: outcome}
        state = put_index(state, index)
        {_index, state} = emit_scan_finished_once(index, state, output)
        {:ok, state}
    end
  end

  defp emit_scan_finished_once(%Index{completion_emitted?: true} = index, state, _output),
    do: {index, state}

  defp emit_scan_finished_once(%Index{} = index, state, output) do
    event =
      case scan_result_wire(index.outcome) do
        {:ok, result} ->
          %{"event" => "scan_finished", "index_id" => index.id, "result" => result}

        {:error, reason} ->
          %{
            "event" => "scan_finished",
            "index_id" => index.id,
            "error" => inspect(reason)
          }
      end

    emit(output, event)
    index = %{index | completion_emitted?: true}
    {index, put_index(state, index)}
  end

  defp index_status_wire(%Index{} = index) do
    try do
      status = Indexer.status(index.indexer)

      {:ok,
       %{
         "index_id" => index.id,
         "root" => index.root,
         "state" => Atom.to_string(status.state),
         "ranking" => Atom.to_string(status.ranking),
         "pending" => status.pending,
         "in_flight" => status.in_flight,
         "counters" => counters_wire(status.counters),
         "store" => string_key_map(status.store),
         "outcome" => outcome_name(index.outcome)
       }}
    catch
      :exit, reason -> {:error, reason}
    end
  end

  defp counters_wire(counters) do
    %{
      "entries" => counters.entries,
      "directories" => counters.directories,
      "regular_files" => counters.regular_files,
      "symlinks" => counters.symlinks,
      "other" => counters.other,
      "metadata_errors" => counters.metadata_errors,
      "metadata_error_counts" => count_map(counters.metadata_error_counts),
      "directory_failure_counts" => count_map(counters.directory_failure_counts),
      "directory_failure_reasons" => count_map(counters.directory_failure_reasons),
      "skipped_mounts" => counters.skipped_mounts
    }
  end

  defp outcome_name(nil), do: nil
  defp outcome_name({:ok, _result}), do: "ok"
  defp outcome_name({:error, %Failure{}}), do: "fatal"
  defp outcome_name({:bridge_waiter_exit, _reason}), do: "bridge_error"

  defp scan_result_wire({:ok, result}) do
    {:ok,
     %{
       "outcome" => "ok",
       "report" => report_wire(result.report),
       "failure" => nil
     }}
  end

  defp scan_result_wire({:error, %Failure{} = failure}) do
    {:ok,
     %{
       "outcome" => "fatal",
       "report" => report_wire(failure.report),
       "failure" => %{
         "kind" => Atom.to_string(failure.kind),
         "reason" => inspect(failure.reason)
       }
     }}
  end

  defp scan_result_wire({:bridge_waiter_exit, reason}), do: {:error, reason}
  defp scan_result_wire(nil), do: {:error, :missing_outcome}

  defp directory_page(%DirectoryNode{} = directory, offset, limit) do
    count = if match?(%Batch{}, directory.entries), do: directory.entries.count, else: 0

    if offset > count do
      {:error, "offset exceeds directory entry count"}
    else
      next_offset = min(offset + limit, count)

      {child_ids, child_count} =
        Enum.reduce(directory.children, {%{}, 0}, fn {row, child_id}, {page_ids, total} ->
          page_ids =
            if row >= offset and row < next_offset do
              Map.put(page_ids, row, child_id)
            else
              page_ids
            end

          {page_ids, total + 1}
        end)

      entries =
        case directory.entries do
          %Batch{} = batch -> directory_entries_wire(batch, child_ids, offset, next_offset)
          nil -> []
        end

      {:ok,
       %{
         "directory_id" => directory.id,
         "state" => Atom.to_string(directory.state),
         "parent_id" => wire_nullable(directory.parent_id),
         "name" => wire_binary(directory.name),
         "error" => wire_value(directory.error),
         "entry_count" => count,
         "child_count" => child_count,
         "offset" => offset,
         "next_offset" => next_offset,
         "done" => next_offset == count,
         "entries" => entries
       }}
    end
  end

  @maximum_summarize_batch 4_096
  @histogram_buckets 44

  defp required_directory_id_list(request) do
    case Map.fetch(request, "directory_ids") do
      {:ok, ids} when is_list(ids) and length(ids) <= @maximum_summarize_batch ->
        if Enum.all?(ids, &(is_integer(&1) and &1 >= 0 and &1 <= @maximum_directory_id)) do
          {:ok, ids}
        else
          {:error, "directory_ids must contain directory IDs"}
        end

      _other ->
        {:error, "directory_ids must be a list of at most #{@maximum_summarize_batch} IDs"}
    end
  end

  defp required_size_field(request) do
    case Map.fetch(request, "size_field") do
      {:ok, name} when is_binary(name) ->
        try do
          {:ok, String.to_existing_atom(name)}
        rescue
          ArgumentError -> {:error, "size_field #{name} is not a known field"}
        end

      _other ->
        {:error, "size_field must be a string"}
    end
  end

  defp directory_summaries(store, directory_ids, size_field, largest_limit) do
    directory_ids
    |> Enum.reduce_while({:ok, []}, fn directory_id, {:ok, acc} ->
      with {:ok, directory} <- Store.fetch_directory(store, directory_id),
           {:ok, summary} <-
             directory_summary(directory, directory_id, size_field, largest_limit) do
        {:cont, {:ok, [summary | acc]}}
      else
        error -> {:halt, error}
      end
    end)
    |> case do
      {:ok, summaries} -> {:ok, Enum.reverse(summaries)}
      error -> error
    end
  end

  defp directory_summary(%DirectoryNode{} = directory, directory_id, size_field, largest_limit) do
    batch = directory.entries

    with {:ok, {bytes, largest, histogram}} <- summarize_batch(batch, size_field, largest_limit) do
      {:ok,
       %{
         "directory_id" => directory_id,
         "state" => Atom.to_string(directory.state),
         "parent_id" => wire_nullable(directory.parent_id),
         "name" => wire_binary(directory.name),
         "error" => wire_value(directory.error),
         "entry_count" => if(match?(%Batch{}, batch), do: batch.count, else: 0),
         "child_count" => Enum.count(directory.children),
         "size_bytes" => bytes,
         "largest" =>
           largest
           |> Enum.reverse()
           |> Enum.map(fn {size, name} -> %{"name" => wire_binary(name), "size" => size} end),
         "histogram" =>
           Enum.map(histogram, fn {bucket, {count, bucket_bytes}} ->
             [bucket, count, bucket_bytes]
           end)
       }}
    end
  end

  defp summarize_batch(%Batch{} = batch, size_field, largest_limit) do
    with {:ok, types} <- Map.fetch(batch.columns, :type),
         {:ok, type_validity} <- Map.fetch(batch.validity, :type),
         {:ok, sizes} <- Map.fetch(batch.columns, size_field),
         {:ok, size_validity} <- Map.fetch(batch.validity, size_field),
         true <- byte_size(types) == batch.count,
         true <- byte_size(sizes) == batch.count * 8 do
      {:ok,
       summarize_size_columns(
         batch,
         types,
         sizes,
         type_validity,
         size_validity,
         0,
         largest_limit,
         0,
         [],
         %{}
       )}
    else
      :error -> {:error, "size_field is not retained by this scan"}
      false -> {:error, "size_field is not a signed 64-bit size column"}
    end
  end

  defp summarize_batch(_entries, _size_field, _largest_limit), do: {:ok, {0, [], %{}}}

  # Stored batches are columnar. Walking the two relevant columns once avoids
  # the map lookup, bounds validation, validity lookup, and binary slicing that
  # `Batch.value/3` would otherwise repeat for every field of every row.
  defp summarize_size_columns(
         _batch,
         <<>>,
         <<>>,
         _type_validity,
         _size_validity,
         _row,
         _largest_limit,
         bytes,
         largest,
         histogram
       ),
       do: {bytes, largest, histogram}

  defp summarize_size_columns(
         batch,
         <<type, remaining_types::binary>>,
         <<raw_size::signed-native-64, remaining_sizes::binary>>,
         type_validity,
         size_validity,
         row,
         largest_limit,
         bytes,
         largest,
         histogram
       ) do
    {bytes, largest, histogram} =
      if type == 1 and validity_bit_set?(type_validity, row) do
        size = if validity_bit_set?(size_validity, row), do: max(raw_size, 0), else: 0
        bucket = size_bucket(size)

        histogram =
          Map.update(histogram, bucket, {1, size}, fn {count, bucket_bytes} ->
            {count + 1, bucket_bytes + size}
          end)

        {bytes + size, note_largest(largest, batch, row, size, largest_limit), histogram}
      else
        {bytes, largest, histogram}
      end

    summarize_size_columns(
      batch,
      remaining_types,
      remaining_sizes,
      type_validity,
      size_validity,
      row + 1,
      largest_limit,
      bytes,
      largest,
      histogram
    )
  end

  defp validity_bit_set?(bitmap, row) do
    (:binary.at(bitmap, div(row, 8)) &&& 1 <<< rem(row, 8)) != 0
  end

  # Keeps an ascending list of at most `limit` `{size, name}` pairs; names
  # are only decoded for entries that actually enter the list.
  defp note_largest(largest, _batch, _row, _size, 0), do: largest
  defp note_largest(largest, _batch, _row, 0, _limit), do: largest

  defp note_largest(largest, batch, row, size, limit) do
    cond do
      length(largest) < limit ->
        insert_by_size(largest, {size, Batch.value(batch, :name, row)})

      size > elem(hd(largest), 0) ->
        [_smallest | kept] = largest
        insert_by_size(kept, {size, Batch.value(batch, :name, row)})

      true ->
        largest
    end
  end

  defp insert_by_size(list, {size, _name} = entry) do
    {smaller, rest} = Enum.split_while(list, fn {other, _} -> other <= size end)
    smaller ++ [entry | rest]
  end

  defp size_bucket(0), do: 0
  defp size_bucket(size), do: min(bit_count(size, 0), @histogram_buckets - 1)

  defp bit_count(0, count), do: count
  defp bit_count(size, count), do: bit_count(div(size, 2), count + 1)

  defp directory_entries_wire(_batch, _child_ids, offset, offset), do: []

  defp directory_entries_wire(batch, child_ids, offset, next_offset) do
    Enum.map(offset..(next_offset - 1), fn row ->
      %{
        "row" => row,
        "child_directory_id" => wire_nullable(Map.get(child_ids, row)),
        "values" => entry_values_wire(batch, row)
      }
    end)
  end

  defp entry_values_wire(batch, row) do
    Map.new(batch.fields, fn field ->
      {Atom.to_string(field), field_wire_value(field, Batch.value(batch, field, row))}
    end)
  end

  defp field_wire_value(_field, nil), do: nil
  defp field_wire_value(:name, value), do: wire_binary(value)

  defp field_wire_value(_field, value), do: wire_value(value)

  defp wire_value(nil), do: nil
  defp wire_value(value) when is_atom(value), do: Atom.to_string(value)
  defp wire_value(value) when is_binary(value), do: wire_binary(value)

  defp wire_value(value) when is_tuple(value),
    do: value |> Tuple.to_list() |> Enum.map(&wire_value/1)

  defp wire_value(value) when is_list(value), do: Enum.map(value, &wire_value/1)
  defp wire_value(value), do: value

  defp wire_binary(value) when is_binary(value), do: value

  defp scan_options(request, operation) do
    allowed = [
      "id",
      "op",
      "root",
      "fields",
      "concurrency",
      "buffer_size",
      "ranking",
      "mount_policy",
      "failure_sample_limit"
    ]

    with :ok <- reject_unknown_keys(request, allowed),
         true <- Map.get(request, "op") == operation,
         {:ok, root} <- required_nonempty_binary(request, "root"),
         {:ok, fields} <- parse_fields(Map.get(request, "fields", ["type"])),
         {:ok, concurrency} <- optional_positive_integer(request, "concurrency", nil),
         {:ok, buffer_size} <- optional_positive_integer(request, "buffer_size", nil),
         {:ok, ranking} <- parse_ranking(Map.get(request, "ranking", "default")),
         {:ok, mount_policy} <-
           parse_mount_policy(Map.get(request, "mount_policy", "stay_on_filesystem")),
         {:ok, failure_sample_limit} <-
           optional_non_negative_integer(request, "failure_sample_limit", nil) do
      options =
        [fields: fields, ranking: ranking, mount_policy: mount_policy]
        |> put_option(:concurrency, concurrency)
        |> put_option(:buffer_size, buffer_size)
        |> put_option(:failure_sample_limit, failure_sample_limit)

      {:ok, root, options}
    else
      false -> {:error, "operation mismatch"}
      error -> error
    end
  end

  defp parse_fields(fields) when is_list(fields) and fields != [] do
    fields_by_name = Map.new(Directory.supported_fields(), &{Atom.to_string(&1), &1})

    Enum.reduce_while(fields, {:ok, []}, fn field, {:ok, parsed} ->
      case fields_by_name do
        %{^field => atom} when is_binary(field) -> {:cont, {:ok, [atom | parsed]}}
        _other -> {:halt, {:error, "unsupported field: #{inspect(field)}"}}
      end
    end)
    |> case do
      {:ok, parsed} ->
        parsed = parsed |> Enum.reverse() |> Enum.uniq()

        if :type in parsed do
          {:ok, parsed}
        else
          {:error, "fields must contain type"}
        end

      error ->
        error
    end
  end

  defp parse_fields(_fields), do: {:error, "fields must be a non-empty string array"}

  defp parse_mount_policy("stay_on_filesystem"), do: {:ok, :stay_on_filesystem}
  defp parse_mount_policy("cross"), do: {:ok, :cross}

  defp parse_mount_policy(_policy),
    do: {:error, "mount_policy must be stay_on_filesystem or cross"}

  defp parse_ranking("default"), do: {:ok, :default}
  defp parse_ranking("name_biased"), do: {:ok, :name_biased}
  defp parse_ranking("macos"), do: {:ok, :macos}

  defp parse_ranking(_ranking),
    do: {:error, "ranking must be default, name_biased, or macos"}

  defp required_binary(request, key) do
    case Map.fetch(request, key) do
      {:ok, value} when is_binary(value) -> {:ok, value}
      :error -> {:error, "missing #{key}"}
      _other -> {:error, "#{key} must be a string"}
    end
  end

  defp required_nonempty_binary(request, key) do
    with {:ok, value} <- required_binary(request, key) do
      if value == "", do: {:error, "#{key} must not be empty"}, else: {:ok, value}
    end
  end

  defp required_index_id(request) do
    case Map.fetch(request, "index_id") do
      {:ok, value} when is_integer(value) and value >= 0 and value <= @maximum_request_id ->
        {:ok, value}

      :error ->
        {:error, "missing index_id"}

      _other ->
        {:error, "index_id must be an unsigned 64-bit integer"}
    end
  end

  defp required_directory_id(request) do
    case Map.fetch(request, "directory_id") do
      {:ok, value} when is_integer(value) and value >= 0 and value <= @maximum_directory_id ->
        {:ok, value}

      :error ->
        {:error, "missing directory_id"}

      _other ->
        {:error, "directory_id must be an unsigned 32-bit integer"}
    end
  end

  defp optional_positive_integer(request, key, default) do
    case Map.fetch(request, key) do
      :error -> {:ok, default}
      {:ok, value} when is_integer(value) and value > 0 -> {:ok, value}
      _other -> {:error, "#{key} must be a positive integer"}
    end
  end

  defp optional_non_negative_integer(request, key, default) do
    case Map.fetch(request, key) do
      :error -> {:ok, default}
      {:ok, value} when is_integer(value) and value >= 0 -> {:ok, value}
      _other -> {:error, "#{key} must be a non-negative integer"}
    end
  end

  defp optional_unsigned_64(request, key, default) do
    case Map.fetch(request, key) do
      :error ->
        {:ok, default}

      {:ok, value}
      when is_integer(value) and value >= 0 and value <= @maximum_request_id ->
        {:ok, value}

      _other ->
        {:error, "#{key} must be an unsigned 64-bit integer"}
    end
  end

  defp page_limit(request) do
    with {:ok, limit} <- optional_positive_integer(request, "limit", 256) do
      if limit <= @maximum_page_size do
        {:ok, limit}
      else
        {:error, "limit exceeds #{@maximum_page_size}"}
      end
    end
  end

  defp reject_unknown_keys(request, allowed) do
    case Map.keys(request) -- allowed do
      [] -> :ok
      unknown -> {:error, "unknown request keys: #{Enum.sort(unknown) |> inspect()}"}
    end
  end

  defp validate_request_id(id)
       when is_integer(id) and id >= 0 and id <= @maximum_request_id,
       do: :ok

  defp validate_request_id(_id), do: {:error, "id must be an unsigned 64-bit integer"}

  defp fetch_index(%State{indexes: indexes}, index_id) do
    case Map.fetch(indexes, index_id) do
      {:ok, index} -> {:ok, index}
      :error -> {:error, :unknown_index}
    end
  end

  defp put_index(%State{} = state, %Index{} = index) do
    %{state | indexes: Map.put(state.indexes, index.id, index)}
  end

  defp put_option(options, _key, nil), do: options
  defp put_option(options, key, value), do: Keyword.put(options, key, value)

  defp report_wire(%Report{} = report) do
    %{
      "root" => wire_binary(report.root),
      "complete" => report.complete?,
      "elapsed_ms" => report.elapsed_milliseconds,
      "entries" => report.entries,
      "directories" => report.directories,
      "regular_files" => report.regular_files,
      "symlinks" => report.symlinks,
      "other" => report.other,
      "metadata_errors" => report.metadata_errors,
      "metadata_error_counts" => count_map(report.metadata_error_counts),
      "directory_failure_counts" => count_map(report.directory_failure_counts),
      "directory_failure_reasons" => count_map(report.directory_failure_reasons),
      "directory_failure_samples" =>
        Enum.map(report.directory_failure_samples, fn failure ->
          %{
            "id" => failure.id,
            "path" => wire_binary(failure.path),
            "phase" => Atom.to_string(failure.phase),
            "reason" => to_string(failure.reason),
            "category" => Atom.to_string(failure.category)
          }
        end),
      "skipped_mounts" => report.skipped_mounts,
      "store" => string_key_map(report.store)
    }
  end

  defp count_map(counts) do
    Map.new(counts, fn {key, value} -> {to_string(key), value} end)
  end

  defp string_key_map(map) do
    Map.new(map, fn {key, value} -> {to_string(key), value} end)
  end

  defp wire_nullable(nil), do: nil
  defp wire_nullable(value), do: value

  defp wire_id(id) when is_integer(id) and id >= 0 and id <= @maximum_request_id, do: id
  defp wire_id(_id), do: nil

  defp emit_ok(output, id, result) do
    emit(output, %{"id" => id, "status" => "ok", "result" => result})
  end

  defp emit_error(output, id, code, message) do
    emit(output, %{
      "id" => id,
      "status" => "error",
      "error" => %{"code" => code, "message" => message}
    })
  end

  defp emit(output, value), do: Wire.write(output, value)

  defp cleanup_all(%State{} = state),
    do: Enum.each(state.indexes, fn {_id, index} -> cleanup_index(index) end)

  defp cleanup_index(%Index{} = index) do
    if index.waiter, do: Task.shutdown(index.waiter, :brutal_kill)
    stop_indexer(index.indexer)
    :ok
  end

  defp stop_indexer(indexer) do
    if Process.alive?(indexer) do
      try do
        GenServer.stop(indexer, :normal, 5_000)
      catch
        :exit, _reason -> :ok
      end
    end
  end

  defp flush_owned_exits do
    receive do
      {:EXIT, _pid, _reason} -> flush_owned_exits()
    after
      0 -> :ok
    end
  end
end
