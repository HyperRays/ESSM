defmodule Findex.Ranking do
  @moduledoc """
  Built-in traversal ranking policies evaluated inside the index coordinator.

  Policies are intentionally pure and cheap: a directory is ranked when its
  parent publishes it, and the result goes straight into `Findex.Scheduler`.
  No candidate staging, serialization, or out-of-process acknowledgement is
  involved. Applications which need mutable ranking inputs can still provide
  `Findex.Indexer` with their own `:rank` and `:rank_data` options.
  """

  alias Findex.Indexer.DirectoryTask

  @maximum_lineage_components 8

  @penalized_names MapSet.new([
                     ".git",
                     ".deps",
                     ".venv",
                     "__pycache__",
                     "_build",
                     "build",
                     "cache",
                     "config",
                     "deps",
                     "dist",
                     "git",
                     "node_modules",
                     "target",
                     "vendor"
                   ])

  @type policy :: :default | :name_biased | :macos

  @doc "Returns the named policies accepted by `Findex.Indexer`."
  @spec policies() :: [policy()]
  def policies, do: [:default, :name_biased, :macos]

  @doc false
  @spec ranker(policy(), Path.t()) :: (DirectoryTask.t(), Findex.Scheduler.reader() -> term())
  def ranker(:default, _root), do: fn _task, _read -> 0 end
  def ranker(:name_biased, _root), do: &name_biased/2

  def ranker(:macos, root) do
    root = Path.expand(root)
    root_components = normalized_components(root)
    fn task, read -> macos(task, read, root, root_components) end
  end

  def ranker(_policy, _root), do: nil

  @doc "Deprioritizes generated, dependency, cache, and configuration trees."
  @spec name_biased(DirectoryTask.t(), Findex.Scheduler.reader()) :: tuple()
  def name_biased(%DirectoryTask{} = task, _read) do
    penalized? = Enum.any?(Path.split(task.path), &penalized_name?/1)
    {if(penalized?, do: -1, else: 0), -task.depth, -task.id}
  end

  @doc false
  @spec macos(DirectoryTask.t(), Findex.Scheduler.reader(), Path.t()) :: tuple()
  def macos(%DirectoryTask{} = task, read, root) do
    root = Path.expand(root)
    macos(task, read, root, normalized_components(root))
  end

  defp macos(%DirectoryTask{depth: 0} = task, _read, _root, _root_components),
    do: {[], 0, -task.id}

  defp macos(%DirectoryTask{} = task, _read, root, root_components) do
    relative = root |> relative_components(task.path) |> downcase_components()
    relative_tuple = List.to_tuple(relative)
    absolute_tuple = List.to_tuple(root_components ++ relative)
    absolute_offset = length(root_components)

    lineage =
      relative
      |> Enum.with_index()
      |> Enum.map(fn {_component, relative_index} ->
        macos_edge_boost(
          root,
          relative_tuple,
          absolute_tuple,
          relative_index,
          absolute_offset + relative_index
        )
      end)
      |> bounded_lineage()

    {lineage, -task.depth, -task.id}
  end

  defp penalized_name?(name) do
    case safe_downcase(name) do
      nil -> false
      name -> MapSet.member?(@penalized_names, name)
    end
  end

  defp relative_components(root, path) do
    case Path.relative_to(path, root) do
      "." -> []
      relative -> Path.split(relative)
    end
  end

  defp bounded_lineage(lineage) when length(lineage) <= @maximum_lineage_components,
    do: lineage

  defp bounded_lineage(lineage) do
    Enum.take(lineage, @maximum_lineage_components - 1) ++ [List.last(lineage)]
  end

  defp macos_edge_boost(root, relative, absolute, relative_index, absolute_index) do
    leaf = elem(relative, relative_index)

    max(
      root_boost(root, relative_index, leaf),
      max(
        leaf_boost(leaf),
        contextual_boost(relative, absolute, relative_index, absolute_index, leaf)
      )
    )
  end

  defp root_boost("/", 0, leaf) do
    case leaf do
      "users" -> 16
      "applications" -> 10
      "opt" -> 10
      "system" -> 8
      "private" -> 7
      "library" -> 6
      "usr" -> 3
      _other -> 1
    end
  end

  defp root_boost(_root, _relative_index, _leaf), do: 1

  defp leaf_boost(leaf) do
    cond do
      leaf == ".cache" -> 16
      leaf in ["deriveddata", "coresimulator", "mobilesync"] -> 14
      leaf == ".minikube" -> 12
      leaf in [".docker", ".elan", ".ghcup", ".ghc-wasm", ".ollama"] -> 10
      leaf in [".local", "caches"] -> 9
      leaf in [".cabal", "node_modules", "target", "pods"] -> 8
      leaf in [".gradle", ".m2"] -> 7
      leaf in [".vscode", ".venv", "venv", "_build"] -> 6
      leaf in [".lmstudio", ".npm", ".opam", ".rustup"] -> 5
      leaf in ["downloads", "movies", "pictures", "music"] -> 5
      leaf in [".codex", ".claude", "build", "dist"] -> 4
      leaf == ".cargo" -> 3
      true -> 1
    end
  end

  defp contextual_boost(relative, absolute, relative_index, absolute_index, leaf) do
    cond do
      home_child?(absolute, absolute_index, "library") ->
        12

      home_child?(absolute, absolute_index, "documents") ->
        6

      exact_prefix?(relative, relative_index, ["system", "library"]) ->
        14

      exact_prefix?(relative, relative_index, ["system", "library", "assetsv2"]) ->
        12

      exact_prefix?(relative, relative_index, ["system", "library", "privateframeworks"]) ->
        8

      exact_prefix?(relative, relative_index, ["private", "var"]) ->
        12

      exact_prefix?(relative, relative_index, ["private", "tmp"]) ->
        7

      exact_prefix?(relative, relative_index, ["opt", "homebrew"]) ->
        12

      exact_prefix?(relative, relative_index, ["opt", "miniconda3"]) ->
        7

      suffix_at?(absolute, absolute_index, ["homebrew", "caskroom"]) ->
        10

      suffix_at?(absolute, absolute_index, ["homebrew", "cellar"]) ->
        10

      suffix_at?(absolute, absolute_index, ["miniconda3", "envs"]) ->
        9

      suffix_at?(absolute, absolute_index, ["miniconda3", "pkgs"]) ->
        9

      suffix_at?(absolute, absolute_index, [
        "library",
        "application support",
        "mobilesync",
        "backup"
      ]) ->
        16

      suffix_at?(absolute, absolute_index, ["library", "developer", "coresimulator"]) ->
        16

      suffix_at?(absolute, absolute_index, [
        "library",
        "developer",
        "xcode",
        "deriveddata"
      ]) ->
        16

      suffix_at?(absolute, absolute_index, ["library", "application support"]) ->
        14

      suffix_at?(absolute, absolute_index, ["library", "containers"]) ->
        12

      suffix_at?(absolute, absolute_index, ["library", "caches"]) ->
        10

      suffix_at?(absolute, absolute_index, ["library", "group containers"]) ->
        8

      suffix_at?(absolute, absolute_index, ["library", "mobile documents"]) ->
        8

      suffix_at?(absolute, absolute_index, ["library", "mail"]) ->
        9

      suffix_at?(absolute, absolute_index, ["library", "messages", "attachments"]) ->
        9

      suffix_at?(absolute, absolute_index, ["private", "var", "vm"]) ->
        9

      is_binary(leaf) and String.ends_with?(leaf, ".photoslibrary") ->
        14

      absolute_index >= 1 and elem(absolute, 0) == "applications" and
          String.ends_with?(leaf, ".app") ->
        3

      leaf in ["hub", "blobs"] and
          contains_at?(absolute, absolute_index, [".cache", "huggingface"]) ->
        10

      leaf in ["models", "blobs"] and contains_at?(absolute, absolute_index, [".lmstudio"]) ->
        10

      true ->
        1
    end
  end

  defp home_child?(absolute, 2, leaf) do
    elem(absolute, 0) == "users" and elem(absolute, 2) == leaf
  end

  defp home_child?(_absolute, _absolute_index, _leaf), do: false

  defp exact_prefix?(components, index, expected) do
    tuple_size(components) >= length(expected) and index == length(expected) - 1 and
      suffix_at?(components, index, expected)
  end

  defp suffix_at?(components, index, suffix) do
    suffix_length = length(suffix)

    if index + 1 < suffix_length do
      false
    else
      start = index + 1 - suffix_length

      suffix
      |> Enum.with_index(start)
      |> Enum.all?(fn {component, component_index} ->
        elem(components, component_index) == component
      end)
    end
  end

  defp contains_at?(components, index, pattern) do
    pattern_length = length(pattern)
    last_start = index + 1 - pattern_length

    last_start >= 0 and
      Enum.any?(0..last_start, fn start ->
        pattern
        |> Enum.with_index(start)
        |> Enum.all?(fn {component, component_index} ->
          elem(components, component_index) == component
        end)
      end)
  end

  defp normalized_components(path) do
    path
    |> Path.split()
    |> Enum.reject(&(&1 == "/"))
    |> downcase_components()
  end

  defp downcase_components(components) do
    Enum.map(components, &(safe_downcase(&1) || ""))
  end

  defp safe_downcase(value) when is_binary(value) do
    if String.valid?(value), do: String.downcase(value)
  end

  defp safe_downcase(_value), do: nil
end
