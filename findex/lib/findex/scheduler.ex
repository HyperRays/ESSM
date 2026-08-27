defmodule Findex.Scheduler do
  @moduledoc """
  An immutable, dependency-tracked priority queue.

  The process coordinating traversal owns the scheduler value directly. This
  keeps ranking, invalidation, and task dispatch atomic without a second
  process or message round-trips.

  Every task has a rank produced by one pure function. The function receives
  the task and a tracked `read` function:

      scheduler =
        Findex.Scheduler.new(
          rank: fn task, read ->
            case read.(:policy) do
              :depth_first -> task.depth
              :manual -> read.({:priority, task.id})
            end
          end,
          data: %{policy: :depth_first}
        )

  Calls to `read.(key)` become the task's dynamic dependencies. Changing data
  reevaluates only tasks which read a changed key, and a reevaluation replaces
  the old dependency set.

  The greatest rank is popped first. Equal ranks retain insertion order. Rank
  values may be any Erlang terms, including tuples for lexicographic ranking.

  Ranking functions must be deterministic, obtain every mutable input through
  `read`, execute quickly, and use `read` only during the ranking call.
  """

  defmodule Item do
    @moduledoc false
    @enforce_keys [:data, :rank, :dependencies, :sequence, :evaluated_at]
    defstruct [:data, :rank, :dependencies, :sequence, :evaluated_at]

    @type t :: %__MODULE__{
            data: term(),
            rank: term(),
            dependencies: MapSet.t(term()),
            sequence: non_neg_integer(),
            evaluated_at: non_neg_integer()
          }
  end

  @enforce_keys [:ranker, :data, :queue]
  defstruct ranker: nil,
            data: %{},
            revision: 0,
            tasks: %{},
            queue: nil,
            dependents: %{},
            next_sequence: 0

  @type task_id :: term()
  @type data_key :: term()
  @type rank :: term()
  @type reader :: (data_key() -> term())
  @type ranker :: (term(), reader() -> rank())
  @type rank_error :: {:rank_error, task_id(), {term(), term(), Exception.stacktrace()}}
  @type scheduler_error :: rank_error() | {:invalid_task, term()}
  @type popped_task :: {task_id(), term(), rank()}
  @type t :: %__MODULE__{
          ranker: ranker(),
          data: map(),
          revision: non_neg_integer(),
          tasks: %{task_id() => Item.t()},
          queue: :gb_trees.tree(),
          dependents: %{data_key() => MapSet.t(task_id())},
          next_sequence: non_neg_integer()
        }

  @doc """
  Creates a scheduler.

  Options:

    * `:rank` — required two-argument ranking function
    * `:data` — initial versioned data map; defaults to an empty map
  """
  @spec new(keyword()) :: t()
  def new(options) when is_list(options) do
    ranker = Keyword.fetch!(options, :rank)
    data = Keyword.get(options, :data, %{})

    unless is_function(ranker, 2) do
      raise ArgumentError, ":rank must be a two-argument function"
    end

    unless is_map(data) do
      raise ArgumentError, ":data must be a map"
    end

    %__MODULE__{ranker: ranker, data: data, queue: :gb_trees.empty()}
  end

  @doc "Inserts or replaces one pending task and evaluates its rank."
  @spec put_task(t(), task_id(), term()) :: {:ok, t()} | {:error, rank_error()}
  def put_task(%__MODULE__{} = scheduler, id, task) do
    case evaluate(scheduler.ranker, id, task, scheduler.data) do
      {:ok, rank, dependencies} ->
        {:ok, put_evaluated_task({id, task, rank, dependencies}, scheduler)}

      {:error, reason} ->
        {:error, reason}
    end
  end

  @doc """
  Atomically inserts or replaces `{id, task}` pairs.

  New tasks with equal ranks retain the order of the input list. If any task
  is invalid or ranking fails, the original scheduler remains valid and no
  updated scheduler is returned.
  """
  @spec put_tasks(t(), [{task_id(), term()}]) :: {:ok, t()} | {:error, scheduler_error()}
  def put_tasks(%__MODULE__{} = scheduler, tasks) when is_list(tasks) do
    case evaluate_new_tasks(scheduler, tasks) do
      {:ok, evaluations} ->
        {:ok, Enum.reduce(evaluations, scheduler, &put_evaluated_task/2)}

      {:error, reason} ->
        {:error, reason}
    end
  end

  @doc "Deletes a pending task."
  @spec delete_task(t(), task_id()) :: t()
  def delete_task(%__MODULE__{} = scheduler, id), do: remove_task(scheduler, id)

  @doc "Atomically changes one data value and reranks its dependent tasks."
  @spec put_data(t(), data_key(), term()) :: {:ok, t()} | {:error, rank_error()}
  def put_data(%__MODULE__{} = scheduler, key, value) do
    put_data(scheduler, %{key => value})
  end

  @doc """
  Atomically changes several data values.

  A task depending on multiple changed keys is evaluated once. If any ranking
  call fails, the original scheduler remains valid and no update is returned.
  """
  @spec put_data(t(), map()) :: {:ok, t()} | {:error, rank_error()}
  def put_data(%__MODULE__{} = scheduler, values) when is_map(values) do
    changed_values =
      Map.reject(values, fn {key, value} ->
        match?({:ok, ^value}, Map.fetch(scheduler.data, key))
      end)

    apply_data_change(
      scheduler,
      Map.merge(scheduler.data, changed_values),
      Map.keys(changed_values)
    )
  end

  @doc """
  Deletes a data key and atomically reranks dependent tasks.

  Deletion fails if a pending task's ranking function still requires the key.
  """
  @spec delete_data(t(), data_key()) :: {:ok, t()} | {:error, rank_error()}
  def delete_data(%__MODULE__{} = scheduler, key) do
    if Map.has_key?(scheduler.data, key) do
      apply_data_change(scheduler, Map.delete(scheduler.data, key), [key])
    else
      {:ok, scheduler}
    end
  end

  @doc "Returns the highest-ranked task without removing it."
  @spec peek(t()) :: {:ok, task_id(), term(), rank()} | :empty
  def peek(%__MODULE__{} = scheduler) do
    if :gb_trees.is_empty(scheduler.queue) do
      :empty
    else
      {_key, id} = :gb_trees.largest(scheduler.queue)
      item = Map.fetch!(scheduler.tasks, id)
      {:ok, id, item.data, item.rank}
    end
  end

  @doc "Returns the highest-ranked task and the updated scheduler."
  @spec pop(t()) :: {{:ok, task_id(), term(), rank()}, t()} | {:empty, t()}
  def pop(%__MODULE__{} = scheduler) do
    if :gb_trees.is_empty(scheduler.queue) do
      {:empty, scheduler}
    else
      {_key, id, queue} = :gb_trees.take_largest(scheduler.queue)
      item = Map.fetch!(scheduler.tasks, id)

      scheduler = %{
        scheduler
        | queue: queue,
          tasks: Map.delete(scheduler.tasks, id),
          dependents: remove_dependencies(scheduler.dependents, id, item.dependencies)
      }

      {{:ok, id, item.data, item.rank}, scheduler}
    end
  end

  @doc """
  Pops up to `count` tasks in descending rank order.

  This is the preferred dispatch operation for filling several free worker
  slots because it updates one locally owned scheduler value without IPC.
  """
  @spec pop_many(t(), non_neg_integer()) :: {[popped_task()], t()}
  def pop_many(%__MODULE__{} = scheduler, count)
      when is_integer(count) and count >= 0 do
    pop_many(scheduler, count, [])
  end

  @doc "Returns information about a pending task."
  @spec task_info(t(), task_id()) ::
          {:ok,
           %{
             data: term(),
             dependencies: MapSet.t(data_key()),
             evaluated_at: non_neg_integer(),
             rank: rank()
           }}
          | :error
  def task_info(%__MODULE__{} = scheduler, id) do
    case Map.fetch(scheduler.tasks, id) do
      {:ok, item} ->
        {:ok,
         %{
           data: item.data,
           rank: item.rank,
           dependencies: item.dependencies,
           evaluated_at: item.evaluated_at
         }}

      :error ->
        :error
    end
  end

  @doc "Fetches a value from the versioned data store."
  @spec fetch_data(t(), data_key()) :: {:ok, term()} | :error
  def fetch_data(%__MODULE__{} = scheduler, key), do: Map.fetch(scheduler.data, key)

  @doc "Returns scheduler size and data revision information."
  @spec status(t()) :: %{
          data_size: non_neg_integer(),
          revision: non_neg_integer(),
          size: non_neg_integer()
        }
  def status(%__MODULE__{} = scheduler) do
    %{
      size: map_size(scheduler.tasks),
      data_size: map_size(scheduler.data),
      revision: scheduler.revision
    }
  end

  defp pop_many(scheduler, 0, popped), do: {Enum.reverse(popped), scheduler}

  defp pop_many(scheduler, count, popped) do
    case pop(scheduler) do
      {{:ok, id, task, rank}, scheduler} ->
        pop_many(scheduler, count - 1, [{id, task, rank} | popped])

      {:empty, scheduler} ->
        {Enum.reverse(popped), scheduler}
    end
  end

  defp apply_data_change(scheduler, _data, []), do: {:ok, scheduler}

  defp apply_data_change(scheduler, data, changed_keys) do
    affected_ids = affected_ids(scheduler.dependents, changed_keys)

    case evaluate_tasks(scheduler, data, affected_ids) do
      {:ok, evaluations} ->
        revision = scheduler.revision + 1
        scheduler = %{scheduler | data: data, revision: revision}

        scheduler =
          Enum.reduce(evaluations, scheduler, fn {id, {rank, dependencies}}, accumulator ->
            replace_evaluation(accumulator, id, rank, dependencies, revision)
          end)

        {:ok, scheduler}

      {:error, reason} ->
        {:error, reason}
    end
  end

  defp affected_ids(dependents, changed_keys) do
    Enum.reduce(changed_keys, MapSet.new(), fn key, affected ->
      MapSet.union(affected, Map.get(dependents, key, MapSet.new()))
    end)
  end

  defp evaluate_tasks(scheduler, data, ids) do
    Enum.reduce_while(ids, {:ok, %{}}, fn id, {:ok, evaluations} ->
      item = Map.fetch!(scheduler.tasks, id)

      case evaluate(scheduler.ranker, id, item.data, data) do
        {:ok, rank, dependencies} ->
          {:cont, {:ok, Map.put(evaluations, id, {rank, dependencies})}}

        {:error, reason} ->
          {:halt, {:error, reason}}
      end
    end)
  end

  defp evaluate_new_tasks(scheduler, tasks) do
    tasks
    |> Enum.reduce_while({:ok, []}, fn
      {id, task}, {:ok, evaluations} ->
        case evaluate(scheduler.ranker, id, task, scheduler.data) do
          {:ok, rank, dependencies} ->
            {:cont, {:ok, [{id, task, rank, dependencies} | evaluations]}}

          {:error, reason} ->
            {:halt, {:error, reason}}
        end

      invalid, {:ok, _evaluations} ->
        {:halt, {:error, {:invalid_task, invalid}}}
    end)
    |> case do
      {:ok, evaluations} -> {:ok, Enum.reverse(evaluations)}
      {:error, reason} -> {:error, reason}
    end
  end

  defp evaluate(ranker, id, task, data) do
    tracker = {__MODULE__, make_ref()}
    Process.put(tracker, MapSet.new())

    read = fn key ->
      dependencies = Process.get(tracker)

      if is_nil(dependencies) do
        raise "the tracked rank-data reader was used outside its evaluation"
      end

      Process.put(tracker, MapSet.put(dependencies, key))
      Map.fetch!(data, key)
    end

    try do
      rank = ranker.(task, read)
      {:ok, rank, Process.get(tracker)}
    rescue
      exception ->
        {:error, {:rank_error, id, {:error, exception, __STACKTRACE__}}}
    catch
      kind, reason ->
        {:error, {:rank_error, id, {kind, reason, __STACKTRACE__}}}
    after
      Process.delete(tracker)
    end
  end

  defp replace_evaluation(scheduler, id, rank, dependencies, revision) do
    old_item = Map.fetch!(scheduler.tasks, id)

    item = %{
      old_item
      | rank: rank,
        dependencies: dependencies,
        evaluated_at: revision
    }

    scheduler |> remove_task(id) |> add_task(id, item)
  end

  defp put_evaluated_task({id, data, rank, dependencies}, scheduler) do
    {sequence, next_sequence} = sequence_for(scheduler, id)

    item = %Item{
      data: data,
      rank: rank,
      dependencies: dependencies,
      sequence: sequence,
      evaluated_at: scheduler.revision
    }

    scheduler = scheduler |> remove_task(id) |> add_task(id, item)
    %{scheduler | next_sequence: next_sequence}
  end

  defp sequence_for(scheduler, id) do
    case Map.fetch(scheduler.tasks, id) do
      {:ok, item} -> {item.sequence, scheduler.next_sequence}
      :error -> {scheduler.next_sequence, scheduler.next_sequence + 1}
    end
  end

  defp add_task(scheduler, id, item) do
    %{
      scheduler
      | tasks: Map.put(scheduler.tasks, id, item),
        queue: :gb_trees.insert(queue_key(item), id, scheduler.queue),
        dependents: add_dependencies(scheduler.dependents, id, item.dependencies)
    }
  end

  defp remove_task(scheduler, id) do
    case Map.pop(scheduler.tasks, id) do
      {nil, _tasks} ->
        scheduler

      {item, tasks} ->
        %{
          scheduler
          | tasks: tasks,
            queue: :gb_trees.delete_any(queue_key(item), scheduler.queue),
            dependents: remove_dependencies(scheduler.dependents, id, item.dependencies)
        }
    end
  end

  defp add_dependencies(dependents, id, dependencies) do
    Enum.reduce(dependencies, dependents, fn key, index ->
      Map.update(index, key, MapSet.new([id]), &MapSet.put(&1, id))
    end)
  end

  defp remove_dependencies(dependents, id, dependencies) do
    Enum.reduce(dependencies, dependents, fn key, index ->
      case Map.fetch(index, key) do
        {:ok, ids} ->
          ids = MapSet.delete(ids, id)
          if MapSet.size(ids) == 0, do: Map.delete(index, key), else: Map.put(index, key, ids)

        :error ->
          index
      end
    end)
  end

  defp queue_key(item), do: {item.rank, -item.sequence}
end
