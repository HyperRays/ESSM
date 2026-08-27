defmodule FindexRust.BridgeTest do
  use ExUnit.Case, async: true

  alias FindexRust.{Bridge, Wire}

  setup do
    root =
      Path.join(
        System.tmp_dir!(),
        "findex-bridge-test-#{System.os_time(:nanosecond)}-#{System.unique_integer([:positive])}"
      )

    File.mkdir_p!(Path.join(root, "directory"))
    File.write!(Path.join(root, "file.txt"), "one")
    File.write!(Path.join(root, "directory/nested.txt"), "two")
    on_exit(fn -> File.rm_rf!(root) end)

    %{root: root}
  end

  test "retains live stores for bounded reads and explicit release", %{root: root} do
    events =
      run_bridge([
        %{"id" => 0, "op" => "ping"},
        start_request(1, root),
        %{"id" => 2, "op" => "index_status", "index_id" => 0},
        %{"id" => 3, "op" => "await_scan", "index_id" => 0},
        %{
          "id" => 4,
          "op" => "completed_directories",
          "index_id" => 0,
          "cursor" => 0,
          "limit" => 1
        },
        %{
          "id" => 5,
          "op" => "completed_directories",
          "index_id" => 0,
          "cursor" => 1,
          "limit" => 16
        },
        fetch_request(6, 0),
        fetch_request(7, 1),
        %{"id" => 8, "op" => "release_index", "index_id" => 0},
        %{"id" => 9, "op" => "index_status", "index_id" => 0},
        start_request(10, root),
        %{"id" => 11, "op" => "await_scan", "index_id" => 1},
        %{"id" => 12, "op" => "release_index", "index_id" => 1},
        %{"id" => 13, "op" => "shutdown"}
      ])

    ready = event(events, "ready")
    ping = response(events, 0)
    started = response(events, 1)
    live_status = response(events, 2)
    finished = response(events, 3)
    first_completions = response(events, 4)
    remaining_completions = response(events, 5)
    first_page = response(events, 6)
    second_page = response(events, 7)
    released = response(events, 8)
    unavailable = response(events, 9)
    second_started = response(events, 10)
    second_finished = response(events, 11)
    second_released = response(events, 12)
    shutdown = response(events, 13)

    assert ready == %{"event" => "ready", "pid" => System.pid(), "protocol" => 6}
    assert Enum.count(events, &(&1["event"] == "scan_finished")) == 2
    assert ping["result"]["pid"] == ready["pid"]
    assert started["result"] == %{"index_id" => 0, "root" => root}
    assert live_status["result"]["index_id"] == 0
    assert live_status["result"]["ranking"] == "default"
    assert live_status["result"]["state"] in ["running", "finished"]

    assert finished["result"]["outcome"] == "ok"
    assert finished["result"]["failure"] == nil
    assert finished["result"]["report"]["complete"]
    assert finished["result"]["report"]["entries"] == 3
    assert finished["result"]["report"]["store"]["directory_count"] == 2

    assert first_completions["result"] == %{
             "cursor" => 1,
             "directory_ids" => [0],
             "from_cursor" => 0,
             "index_id" => 0
           }

    assert remaining_completions["result"] == %{
             "cursor" => 2,
             "directory_ids" => [1],
             "from_cursor" => 1,
             "index_id" => 0
           }

    pages = [first_page["result"], second_page["result"]]
    assert Enum.map(pages, & &1["offset"]) == [0, 1]
    assert Enum.map(pages, & &1["next_offset"]) == [1, 2]
    assert Enum.map(pages, & &1["done"]) == [false, true]

    names = Enum.map(pages, fn page -> page["entries"] |> hd() |> get_in(["values", "name"]) end)
    assert MapSet.new(names) == MapSet.new(["directory", "file.txt"])

    assert Enum.all?(pages, fn page ->
             page["entries"] |> hd() |> get_in(["values", "file_id"]) |> is_integer()
           end)

    directory_row =
      Enum.find_value(pages, fn page ->
        case hd(page["entries"])["values"]["type"] do
          "directory" -> hd(page["entries"])
          _other -> nil
        end
      end)

    assert directory_row["child_directory_id"] == 1
    assert released["result"] == %{"index_id" => 0, "released" => true}
    assert unavailable["error"]["code"] == "unknown_index"

    assert second_started["result"]["index_id"] == 1
    assert second_finished["result"]["report"]["entries"] == 3
    assert second_released["result"]["released"]
    assert shutdown["result"] == %{"shutdown" => true}
  end

  test "rejects bad input without terminating the bridge" do
    events =
      run_bridge([
        "not-a-request-map",
        %{
          "id" => 0,
          "op" => "start_scan",
          "root" => "/tmp",
          "fields" => ["file_id"]
        },
        %{"id" => 1, "op" => "fetch_directory", "index_id" => 99},
        %{"id" => 2, "op" => "ping"},
        %{"id" => 3, "op" => "shutdown"}
      ])

    assert [ready, malformed, invalid_scan, unknown_index, ping, shutdown] = events
    assert ready["event"] == "ready"
    assert malformed["error"]["code"] == "invalid_request"

    assert invalid_scan["error"] == %{
             "code" => "invalid_request",
             "message" => "fields must contain type"
           }

    assert unknown_index["error"]["code"] == "unknown_index"
    assert ping["status"] == "ok"
    assert shutdown["status"] == "ok"
  end

  test "accepts named in-process ranking and rejects removed rank operations", %{root: root} do
    events =
      run_bridge([
        start_request(0, root) |> Map.put("ranking", "macos"),
        %{"id" => 1, "op" => "await_scan", "index_id" => 0},
        %{"id" => 2, "op" => "release_index", "index_id" => 0},
        %{
          "id" => 3,
          "op" => "submit_rank_batch",
          "index_id" => 0,
          "batch_id" => 0,
          "assignments" => [%{"directory_id" => 1, "rank" => <<1>>}]
        },
        %{
          "id" => 4,
          "op" => "rerank_pending",
          "index_id" => 0,
          "assignments" => []
        },
        start_request(5, root) |> Map.put("ranking", "external"),
        %{"id" => 6, "op" => "shutdown"}
      ])

    assert event(events, "ready")["protocol"] == 6
    assert response(events, 1)["result"]["outcome"] == "ok"
    assert response(events, 2)["result"]["released"]
    assert response(events, 3)["error"]["code"] == "unknown_operation"
    assert response(events, 4)["error"]["code"] == "unknown_operation"
    assert response(events, 5)["error"]["code"] == "invalid_request"
    assert response(events, 6)["status"] == "ok"
    refute Enum.any?(events, &(&1["event"] in ["rank_batch", "ranking_error"]))
  end

  defp start_request(id, root) do
    %{
      "id" => id,
      "op" => "start_scan",
      "root" => root,
      "fields" => ["type", "file_id"],
      "concurrency" => 2,
      "mount_policy" => "cross"
    }
  end

  defp fetch_request(id, offset) do
    %{
      "id" => id,
      "op" => "fetch_directory",
      "index_id" => 0,
      "directory_id" => 0,
      "offset" => offset,
      "limit" => 1
    }
  end

  defp run_bridge(requests), do: requests |> Enum.map(&Wire.encode/1) |> run_bridge_frames()

  defp run_bridge_frames(frames) do
    {:ok, input} = StringIO.open(IO.iodata_to_binary(frames), encoding: :latin1)
    {:ok, output} = StringIO.open("", encoding: :latin1)

    assert :ok = Bridge.run(input, output)
    {_remaining_input, encoded} = StringIO.contents(output)

    assert {:ok, events} = Wire.decode_all(encoded)
    events
  end

  defp response(events, id), do: Enum.find(events, &(&1["id"] == id))
  defp event(events, name), do: Enum.find(events, &(&1["event"] == name))
end
