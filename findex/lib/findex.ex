defmodule Findex do
  @moduledoc """
  macOS filesystem indexing with a concurrent, readable native store.

  Use `Findex.Indexer` for recursive scans, `Findex.Store` to read the retained
  tree, and `Findex.Directory` for bounded, non-recursive enumeration.
  `Findex.Batch` decodes metadata from packed directory results without
  allocating an entry struct for every row.

  ## Examples

      iex> :data_size in Findex.Directory.supported_fields()
      true

  """
end
