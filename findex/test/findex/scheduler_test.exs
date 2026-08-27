defmodule Findex.SchedulerTest do
  use ExUnit.Case, async: true

  alias Findex.Scheduler

  test "pops greatest ranks in batches and preserves insertion order for ties" do
    scheduler = new_scheduler(fn task, _read -> task.priority end)

    assert {:ok, scheduler} =
             Scheduler.put_tasks(scheduler,
               first: %{priority: 10},
               second: %{priority: 10},
               highest: %{priority: 20}
             )

    assert {:ok, :highest, %{priority: 20}, 20} = Scheduler.peek(scheduler)

    assert {[{:highest, %{priority: 20}, 20}, {:first, %{priority: 10}, 10}], scheduler} =
             Scheduler.pop_many(scheduler, 2)

    assert {{:ok, :second, %{priority: 10}, 10}, scheduler} = Scheduler.pop(scheduler)
    assert {:empty, scheduler} = Scheduler.pop(scheduler)
    assert Scheduler.status(scheduler).size == 0
  end

  test "batch insertion is ordered and atomic" do
    rank = fn task, _read ->
      if task == :invalid, do: raise("invalid task"), else: task
    end

    scheduler = new_scheduler(rank)
    assert {:ok, scheduler} = Scheduler.put_tasks(scheduler, first: 10, second: 10)
    assert {{:ok, :first, 10, 10}, scheduler} = Scheduler.pop(scheduler)
    assert {:ok, :second, 10, 10} = Scheduler.peek(scheduler)

    assert {:error, {:rank_error, :broken, {:error, %RuntimeError{}, _stacktrace}}} =
             Scheduler.put_tasks(scheduler, second: 20, broken: :invalid)

    assert %{size: 1} = Scheduler.status(scheduler)
    assert {:ok, :second, 10, 10} = Scheduler.peek(scheduler)

    assert {:error, {:invalid_task, :not_a_pair}} =
             Scheduler.put_tasks(scheduler, [{:third, 30}, :not_a_pair])

    assert %{size: 1} = Scheduler.status(scheduler)
  end

  test "tracks dynamic dependencies and reranks only affected tasks" do
    test_process = self()

    rank = fn task, read ->
      send(test_process, {:ranked, task.id})

      case read.(:policy) do
        :score -> read.({:score, task.id})
        :depth -> read.({:depth, task.id})
      end
    end

    scheduler =
      new_scheduler(rank, %{
        {:score, :a} => 1,
        {:score, :b} => 2,
        {:depth, :a} => 20,
        {:depth, :b} => 10,
        policy: :score
      })

    assert {:ok, scheduler} = Scheduler.put_task(scheduler, :a, %{id: :a})
    assert_receive {:ranked, :a}
    assert {:ok, scheduler} = Scheduler.put_task(scheduler, :b, %{id: :b})
    assert_receive {:ranked, :b}

    assert {:ok, %{dependencies: dependencies}} = Scheduler.task_info(scheduler, :a)
    assert dependencies == MapSet.new([:policy, {:score, :a}])

    assert {:ok, scheduler} = Scheduler.put_data(scheduler, {:score, :a}, 30)
    assert_receive {:ranked, :a}
    refute_receive {:ranked, :b}
    assert {:ok, :a, %{id: :a}, 30} = Scheduler.peek(scheduler)

    assert {:ok, scheduler} = Scheduler.put_data(scheduler, :unobserved, :value)
    refute_receive {:ranked, _id}

    assert {:ok, scheduler} = Scheduler.put_data(scheduler, :policy, :depth)

    reranked =
      for _index <- 1..2 do
        assert_receive {:ranked, id}
        id
      end

    assert MapSet.new(reranked) == MapSet.new([:a, :b])

    assert {:ok, %{dependencies: dependencies}} = Scheduler.task_info(scheduler, :a)
    assert dependencies == MapSet.new([:policy, {:depth, :a}])

    assert {:ok, scheduler} = Scheduler.put_data(scheduler, {:score, :a}, 100)
    refute_receive {:ranked, _id}
    assert {:ok, :a, %{id: :a}, 20} = Scheduler.peek(scheduler)
  end

  test "applies multi-key changes atomically and evaluates each affected task once" do
    test_process = self()

    rank = fn task, read ->
      send(test_process, {:ranked, task.id})
      read.({:left, task.id}) + read.({:right, task.id})
    end

    scheduler =
      new_scheduler(rank, %{
        {:left, :a} => 1,
        {:right, :a} => 2,
        {:left, :b} => 3,
        {:right, :b} => 4
      })

    assert {:ok, scheduler} = Scheduler.put_tasks(scheduler, a: %{id: :a}, b: %{id: :b})
    assert_receive {:ranked, :a}
    assert_receive {:ranked, :b}

    assert {:ok, scheduler} =
             Scheduler.put_data(scheduler, %{
               {:left, :a} => 10,
               {:right, :a} => 20,
               {:left, :b} => 30
             })

    reranked =
      for _index <- 1..2 do
        assert_receive {:ranked, id}
        id
      end

    assert MapSet.new(reranked) == MapSet.new([:a, :b])
    refute_receive {:ranked, _id}
    assert Scheduler.status(scheduler).revision == 1
    assert {:ok, :b, %{id: :b}, 34} = Scheduler.peek(scheduler)
  end

  test "a failed rerank leaves the original value unchanged" do
    scheduler = new_scheduler(fn _task, read -> read.(:required) end, %{required: 7})
    assert {:ok, scheduler} = Scheduler.put_task(scheduler, :task, :data)
    assert {:ok, :task, :data, 7} = Scheduler.peek(scheduler)

    assert {:error, {:rank_error, :task, {:error, %KeyError{}, _stacktrace}}} =
             Scheduler.delete_data(scheduler, :required)

    assert {:ok, 7} = Scheduler.fetch_data(scheduler, :required)
    assert %{revision: 0, size: 1} = Scheduler.status(scheduler)
    assert {:ok, :task, :data, 7} = Scheduler.peek(scheduler)
  end

  test "replacing a task updates its dependencies without changing tie order" do
    test_process = self()

    rank = fn task, read ->
      send(test_process, {:ranked, task.id})
      read.(task.key)
    end

    scheduler = new_scheduler(rank, %{old: 1, new: 1})

    assert {:ok, scheduler} =
             Scheduler.put_tasks(scheduler,
               first: %{id: :first, key: :old},
               second: %{id: :second, key: :new}
             )

    assert_receive {:ranked, :first}
    assert_receive {:ranked, :second}

    assert {:ok, scheduler} =
             Scheduler.put_task(scheduler, :first, %{id: :first, key: :new})

    assert_receive {:ranked, :first}
    assert {:ok, scheduler} = Scheduler.put_data(scheduler, :old, 100)
    refute_receive {:ranked, _id}

    assert {[{:first, %{id: :first, key: :new}, 1}, {:second, _, 1}], scheduler} =
             Scheduler.pop_many(scheduler, 2)

    assert Scheduler.status(scheduler).size == 0
  end

  test "unchanged values do not advance the revision or rerank tasks" do
    test_process = self()

    rank = fn _task, read ->
      send(test_process, :ranked)
      read.(:value)
    end

    scheduler = new_scheduler(rank, %{value: 42})
    assert {:ok, scheduler} = Scheduler.put_task(scheduler, :task, :data)
    assert_receive :ranked

    assert {:ok, same_scheduler} = Scheduler.put_data(scheduler, :value, 42)
    assert same_scheduler === scheduler
    refute_receive :ranked
    assert Scheduler.status(same_scheduler).revision == 0
  end

  test "rank failures do not replace an existing task" do
    rank = fn task, _read ->
      if task == :invalid, do: raise("invalid task"), else: task
    end

    scheduler = new_scheduler(rank)
    assert {:ok, scheduler} = Scheduler.put_task(scheduler, :task, 10)

    assert {:error, {:rank_error, :task, {:error, %RuntimeError{}, _stacktrace}}} =
             Scheduler.put_task(scheduler, :task, :invalid)

    assert {:ok, :task, 10, 10} = Scheduler.peek(scheduler)
  end

  test "validates construction options" do
    assert_raise KeyError, fn -> Scheduler.new([]) end
    assert_raise ArgumentError, fn -> Scheduler.new(rank: :not_a_function) end
    assert_raise ArgumentError, fn -> Scheduler.new(rank: fn _, _ -> 0 end, data: []) end
  end

  defp new_scheduler(rank, data \\ %{}) do
    Scheduler.new(rank: rank, data: data)
  end
end
