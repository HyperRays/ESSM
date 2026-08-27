defmodule Findex.PosixError do
  @moduledoc """
  macOS POSIX error decoding and traversal policy.

  Directory-local failures are safe to retain in an incomplete index. Resource
  exhaustion and native invariants abort the traversal so a partial result is
  never mistaken for a complete index.
  """

  @reasons %{
    1 => :eperm,
    2 => :enoent,
    4 => :eintr,
    5 => :eio,
    6 => :enxio,
    9 => :ebadf,
    11 => :edeadlk,
    12 => :enomem,
    13 => :eacces,
    14 => :efault,
    16 => :ebusy,
    20 => :enotdir,
    22 => :einval,
    23 => :enfile,
    24 => :emfile,
    28 => :enospc,
    30 => :erofs,
    34 => :erange,
    35 => :eagain,
    62 => :eloop,
    63 => :enametoolong,
    70 => :estale,
    84 => :eoverflow,
    89 => :ecanceled,
    92 => :eilseq,
    102 => :enotsup
  }

  @access_errors [:eacces, :eperm]
  @changed_errors [:enoent, :enotdir, :eloop, :estale]
  @io_errors [:eio, :enxio, :eilseq]
  @transient_errors [:eagain, :ebusy, :ecanceled, :eintr]
  @resource_errors [:emfile, :enfile, :enomem, :enospc]
  @invariant_errors [:ebadf, :efault, :einval]

  @type reason :: atom() | pos_integer()
  @type category ::
          :access_denied
          | :filesystem_changed
          | :dataless
          | :io
          | :path
          | :unsupported
          | :buffer_limit
          | :transient
          | :resource_exhausted
          | :native_invariant
          | :unexpected

  @doc "Decodes an integer using the errno values from the macOS SDK."
  @spec reason(non_neg_integer()) :: reason()
  def reason(error_number) when is_integer(error_number) and error_number >= 0,
    do: Map.get(@reasons, error_number, error_number)

  @doc "Classifies a directory operation failure as recoverable or fatal."
  @spec classify(reason()) :: {:recoverable | :fatal, category()}
  def classify(reason) when reason in @access_errors, do: {:recoverable, :access_denied}
  def classify(reason) when reason in @changed_errors, do: {:recoverable, :filesystem_changed}
  def classify(:edeadlk), do: {:recoverable, :dataless}
  def classify(reason) when reason in @io_errors, do: {:recoverable, :io}
  def classify(:enametoolong), do: {:recoverable, :path}
  def classify(:enotsup), do: {:recoverable, :unsupported}
  def classify(:erange), do: {:recoverable, :buffer_limit}
  def classify(:eoverflow), do: {:recoverable, :buffer_limit}
  def classify(reason) when reason in @transient_errors, do: {:recoverable, :transient}
  def classify(reason) when reason in @resource_errors, do: {:fatal, :resource_exhausted}
  def classify(reason) when reason in @invariant_errors, do: {:fatal, :native_invariant}
  def classify(_reason), do: {:fatal, :unexpected}
end
