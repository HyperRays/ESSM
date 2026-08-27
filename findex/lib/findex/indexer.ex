defmodule Findex.Indexer do
  @moduledoc """
  Concurrent recursive filesystem indexing with explicit completion semantics.

  One coordinator owns the dynamic `Findex.Scheduler`; workers enumerate one
  directory at a time and atomically publish complete native blocks. Directory
  failures caused by permissions, filesystem mutation, or local I/O are stored
  and reported. Resource exhaustion, native invariants, worker crashes, and
  store errors abort the run with a structured partial report.

  The default `:stay_on_filesystem` mount policy does not descend into mount
  points or automount triggers. Set `mount_policy: :cross` to traverse them.
  """

  use GenServer

  alias Findex.{Directory, PosixError, Ranking, Scheduler, Store}
  alias Findex.Indexer.Worker

  @default_buffer_size 256 * 1024
  @minimum_buffer_size 4 * 1024
  @maximum_buffer_size 16 * 1024 * 1024
  defmodule DirectoryTask do
    @moduledoc "Data supplied to the traversal ranking function."

    @enforce_keys [:id, :path, :depth]
    defstruct [
      :id,
      :path,
      :depth,
      :parent_id,
      :parent_row,
      :name
    ]

    @type t :: %__MODULE__{
            id: Store.directory_id(),
            path: Path.t(),
            depth: non_neg_integer(),
            parent_id: Store.directory_id() | nil,
            parent_row: non_neg_integer() | nil,
            name: binary() | nil
          }
  end

  defmodule DirectoryFailure do
    @moduledoc "One sampled recoverable directory failure."

    @enforce_keys [:id, :path, :phase, :reason, :category]
    defstruct [:id, :path, :phase, :reason, :category]

    @type t :: %__MODULE__{
            id: Store.directory_id(),
            path: Path.t(),
            phase: :open | :read,
            reason: PosixError.reason(),
            category: PosixError.category()
          }
  end

  defmodule Report do
    @moduledoc "Completion and coverage report for an indexing run."

    @enforce_keys [
      :root,
      :complete?,
      :elapsed_milliseconds,
      :entries,
      :directories,
      :regular_files,
      :symlinks,
      :other,
      :metadata_errors,
      :metadata_error_counts,
      :directory_failure_counts,
      :directory_failure_reasons,
      :directory_failure_samples,
      :skipped_mounts,
      :store
    ]
    defstruct @enforce_keys

    @type t :: %__MODULE__{
            root: Path.t(),
            complete?: boolean(),
            elapsed_milliseconds: float(),
            entries: non_neg_integer(),
            directories: non_neg_integer(),
            regular_files: non_neg_integer(),
            symlinks: non_neg_integer(),
            other: non_neg_integer(),
            metadata_errors: non_neg_integer(),
            metadata_error_counts: %{PosixError.reason() => pos_integer()},
            directory_failure_counts: %{PosixError.category() => pos_integer()},
            directory_failure_reasons: %{PosixError.reason() => pos_integer()},
            directory_failure_samples: [DirectoryFailure.t()],
            skipped_mounts: non_neg_integer(),
            store: Store.stats()
          }
  end

  defmodule Result do
    @moduledoc "A completed index and its coverage report."

    @enforce_keys [:store, :report]
    defstruct [:store, :report]

    @type t :: %__MODULE__{store: Store.t(), report: Report.t()}
  end

  defmodule Failure do
    @moduledoc "A fatal traversal failure with the safely retained partial tree."

    @enforce_keys [:kind, :reason, :task, :store, :report]
    defstruct [:kind, :reason, :task, :store, :report]

    @type t :: %__MODULE__{
            kind: :directory | :ranking | :store | :worker,
            reason: term(),
            task: DirectoryTask.t() | nil,
            store: Store.t(),
            report: Report.t()
          }
  end

  defmodule Counters do
    @moduledoc false
    defstruct entries: 0,
              directories: 0,
              regular_files: 0,
              symlinks: 0,
              other: 0,
              metadata_errors: 0,
              metadata_error_counts: %{},
              directory_failure_counts: %{},
              directory_failure_reasons: %{},
              directory_failure_samples: [],
              skipped_mounts: 0
  end

  defmodule DirectoryResult do
    @moduledoc false
    @enforce_keys [:children, :counters]
    defstruct [:children, :counters]
  end

  defmodule Config do
    @moduledoc false
    @enforce_keys [
      :root,
      :fields,
      :concurrency,
      :buffer_size,
      :ranking,
      :rank,
      :rank_data,
      :mount_policy,
      :failure_sample_limit
    ]
    defstruct @enforce_keys
  end

  defmodule State do
    @moduledoc false
    @enforce_keys [
      :config,
      :store,
      :scheduler,
      :worker_references,
      :idle_workers,
      :started_at
    ]
    defstruct config: nil,
              store: nil,
              scheduler: nil,
              worker_references: %{},
              idle_workers: [],
              started_at: nil,
              in_flight: %{},
              counters: %Counters{},
              waiters: [],
              outcome: nil
  end

  @type option ::
          {:fields, [Directory.field()]}
          | {:concurrency, pos_integer()}
          | {:buffer_size, pos_integer()}
          | {:ranking, Ranking.policy()}
          | {:rank, Scheduler.ranker()}
          | {:rank_data, map()}
          | {:mount_policy, :stay_on_filesystem | :cross}
          | {:failure_sample_limit, non_neg_integer()}

  @doc "Starts an asynchronous traversal coordinator."
  @spec start_link(Path.t(), [option()]) :: GenServer.on_start()
  def start_link(root, options \\ []) do
    with {:ok, config} <- configure(root, options),
         {:ok, store, scheduler} <- prepare_index(config) do
      GenServer.start_link(__MODULE__, {config, store, scheduler})
    end
  end

  @doc "Returns the default number of persistent traversal workers."
  @spec default_concurrency() :: pos_integer()
  def default_concurrency, do: 2 * :erlang.system_info(:dirty_io_schedulers)

  @doc "Runs an index to completion and stops its coordinator."
  @spec run(Path.t(), [option()]) :: {:ok, Result.t()} | {:error, Failure.t()} | {:error, term()}
  def run(root, options \\ []) do
    with {:ok, indexer} <- start_link(root, options) do
      try do
        await(indexer, :infinity)
      after
        if Process.alive?(indexer), do: GenServer.stop(indexer, :normal)
      end
    end
  end

  @doc "Waits for an asynchronous traversal to complete or abort."
  @spec await(GenServer.server(), timeout()) :: {:ok, Result.t()} | {:error, Failure.t()}
  def await(indexer, timeout \\ :infinity), do: GenServer.call(indexer, :await, timeout)

  @doc "Atomically updates one dynamic rank input for all dependent pending tasks."
  @spec put_rank_data(GenServer.server(), Scheduler.data_key(), term()) ::
          :ok | {:error, term()}
  def put_rank_data(indexer, key, value),
    do: GenServer.call(indexer, {:put_rank_data, %{key => value}})

  @doc "Atomically updates several dynamic rank inputs."
  @spec put_rank_data(GenServer.server(), map()) :: :ok | {:error, term()}
  def put_rank_data(indexer, values) when is_map(values),
    do: GenServer.call(indexer, {:put_rank_data, values})

  @doc "Returns live queue, worker, counter, and native-store information."
  @spec status(GenServer.server()) :: map()
  def status(indexer), do: GenServer.call(indexer, :status)

  @doc "Returns the concurrently readable native store owned by a running indexer."
  @spec store(GenServer.server()) :: Store.t()
  def store(indexer), do: GenServer.call(indexer, :store)

  @impl true
  def init({%Config{} = config, %Store{} = store, %Scheduler{} = scheduler}) do
    {:ok,
     %State{
       config: config,
       store: store,
       scheduler: scheduler,
       worker_references: %{},
       idle_workers: [],
       started_at: System.monotonic_time()
     }, {:continue, :start_workers}}
  end

  @impl true
  def handle_continue(:start_workers, state) do
    {:noreply, state |> start_worker_pool() |> dispatch_and_finish()}
  end

  @impl true
  def handle_call(:await, _from, %State{outcome: outcome} = state) when not is_nil(outcome),
    do: {:reply, outcome, state}

  def handle_call(:await, from, state),
    do: {:noreply, %{state | waiters: [from | state.waiters]}}

  def handle_call(:store, _from, state), do: {:reply, state.store, state}

  def handle_call({:put_rank_data, _values}, _from, %State{outcome: outcome} = state)
      when not is_nil(outcome),
      do: {:reply, {:error, :finished}, state}

  def handle_call({:put_rank_data, values}, _from, state) do
    case Scheduler.put_data(state.scheduler, values) do
      {:ok, scheduler} ->
        {:reply, :ok, dispatch_and_finish(%{state | scheduler: scheduler})}

      {:error, reason} ->
        {:reply, {:error, reason}, state}
    end
  end

  def handle_call(:status, _from, state) do
    scheduler_status = Scheduler.status(state.scheduler)

    status = %{
      state: if(is_nil(state.outcome), do: :running, else: :finished),
      ranking: state.config.ranking,
      pending: scheduler_status.size,
      in_flight: map_size(state.in_flight),
      counters: state.counters,
      store: Store.stats(state.store)
    }

    {:reply, status, state}
  end

  @impl true
  def handle_info(
        {:findex_indexer_worker_result, worker, task_id, worker_result},
        state
      )
      when is_map_key(state.in_flight, worker) do
    {task, in_flight} = Map.pop!(state.in_flight, worker)
    state = %{state | in_flight: in_flight, idle_workers: [worker | state.idle_workers]}

    if task.id == task_id do
      state = handle_worker_result(state, task, worker_result)
      {:noreply, dispatch_and_finish(state)}
    else
      reason = {:result_task_mismatch, task.id, task_id}
      {:noreply, finish_fatal(state, :worker, reason, task)}
    end
  end

  def handle_info({:DOWN, reference, :process, worker, reason}, state)
      when is_map_key(state.worker_references, reference) do
    {task, in_flight} = Map.pop(state.in_flight, worker)

    state = %{
      state
      | worker_references: Map.delete(state.worker_references, reference),
        idle_workers: List.delete(state.idle_workers, worker),
        in_flight: in_flight
    }

    {:noreply, finish_fatal(state, :worker, reason, task)}
  end

  def handle_info(_message, state), do: {:noreply, state}

  @impl true
  def terminate(_reason, state) do
    stop_worker_pool(state)
    Store.close_traversal(state.store)
    :ok
  end

  defp prepare_index(config) do
    try do
      store = Store.new(config.root, fields: config.fields)
      scheduler = Scheduler.new(rank: config.rank, data: config.rank_data)

      root_task = %DirectoryTask{
        id: Store.root_id(store),
        path: config.root,
        depth: 0
      }

      case Scheduler.put_task(scheduler, root_task.id, root_task) do
        {:ok, scheduler} -> {:ok, store, scheduler}
        {:error, reason} -> {:error, reason}
      end
    rescue
      exception ->
        {:error, {:store_initialization_failed, exception}}
    end
  end

  defp dispatch_and_finish(%State{outcome: outcome} = state) when not is_nil(outcome), do: state

  defp dispatch_and_finish(state) do
    {scheduled, scheduler} =
      Scheduler.pop_many(state.scheduler, length(state.idle_workers))

    {idle_workers, in_flight} =
      Enum.reduce(
        scheduled,
        {state.idle_workers, state.in_flight},
        fn {_id, task, _rank}, {[worker | idle_workers], in_flight} ->
          :ok = Worker.assign(worker, self(), task)
          {idle_workers, Map.put(in_flight, worker, task)}
        end
      )

    state = %{
      state
      | scheduler: scheduler,
        idle_workers: idle_workers,
        in_flight: in_flight
    }

    if map_size(in_flight) == 0 and Scheduler.status(scheduler).size == 0 do
      finish_success(state)
    else
      state
    end
  end

  defp handle_worker_result(state, _task, {:ok, %DirectoryResult{} = result}) do
    counters = merge_counters(state.counters, result.counters)
    tasks = Enum.map(result.children, &{&1.id, &1})

    case Scheduler.put_tasks(state.scheduler, tasks) do
      {:ok, scheduler} -> %{state | scheduler: scheduler, counters: counters}
      {:error, reason} -> finish_fatal(%{state | counters: counters}, :ranking, reason, nil)
    end
  end

  defp handle_worker_result(state, task, {:directory_error, phase, reason}) do
    case PosixError.classify(reason) do
      {:recoverable, category} ->
        case Store.fail_directory(state.store, task.id, reason) do
          :ok ->
            failure = %DirectoryFailure{
              id: task.id,
              path: task.path,
              phase: phase,
              reason: reason,
              category: category
            }

            %{state | counters: record_directory_failure(state, failure)}

          {:error, store_reason} ->
            finish_fatal(state, :store, store_reason, task)
        end

      {:fatal, category} ->
        finish_fatal(state, :directory, {category, phase, reason}, task)
    end
  end

  defp handle_worker_result(state, task, {:store_error, reason}),
    do: finish_fatal(state, :store, reason, task)

  defp handle_worker_result(state, task, unexpected),
    do: finish_fatal(state, :worker, {:invalid_result, unexpected}, task)

  defp record_directory_failure(state, failure) do
    counters = state.counters

    samples =
      if length(counters.directory_failure_samples) < state.config.failure_sample_limit do
        [failure | counters.directory_failure_samples]
      else
        counters.directory_failure_samples
      end

    %{
      counters
      | directory_failure_counts:
          increment(counters.directory_failure_counts, failure.category, 1),
        directory_failure_reasons:
          increment(counters.directory_failure_reasons, failure.reason, 1),
        directory_failure_samples: samples
    }
  end

  defp finish_success(state) do
    state = stop_worker_pool(state)
    report = build_report(state, true)
    finalize(state, {:ok, %Result{store: state.store, report: report}})
  end

  defp finish_fatal(%State{outcome: outcome} = state, _kind, _reason, _task)
       when not is_nil(outcome),
       do: state

  defp finish_fatal(state, kind, reason, task) do
    state = stop_worker_pool(state)
    report = build_report(state, false)

    failure = %Failure{
      kind: kind,
      reason: reason,
      task: task,
      store: state.store,
      report: report
    }

    finalize(state, {:error, failure})
  end

  defp finalize(state, outcome) do
    Store.close_traversal(state.store)
    Enum.each(state.waiters, &GenServer.reply(&1, outcome))

    %{
      state
      | in_flight: %{},
        waiters: [],
        outcome: outcome
    }
  end

  defp start_worker_pool(state) do
    owner = self()

    {worker_references, idle_workers} =
      Enum.reduce(1..state.config.concurrency, {%{}, []}, fn _index, {references, workers} ->
        {worker, reference} =
          Worker.start(
            owner,
            state.store,
            state.config.fields,
            state.config.buffer_size,
            state.config.mount_policy
          )

        {Map.put(references, reference, worker), [worker | workers]}
      end)

    %{state | worker_references: worker_references, idle_workers: idle_workers}
  end

  defp stop_worker_pool(%State{worker_references: references} = state)
       when map_size(references) == 0,
       do: %{state | idle_workers: [], in_flight: %{}}

  defp stop_worker_pool(state) do
    owner = self()
    workers = Map.values(state.worker_references)
    Enum.each(workers, &Worker.stop(&1, owner))
    state = await_worker_shutdown(state, state.worker_references)

    %{state | worker_references: %{}, idle_workers: [], in_flight: %{}}
  end

  defp await_worker_shutdown(state, references) when map_size(references) == 0, do: state

  defp await_worker_shutdown(state, references) do
    receive do
      {:DOWN, reference, :process, _worker, _reason}
      when is_map_key(references, reference) ->
        await_worker_shutdown(state, Map.delete(references, reference))

      {:findex_indexer_worker_result, worker, task_id, result} ->
        state = settle_stopping_result(state, worker, task_id, result)
        await_worker_shutdown(state, references)
    end
  end

  defp settle_stopping_result(state, worker, task_id, result) do
    case Map.pop(state.in_flight, worker) do
      {%DirectoryTask{id: ^task_id} = task, in_flight} ->
        state = %{state | in_flight: in_flight}
        settle_stopping_task(state, task, result)

      {_task_or_nil, _in_flight} ->
        state
    end
  end

  defp settle_stopping_task(state, _task, {:ok, %DirectoryResult{} = result}) do
    %{state | counters: merge_counters(state.counters, result.counters)}
  end

  defp settle_stopping_task(state, task, {:directory_error, phase, reason}) do
    case PosixError.classify(reason) do
      {:recoverable, category} ->
        case Store.fail_directory(state.store, task.id, reason) do
          :ok ->
            failure = %DirectoryFailure{
              id: task.id,
              path: task.path,
              phase: phase,
              reason: reason,
              category: category
            }

            %{state | counters: record_directory_failure(state, failure)}

          {:error, _store_reason} ->
            state
        end

      {:fatal, _category} ->
        state
    end
  end

  defp settle_stopping_task(state, _task, _result), do: state

  defp build_report(state, allow_complete) do
    store_stats = Store.stats(state.store)
    counters = state.counters

    complete =
      allow_complete and store_stats.pending_directory_count == 0 and
        store_stats.failed_directory_count == 0 and counters.metadata_errors == 0

    elapsed = System.monotonic_time() - state.started_at

    %Report{
      root: state.config.root,
      complete?: complete,
      elapsed_milliseconds: System.convert_time_unit(elapsed, :native, :microsecond) / 1_000,
      entries: counters.entries,
      directories: counters.directories,
      regular_files: counters.regular_files,
      symlinks: counters.symlinks,
      other: counters.other,
      metadata_errors: counters.metadata_errors,
      metadata_error_counts: counters.metadata_error_counts,
      directory_failure_counts: counters.directory_failure_counts,
      directory_failure_reasons: counters.directory_failure_reasons,
      directory_failure_samples: Enum.reverse(counters.directory_failure_samples),
      skipped_mounts: counters.skipped_mounts,
      store: store_stats
    }
  end

  defp merge_counters(counters, addition) do
    %{
      counters
      | entries: counters.entries + addition.entries,
        directories: counters.directories + addition.directories,
        regular_files: counters.regular_files + addition.regular_files,
        symlinks: counters.symlinks + addition.symlinks,
        other: counters.other + addition.other,
        metadata_errors: counters.metadata_errors + addition.metadata_errors,
        metadata_error_counts:
          merge_counts(counters.metadata_error_counts, addition.metadata_error_counts),
        skipped_mounts: counters.skipped_mounts + addition.skipped_mounts
    }
  end

  defp merge_counts(left, right) do
    Map.merge(left, right, fn _key, left_count, right_count -> left_count + right_count end)
  end

  defp increment(counts, key, amount), do: Map.update(counts, key, amount, &(&1 + amount))

  defp configure(root, options) when is_binary(root) and is_list(options) do
    allowed = [
      :fields,
      :concurrency,
      :buffer_size,
      :ranking,
      :rank,
      :rank_data,
      :mount_policy,
      :failure_sample_limit
    ]

    unknown = Keyword.keys(options) -- allowed

    if unknown == [] do
      expanded_root = Path.expand(root)
      fields = Keyword.get(options, :fields, [:type])
      concurrency = Keyword.get(options, :concurrency, default_concurrency())
      buffer_size = Keyword.get(options, :buffer_size, @default_buffer_size)
      ranking = Keyword.get(options, :ranking, :default)
      rank = Keyword.get(options, :rank, Ranking.ranker(ranking, expanded_root))
      rank_data = Keyword.get(options, :rank_data, %{})
      mount_policy = Keyword.get(options, :mount_policy, :stay_on_filesystem)
      failure_sample_limit = Keyword.get(options, :failure_sample_limit, 100)

      with :ok <- validate_fields(fields),
           :ok <- validate_positive_integer(:concurrency, concurrency),
           :ok <- validate_buffer_size(buffer_size),
           :ok <- validate_ranking(ranking),
           :ok <- validate_ranking_options(ranking, options),
           :ok <- validate_rank(rank),
           :ok <- validate_rank_data(rank_data),
           :ok <- validate_mount_policy(mount_policy),
           :ok <- validate_non_negative_integer(:failure_sample_limit, failure_sample_limit) do
        fields =
          if mount_policy == :stay_on_filesystem do
            Enum.uniq(fields ++ [:mount_status])
          else
            fields
          end

        {:ok,
         %Config{
           root: expanded_root,
           fields: fields,
           concurrency: concurrency,
           buffer_size: buffer_size,
           ranking: ranking,
           rank: rank,
           rank_data: rank_data,
           mount_policy: mount_policy,
           failure_sample_limit: failure_sample_limit
         }}
      end
    else
      {:error, {:unknown_options, unknown}}
    end
  end

  defp configure(_root, _options), do: {:error, :invalid_arguments}

  defp validate_fields(fields) when is_list(fields) do
    cond do
      :type not in fields ->
        {:error, {:invalid_option, :fields, :type_required}}

      Enum.any?(fields, &(&1 not in Directory.supported_fields())) ->
        {:error, {:invalid_option, :fields, :unsupported_field}}

      true ->
        :ok
    end
  end

  defp validate_fields(fields), do: {:error, {:invalid_option, :fields, fields}}

  defp validate_positive_integer(_name, value) when is_integer(value) and value > 0, do: :ok
  defp validate_positive_integer(name, value), do: {:error, {:invalid_option, name, value}}

  defp validate_non_negative_integer(_name, value) when is_integer(value) and value >= 0,
    do: :ok

  defp validate_non_negative_integer(name, value),
    do: {:error, {:invalid_option, name, value}}

  defp validate_buffer_size(value)
       when is_integer(value) and value >= @minimum_buffer_size and value <= @maximum_buffer_size,
       do: :ok

  defp validate_buffer_size(value), do: {:error, {:invalid_option, :buffer_size, value}}

  defp validate_rank(rank) when is_function(rank, 2), do: :ok
  defp validate_rank(rank), do: {:error, {:invalid_option, :rank, rank}}

  defp validate_rank_data(data) when is_map(data), do: :ok
  defp validate_rank_data(data), do: {:error, {:invalid_option, :rank_data, data}}

  defp validate_ranking(ranking) when ranking in [:default, :name_biased, :macos], do: :ok
  defp validate_ranking(ranking), do: {:error, {:invalid_option, :ranking, ranking}}

  defp validate_ranking_options(:default, _options), do: :ok

  defp validate_ranking_options(ranking, options) when ranking in [:name_biased, :macos] do
    conflicting = Enum.filter([:rank, :rank_data], &Keyword.has_key?(options, &1))

    if conflicting == [],
      do: :ok,
      else: {:error, {:conflicting_options, ranking, conflicting}}
  end

  defp validate_mount_policy(policy) when policy in [:stay_on_filesystem, :cross], do: :ok
  defp validate_mount_policy(policy), do: {:error, {:invalid_option, :mount_policy, policy}}
end
