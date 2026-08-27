defmodule Findex.Nif do
  @moduledoc """
  Native macOS filesystem operations.
  """

  @on_load :load_nif

  def load_nif do
    nif_path =
      :findex
      |> :code.priv_dir()
      |> to_string()
      |> Path.join("findex_nif")

    :erlang.load_nif(nif_path, 0)
  end

  @doc "Prints `Hello, world!` from C and returns `:ok`."
  def hello, do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def open_directory(_path, _fields, _format, _path_policy),
    do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def next_directory_batch(_cursor, _buffer_size),
    do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def close_directory(_cursor), do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def index_store_new(_root_path, _fields), do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def index_store_open_directory(_store, _directory_id, _fields, _format),
    do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def index_store_close_traversal(_store), do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def index_store_publish(_store, _directory_id, _batches, _child_entry_indices),
    do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def index_store_scan_and_publish(
        _store,
        _directory_id,
        _path,
        _fields,
        _buffer_size,
        _mount_policy
      ),
      do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def index_store_fail(_store, _directory_id, _reason),
    do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def index_store_completed_since(_store, _cursor, _limit),
    do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def index_store_fetch(_store, _directory_id), do: :erlang.nif_error(:nif_not_loaded)

  @doc false
  def index_store_stats(_store), do: :erlang.nif_error(:nif_not_loaded)
end
