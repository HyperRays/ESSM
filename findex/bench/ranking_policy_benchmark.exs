defmodule Findex.RankingPolicyBenchmark do
  @moduledoc false

  alias Findex.{Batch, Indexer, Store}
  alias Findex.Store.DirectoryNode

  @completion_batch_size 1_024
  @poll_milliseconds 1
  @thresholds [25, 50, 75, 90, 95]

  def run(root, trials) do
    root = Path.expand(root)

    IO.puts(
      "benchmark root=#{inspect(root)} trials=#{trials} " <>
        "policies=default,macos completion_batch=#{@completion_batch_size}"
    )

    results =
      Enum.flat_map(1..trials, fn trial ->
        policies = if rem(trial, 2) == 1, do: [:macos, :default], else: [:default, :macos]

        Enum.map(policies, fn policy ->
          result = measure(root, trial, policy)
          print_run(result)
          :erlang.garbage_collect()
          Process.sleep(100)
          result
        end)
      end)

    print_summaries(results)
  end

  defp measure(root, trial, policy) do
    started_at = System.monotonic_time()

    {:ok, indexer} =
      Indexer.start_link(root,
        fields: [:type, :allocated_size],
        ranking: policy,
        failure_sample_limit: 0
      )

    store = Indexer.store(indexer)
    waiter = Task.async(fn -> Indexer.await(indexer) end)

    try do
      {events, outcome} = consume(store, waiter, started_at, 0, [], nil)
      report = outcome_report(outcome)
      wall_milliseconds = elapsed_milliseconds(started_at)
      discovery = discovery_metrics(events, wall_milliseconds)

      Map.merge(discovery, %{
        trial: trial,
        policy: policy,
        total_milliseconds: wall_milliseconds,
        index_milliseconds: report.elapsed_milliseconds,
        entries: report.entries,
        directories: report.store.directory_count,
        failures: report.store.failed_directory_count,
        complete?: report.complete?
      })
    after
      if Process.alive?(indexer), do: GenServer.stop(indexer, :normal)
    end
  end

  defp consume(store, waiter, started_at, cursor, events, outcome) do
    {:ok, directory_ids, next_cursor} =
      Store.completed_since(store, cursor, limit: @completion_batch_size)

    observed_at = elapsed_milliseconds(started_at)

    events =
      Enum.reduce(directory_ids, events, fn directory_id, events ->
        [{observed_at, immediate_allocated_bytes(store, directory_id)} | events]
      end)

    {waiter, outcome} = poll_outcome(waiter, outcome)

    if outcome != nil and next_cursor == completion_count(outcome) do
      {Enum.reverse(events), outcome}
    else
      if directory_ids == [], do: Process.sleep(@poll_milliseconds)
      consume(store, waiter, started_at, next_cursor, events, outcome)
    end
  end

  defp immediate_allocated_bytes(store, directory_id) do
    case Store.fetch_directory(store, directory_id) do
      {:ok, %DirectoryNode{entries: %Batch{} = batch}} ->
        Batch.reduce(batch, 0, fn index, total ->
          case {Batch.value(batch, :type, index), Batch.value(batch, :allocated_size, index)} do
            {:regular, bytes} when is_integer(bytes) and bytes > 0 -> total + bytes
            _other -> total
          end
        end)

      {:ok, %DirectoryNode{entries: nil}} ->
        0
    end
  end

  defp poll_outcome(nil, outcome), do: {nil, outcome}

  defp poll_outcome(waiter, nil) do
    case Task.yield(waiter, 0) do
      nil -> {waiter, nil}
      {:ok, outcome} -> {nil, outcome}
      {:exit, reason} -> raise "index waiter exited: #{inspect(reason)}"
    end
  end

  defp poll_outcome(waiter, outcome), do: {waiter, outcome}

  defp completion_count({:ok, result}), do: result.report.store.completion_count
  defp completion_count({:error, failure}), do: failure.report.store.completion_count
  defp outcome_report({:ok, result}), do: result.report
  defp outcome_report({:error, failure}), do: failure.report

  defp discovery_metrics(events, total_milliseconds) do
    total_bytes = Enum.reduce(events, 0, fn {_time, bytes}, total -> total + bytes end)
    directory_count = length(events)

    thresholds =
      Enum.reduce(@thresholds, %{}, fn percentage, metrics ->
        target = total_bytes * percentage / 100
        {time, position} = threshold_event(events, target, 0, 0)

        metrics
        |> Map.put(String.to_atom("p#{percentage}_milliseconds"), time)
        |> Map.put(
          String.to_atom("p#{percentage}_directory_fraction"),
          position / directory_count
        )
      end)

    Map.merge(thresholds, %{
      allocated_bytes: total_bytes,
      order_auc: order_auc(events, total_bytes),
      time_auc: time_auc(events, total_bytes, total_milliseconds)
    })
  end

  defp threshold_event([{time, bytes} | events], target, discovered, position) do
    discovered = discovered + bytes
    position = position + 1

    if discovered >= target do
      {time, position}
    else
      threshold_event(events, target, discovered, position)
    end
  end

  defp threshold_event([], _target, _discovered, position), do: {0.0, position}

  defp order_auc(_events, 0), do: 0.0

  defp order_auc(events, total_bytes) do
    {area, _discovered} =
      Enum.reduce(events, {0.0, 0}, fn {_time, bytes}, {area, discovered} ->
        before = discovered / total_bytes
        discovered = discovered + bytes
        after_value = discovered / total_bytes
        {area + (before + after_value) / 2, discovered}
      end)

    area / length(events)
  end

  defp time_auc(_events, 0, _total_milliseconds), do: 0.0

  defp time_auc(events, total_bytes, total_milliseconds) do
    {area, previous_time, coverage} =
      Enum.reduce(events, {0.0, 0.0, 0.0}, fn {time, bytes}, {area, previous_time, coverage} ->
        time = min(time, total_milliseconds)
        area = area + coverage * max(time - previous_time, 0.0)
        {area, time, coverage + bytes / total_bytes}
      end)

    area = area + coverage * max(total_milliseconds - previous_time, 0.0)
    area / max(total_milliseconds, 0.001)
  end

  defp print_run(result) do
    threshold_text =
      Enum.map_join(@thresholds, " ", fn percentage ->
        milliseconds = Map.fetch!(result, String.to_atom("p#{percentage}_milliseconds"))
        fraction = Map.fetch!(result, String.to_atom("p#{percentage}_directory_fraction"))
        "p#{percentage}_ms=#{format(milliseconds)} p#{percentage}_dirs=#{format(fraction)}"
      end)

    IO.puts(
      "RUN trial=#{result.trial} policy=#{result.policy} " <>
        "total_ms=#{format(result.total_milliseconds)} " <>
        "index_ms=#{format(result.index_milliseconds)} " <>
        "order_auc=#{format(result.order_auc)} time_auc=#{format(result.time_auc)} " <>
        threshold_text <>
        " entries=#{result.entries} directories=#{result.directories} " <>
        "bytes=#{result.allocated_bytes} failures=#{result.failures} complete=#{result.complete?}"
    )
  end

  defp print_summaries(results) do
    metrics =
      [:total_milliseconds, :index_milliseconds, :order_auc, :time_auc] ++
        Enum.flat_map(@thresholds, fn percentage ->
          [
            String.to_atom("p#{percentage}_milliseconds"),
            String.to_atom("p#{percentage}_directory_fraction")
          ]
        end)

    Enum.each([:default, :macos], fn policy ->
      policy_results = Enum.filter(results, &(&1.policy == policy))

      Enum.each(metrics, fn metric ->
        values = Enum.map(policy_results, &Map.fetch!(&1, metric))
        summary = summarize(values)

        IO.puts(
          "SUMMARY policy=#{policy} metric=#{metric} n=#{length(values)} " <>
            "mean=#{format(summary.mean)} median=#{format(summary.median)} " <>
            "sd=#{format(summary.sd)} min=#{format(summary.minimum)} " <>
            "max=#{format(summary.maximum)} ci95_low=#{format(summary.ci95_low)} " <>
            "ci95_high=#{format(summary.ci95_high)}"
        )
      end)
    end)

    print_paired(results, metrics)
  end

  defp print_paired(results, metrics) do
    by_trial_policy = Map.new(results, &{{&1.trial, &1.policy}, &1})
    trials = results |> Enum.map(& &1.trial) |> Enum.uniq() |> Enum.sort()

    Enum.each(metrics, fn metric ->
      pairs =
        Enum.map(trials, fn trial ->
          default = by_trial_policy |> Map.fetch!({trial, :default}) |> Map.fetch!(metric)
          macos = by_trial_policy |> Map.fetch!({trial, :macos}) |> Map.fetch!(metric)
          {default, macos}
        end)

      differences = Enum.map(pairs, fn {default, macos} -> macos - default end)
      summary = summarize(differences)
      {macos_wins, default_wins} = paired_wins(pairs, metric)
      sign_p = sign_test(macos_wins, default_wins)
      effect_size = if summary.sd == 0.0, do: 0.0, else: summary.mean / summary.sd

      IO.puts(
        "PAIRED metric=#{metric} n=#{length(pairs)} macos_minus_default_mean=#{format(summary.mean)} " <>
          "median=#{format(summary.median)} ci95_low=#{format(summary.ci95_low)} " <>
          "ci95_high=#{format(summary.ci95_high)} cohen_dz=#{format(effect_size)} " <>
          "macos_wins=#{macos_wins} default_wins=#{default_wins} sign_p=#{format(sign_p)}"
      )
    end)
  end

  defp paired_wins(pairs, metric) when metric in [:order_auc, :time_auc] do
    Enum.reduce(pairs, {0, 0}, fn
      {default, macos}, {macos_wins, default_wins} when macos > default ->
        {macos_wins + 1, default_wins}

      {default, macos}, {macos_wins, default_wins} when default > macos ->
        {macos_wins, default_wins + 1}

      _tie, wins ->
        wins
    end)
  end

  defp paired_wins(pairs, _lower_is_better_metric) do
    Enum.reduce(pairs, {0, 0}, fn
      {default, macos}, {macos_wins, default_wins} when macos < default ->
        {macos_wins + 1, default_wins}

      {default, macos}, {macos_wins, default_wins} when default < macos ->
        {macos_wins, default_wins + 1}

      _tie, wins ->
        wins
    end)
  end

  defp summarize(values) do
    count = length(values)
    mean = Enum.sum(values) / count
    sorted = Enum.sort(values)
    median = median(sorted)

    variance =
      if count > 1 do
        Enum.reduce(values, 0.0, fn value, total -> total + :math.pow(value - mean, 2) end) /
          (count - 1)
      else
        0.0
      end

    sd = :math.sqrt(variance)
    margin = t_critical_95(count - 1) * sd / :math.sqrt(count)

    %{
      mean: mean,
      median: median,
      sd: sd,
      minimum: hd(sorted),
      maximum: List.last(sorted),
      ci95_low: mean - margin,
      ci95_high: mean + margin
    }
  end

  defp median(sorted) do
    count = length(sorted)
    middle = div(count, 2)

    if rem(count, 2) == 1 do
      Enum.at(sorted, middle)
    else
      (Enum.at(sorted, middle - 1) + Enum.at(sorted, middle)) / 2
    end
  end

  defp sign_test(macos_wins, default_wins) do
    count = macos_wins + default_wins
    tail = min(macos_wins, default_wins)

    probability =
      Enum.reduce(0..tail, 0.0, fn successes, total ->
        total + binomial(count, successes) / :math.pow(2, count)
      end)

    min(1.0, 2 * probability)
  end

  defp binomial(_n, 0), do: 1

  defp binomial(n, k) do
    Enum.reduce(1..k, 1, fn index, result -> div(result * (n - index + 1), index) end)
  end

  defp t_critical_95(degrees_of_freedom) do
    case degrees_of_freedom do
      1 -> 12.706
      2 -> 4.303
      3 -> 3.182
      4 -> 2.776
      5 -> 2.571
      6 -> 2.447
      7 -> 2.365
      8 -> 2.306
      9 -> 2.262
      value when value <= 12 -> 2.179
      value when value <= 15 -> 2.131
      value when value <= 20 -> 2.086
      value when value <= 30 -> 2.042
      _value -> 1.96
    end
  end

  defp elapsed_milliseconds(started_at) do
    elapsed = System.monotonic_time() - started_at
    System.convert_time_unit(elapsed, :native, :microsecond) / 1_000
  end

  defp format(value) when is_float(value), do: :erlang.float_to_binary(value, decimals: 6)
  defp format(value), do: to_string(value)
end

case System.argv() do
  arguments when length(arguments) in [2, 3] ->
    [root, trials] = if hd(arguments) == "--", do: tl(arguments), else: arguments

    case Integer.parse(trials) do
      {trials, ""} when trials >= 2 -> Findex.RankingPolicyBenchmark.run(root, trials)
      _other -> raise "TRIALS must be an integer of at least 2"
    end

  _other ->
    IO.puts(:stderr, "usage: mix run bench/ranking_policy_benchmark.exs -- ROOT TRIALS")
    System.halt(64)
end
