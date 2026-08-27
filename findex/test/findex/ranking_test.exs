defmodule Findex.RankingTest do
  use ExUnit.Case, async: true

  alias Findex.Indexer.DirectoryTask
  alias Findex.Ranking

  test "exposes only in-process named policies" do
    assert Ranking.policies() == [:default, :name_biased, :macos]
  end

  test "name-biased ranking deprioritizes generated subtrees" do
    rank = Ranking.ranker(:name_biased, "/scan")

    source = task(1, "/scan/source", 1)
    dependency = task(2, "/scan/target/dependency", 2)

    assert rank.(source, &read!/1) > rank.(dependency, &read!/1)
  end

  test "macOS ranking preserves root priors throughout each lineage" do
    rank = Ranking.ranker(:macos, "/")

    user_library = task(1, "/Users/me/Library", 3)
    application = task(2, "/Applications/Example.app", 2)
    system_library = task(3, "/System/Library", 2)
    ordinary = task(4, "/Volumes/data", 2)

    assert rank.(user_library, &read!/1) > rank.(application, &read!/1)
    assert rank.(application, &read!/1) > rank.(system_library, &read!/1)
    assert rank.(system_library, &read!/1) > rank.(ordinary, &read!/1)
  end

  test "macOS ranking keeps a strong cache prior below a scanned home directory" do
    rank = Ranking.ranker(:macos, "/Users/me")

    cache = task(1, "/Users/me/.cache/huggingface/hub", 3)
    source = task(2, "/Users/me/source/project", 2)

    assert rank.(cache, &read!/1) > rank.(source, &read!/1)
  end

  test "macOS rank keys have bounded depth" do
    rank = Ranking.ranker(:macos, "/")
    path = "/Users/me/a/b/c/d/e/f/g/h/i/j/k"
    {lineage, _depth, _id} = rank.(task(1, path, 13), &read!/1)

    assert length(lineage) == 8
  end

  defp task(id, path, depth) do
    %DirectoryTask{id: id, path: path, depth: depth, name: Path.basename(path)}
  end

  defp read!(key), do: raise("ranking unexpectedly read dynamic data: #{inspect(key)}")
end
