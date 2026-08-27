defmodule Findex.TraversalProfile do
  @moduledoc false

  alias Findex.{Batch, Directory, Indexer, Nif, Scheduler, Store}
  alias Findex.Indexer.Worker

  @profile_modules [Indexer, Worker, Directory, Batch, Store, Scheduler, Nif]
  @profile_patterns Enum.map(@profile_modules, &{&1, :_, :_})
  @top_functions 40

  def runtime(root, concurrency) do
    prepare()
    :erlang.garbage_collect()

    scheduler_wall_time_was_enabled =
      :erlang.system_flag(:scheduler_wall_time, true)

    microstate_was_enabled = :msacc.start()
    :msacc.reset()

    before = runtime_snapshot()
    started_at = System.monotonic_time()
    result = traverse(root, concurrency)
    elapsed_native = System.monotonic_time() - started_at
    after_snapshot = runtime_snapshot()
    microstates = :msacc.stats()

    unless microstate_was_enabled, do: :msacc.stop()

    unless scheduler_wall_time_was_enabled,
      do: :erlang.system_flag(:scheduler_wall_time, false)

    elapsed_us =
      System.convert_time_unit(elapsed_native, :native, :microsecond)

    print_result("runtime", result, elapsed_us)
    print_runtime_delta(before, after_snapshot, elapsed_us)
    print_scheduler_wall_time(before.scheduler_wall, after_snapshot.scheduler_wall)
    print_microstates(microstates)
  end

  def tprof(root, concurrency, type)
      when type in [:call_count, :call_time, :call_memory] do
    prepare()
    :erlang.garbage_collect()

    started_at = System.monotonic_time()

    {result, profile} =
      :tprof.profile(
        fn -> traverse(root, concurrency) end,
        %{
          type: type,
          pattern: @profile_patterns,
          report: :return,
          set_on_spawn: true
        }
      )

    elapsed_native = System.monotonic_time() - started_at

    elapsed_us =
      System.convert_time_unit(elapsed_native, :native, :microsecond)

    print_result(Atom.to_string(type), result, elapsed_us)

    %{all: {profile_type, total, lines}} =
      :tprof.inspect(profile, :total, {:measurement, :descending})

    IO.puts(
      "TPROF type=#{profile_type} total_measurement=#{total} " <>
        "shown=#{min(length(lines), @top_functions)} functions=#{length(lines)}"
    )

    :tprof.format({profile_type, total, Enum.take(lines, @top_functions)})
  end

  def repeat(root, concurrency, iterations) do
    prepare()

    results =
      for iteration <- 1..iterations do
        started_at = System.monotonic_time()
        result = traverse(root, concurrency)
        elapsed_native = System.monotonic_time() - started_at

        elapsed_ms =
          System.convert_time_unit(elapsed_native, :native, :microsecond) /
            1_000

        report = result.report

        IO.puts(
          "ITERATION number=#{iteration} elapsed_ms=#{Float.round(elapsed_ms, 3)} " <>
            "entries=#{report.entries} directories=#{report.directories}"
        )

        result
      end

    retained_native_bytes =
      Enum.reduce(results, 0, fn result, total ->
        total + Store.stats(result.store).native_bytes
      end)

    IO.puts(
      "REPEAT iterations=#{iterations} retained_stores=#{length(results)} " <>
        "retained_native_bytes=#{retained_native_bytes}"
    )

    Process.sleep(1_000)
  end

  defp prepare do
    Enum.each(@profile_modules, &Code.ensure_loaded!/1)
    Code.ensure_loaded!(:tprof)
    Code.ensure_loaded!(:msacc)
  end

  defp traverse(root, concurrency) do
    case Indexer.run(root,
           fields: [:type],
           concurrency: concurrency,
           mount_policy: :cross
         ) do
      {:ok, result} ->
        result

      {:error, failure} ->
        raise "profile traversal failed: #{inspect(failure)}"
    end
  end

  defp runtime_snapshot do
    %{
      reductions: statistics_total(:reductions),
      garbage_collection: :erlang.statistics(:garbage_collection),
      context_switches: statistics_total(:context_switches),
      io: io_totals(),
      memory: Map.new(:erlang.memory()),
      process_count: :erlang.system_info(:process_count),
      scheduler_wall: :erlang.statistics(:scheduler_wall_time_all)
    }
  end

  defp statistics_total(key) do
    {total, _since_last_call} = :erlang.statistics(key)
    total
  end

  defp io_totals do
    {{:input, input}, {:output, output}} = :erlang.statistics(:io)
    %{input: input, output: output}
  end

  defp print_result(mode, result, elapsed_us) do
    report = result.report
    store = Store.stats(result.store)

    IO.puts(
      "PROFILE mode=#{mode} elapsed_us=#{elapsed_us} " <>
        "entries=#{report.entries} directories=#{report.directories} " <>
        "regular_files=#{report.regular_files} symlinks=#{report.symlinks} " <>
        "other=#{report.other} directory_failures=#{store.failed_directory_count} " <>
        "metadata_errors=#{report.metadata_errors} native_bytes=#{store.native_bytes}"
    )
  end

  defp print_runtime_delta(before, after_snapshot, elapsed_us) do
    {collections_before, reclaimed_before, _} = before.garbage_collection
    {collections_after, reclaimed_after, _} = after_snapshot.garbage_collection

    IO.puts(
      "BEAM elapsed_us=#{elapsed_us} " <>
        "reductions=#{after_snapshot.reductions - before.reductions} " <>
        "context_switches=#{after_snapshot.context_switches - before.context_switches} " <>
        "gc_collections=#{collections_after - collections_before} " <>
        "gc_reclaimed_words=#{reclaimed_after - reclaimed_before} " <>
        "io_input_bytes=#{after_snapshot.io.input - before.io.input} " <>
        "io_output_bytes=#{after_snapshot.io.output - before.io.output} " <>
        "process_count_delta=#{after_snapshot.process_count - before.process_count} " <>
        "memory_total_delta=#{after_snapshot.memory.total - before.memory.total} " <>
        "memory_processes_delta=#{after_snapshot.memory.processes - before.memory.processes} " <>
        "memory_binary_delta=#{after_snapshot.memory.binary - before.memory.binary}"
    )
  end

  defp print_scheduler_wall_time(before, after_snapshot) do
    normal_schedulers = :erlang.system_info(:schedulers)
    dirty_cpu_schedulers = :erlang.system_info(:dirty_cpu_schedulers)
    dirty_io_schedulers = :erlang.system_info(:dirty_io_schedulers)

    deltas =
      scheduler_deltas(before, after_snapshot)
      |> Enum.map(fn {id, active, total} ->
        type =
          cond do
            id <= normal_schedulers ->
              :scheduler

            id <= normal_schedulers + dirty_cpu_schedulers ->
              :dirty_cpu_scheduler

            id <= normal_schedulers + dirty_cpu_schedulers + dirty_io_schedulers ->
              :dirty_io_scheduler

            true ->
              :unknown
          end

        {type, active, total}
      end)
      |> Enum.group_by(&elem(&1, 0))

    Enum.each([:scheduler, :dirty_cpu_scheduler, :dirty_io_scheduler, :unknown], fn type ->
      case Map.get(deltas, type) do
        nil ->
          :ok

        rows ->
          active = Enum.sum(Enum.map(rows, &elem(&1, 1)))
          total = Enum.sum(Enum.map(rows, &elem(&1, 2)))
          utilization = ratio(active, total)

          IO.puts(
            "SCHEDULER type=#{type} threads=#{length(rows)} active_us=#{to_us(active)} " <>
              "capacity_us=#{to_us(total)} utilization=#{Float.round(utilization, 4)}"
          )
      end
    end)
  end

  defp scheduler_deltas(before, after_snapshot) do
    before_by_id = Map.new(before, fn {id, active, total} -> {id, {active, total}} end)

    Enum.map(after_snapshot, fn {id, active, total} ->
      {before_active, before_total} = Map.fetch!(before_by_id, id)
      {id, active - before_active, total - before_total}
    end)
  end

  defp print_microstates(microstates) do
    system_runtime_us = :msacc.stats(:system_runtime, microstates)
    system_realtime_us = :msacc.stats(:system_realtime, microstates)

    IO.puts(
      "MSACC system_runtime_us=#{system_runtime_us} " <>
        "system_realtime_us=#{system_realtime_us}"
    )

    microstates
    |> Enum.group_by(& &1.type)
    |> Enum.sort_by(fn {type, _rows} -> type end)
    |> Enum.each(fn {type, rows} ->
      counters =
        Enum.reduce(rows, %{}, fn %{counters: current}, totals ->
          Map.merge(totals, current, fn _state, left, right ->
            add_counter(left, right)
          end)
        end)

      sleep_us = counter_time(Map.get(counters, :sleep, 0))

      runtime_us =
        counters
        |> Enum.reject(fn {state, _value} -> state == :sleep end)
        |> Enum.reduce(0, fn {_state, value}, total ->
          total + counter_time(value)
        end)

      states =
        counters
        |> Enum.reject(fn {state, value} ->
          state == :sleep or counter_time(value) == 0
        end)
        |> Enum.sort_by(fn {_state, value} -> -counter_time(value) end)
        |> Enum.map_join(",", fn {state, value} ->
          "#{state}:#{counter_time(value)}"
        end)

      IO.puts(
        "MSACC type=#{type} threads=#{length(rows)} runtime_us=#{runtime_us} " <>
          "sleep_us=#{sleep_us} utilization=#{Float.round(ratio(runtime_us, runtime_us + sleep_us), 4)} " <>
          "states=#{states}"
      )
    end)
  end

  defp add_counter({left_time, left_count}, {right_time, right_count}),
    do: {left_time + right_time, left_count + right_count}

  defp add_counter(left, right) when is_integer(left) and is_integer(right),
    do: left + right

  defp counter_time({time, _count}), do: time
  defp counter_time(time) when is_integer(time), do: time

  defp to_us(native_time),
    do: System.convert_time_unit(native_time, :native, :microsecond)

  defp ratio(_numerator, 0), do: 0.0
  defp ratio(numerator, denominator), do: numerator / denominator
end

parse_positive_integer = fn value, label ->
  case Integer.parse(value) do
    {integer, ""} when integer > 0 -> integer
    _other -> raise ArgumentError, "#{label} must be a positive integer"
  end
end

case System.argv() do
  ["--", mode, root, concurrency] ->
    concurrency = parse_positive_integer.(concurrency, "CONCURRENCY")

    case mode do
      "runtime" -> Findex.TraversalProfile.runtime(Path.expand(root), concurrency)
      "count" -> Findex.TraversalProfile.tprof(Path.expand(root), concurrency, :call_count)
      "time" -> Findex.TraversalProfile.tprof(Path.expand(root), concurrency, :call_time)
      "memory" -> Findex.TraversalProfile.tprof(Path.expand(root), concurrency, :call_memory)
      _other -> raise ArgumentError, "unknown profile mode: #{mode}"
    end

  [mode, root, concurrency] ->
    concurrency = parse_positive_integer.(concurrency, "CONCURRENCY")

    case mode do
      "runtime" -> Findex.TraversalProfile.runtime(Path.expand(root), concurrency)
      "count" -> Findex.TraversalProfile.tprof(Path.expand(root), concurrency, :call_count)
      "time" -> Findex.TraversalProfile.tprof(Path.expand(root), concurrency, :call_time)
      "memory" -> Findex.TraversalProfile.tprof(Path.expand(root), concurrency, :call_memory)
      _other -> raise ArgumentError, "unknown profile mode: #{mode}"
    end

  ["--", "repeat", root, concurrency, iterations] ->
    Findex.TraversalProfile.repeat(
      Path.expand(root),
      parse_positive_integer.(concurrency, "CONCURRENCY"),
      parse_positive_integer.(iterations, "ITERATIONS")
    )

  ["repeat", root, concurrency, iterations] ->
    Findex.TraversalProfile.repeat(
      Path.expand(root),
      parse_positive_integer.(concurrency, "CONCURRENCY"),
      parse_positive_integer.(iterations, "ITERATIONS")
    )

  _other ->
    IO.puts(
      :stderr,
      "usage: mix run bench/profile_traversal.exs -- " <>
        "(runtime|count|time|memory) ROOT CONCURRENCY\n" <>
        "       mix run bench/profile_traversal.exs -- " <>
        "repeat ROOT CONCURRENCY ITERATIONS"
    )

    System.halt(64)
end
