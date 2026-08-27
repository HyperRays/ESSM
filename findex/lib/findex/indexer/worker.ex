defmodule Findex.Indexer.Worker do
  @moduledoc false

  alias Findex.Indexer.{DirectoryResult, DirectoryTask}
  alias Findex.Store

  @result_message :findex_indexer_worker_result
  @scan_message :findex_indexer_scan
  @stop_message :findex_indexer_stop

  @spec start(pid(), Store.t(), [atom()], pos_integer(), atom()) :: {pid(), reference()}
  def start(owner, %Store{} = store, fields, buffer_size, mount_policy)
      when is_pid(owner) and is_list(fields) and is_integer(buffer_size) do
    spawn_monitor(fn -> run(owner, store, fields, buffer_size, mount_policy) end)
  end

  @spec assign(pid(), pid(), DirectoryTask.t()) :: :ok
  def assign(worker, owner, %DirectoryTask{} = task) when is_pid(worker) and is_pid(owner) do
    send(worker, {@scan_message, owner, task})
    :ok
  end

  @spec stop(pid(), pid()) :: :ok
  def stop(worker, owner) when is_pid(worker) and is_pid(owner) do
    send(worker, {@stop_message, owner})
    :ok
  end

  defp run(owner, store, fields, buffer_size, mount_policy) do
    owner_reference = Process.monitor(owner)
    loop(owner, owner_reference, store, fields, buffer_size, mount_policy)
  end

  defp loop(owner, owner_reference, store, fields, buffer_size, mount_policy) do
    receive do
      {@scan_message, ^owner, %DirectoryTask{} = task} ->
        result = scan_directory(task, store, fields, buffer_size, mount_policy)
        send(owner, {@result_message, self(), task.id, result})
        loop(owner, owner_reference, store, fields, buffer_size, mount_policy)

      {@stop_message, ^owner} ->
        :ok

      {:DOWN, ^owner_reference, :process, ^owner, _reason} ->
        :ok

      _unexpected ->
        loop(owner, owner_reference, store, fields, buffer_size, mount_policy)
    end
  end

  defp scan_directory(task, store, fields, buffer_size, mount_policy) do
    case Store.scan_and_publish(
           store,
           task.id,
           task.path,
           fields,
           buffer_size,
           mount_policy
         ) do
      {:ok, children, counters} ->
        child_tasks =
          Enum.map(children, fn {child_id, entry_index, name} ->
            %DirectoryTask{
              id: child_id,
              path: Path.join(task.path, name),
              depth: task.depth + 1,
              parent_id: task.id,
              parent_row: entry_index,
              name: name
            }
          end)

        {:ok, %DirectoryResult{children: child_tasks, counters: counters}}

      {:error, :store, reason} ->
        {:store_error, reason}

      {:error, phase, reason} when phase in [:open, :read] ->
        {:directory_error, phase, reason}
    end
  end
end
