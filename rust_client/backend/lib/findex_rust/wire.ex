defmodule FindexRust.Wire do
  @moduledoc false

  @magic "FIDX"
  @protocol_version 6
  @header_bytes 9
  @maximum_frame_bytes 64 * 1024 * 1024

  @spec protocol_version() :: pos_integer()
  def protocol_version, do: @protocol_version

  @spec encode(term()) :: binary()
  def encode(term) do
    payload = :erlang.term_to_binary(term, [:deterministic])
    <<@magic, @protocol_version, byte_size(payload)::unsigned-big-32, payload::binary>>
  end

  @spec decode(binary()) :: {:ok, term()} | {:error, term()}
  def decode(<<@magic, @protocol_version, size::unsigned-big-32, payload::binary-size(size)>>) do
    decode_payload(payload)
  end

  def decode(<<@magic, version, _rest::binary>>) when version != @protocol_version,
    do: {:error, {:unsupported_protocol, version}}

  def decode(_frame), do: {:error, :invalid_frame}

  @spec read(IO.device()) :: {:ok, term()} | :eof | {:error, term()}
  def read(input) do
    with {:ok, header} <- read_exact(input, @header_bytes),
         <<@magic, version, size::unsigned-big-32>> <- header,
         :ok <- validate_version(version),
         :ok <- validate_size(size),
         {:ok, payload} <- read_exact(input, size) do
      decode_payload(payload)
    else
      :eof -> :eof
      {:error, reason} -> {:error, reason}
      _other -> {:error, :invalid_frame_header}
    end
  end

  @spec write(IO.device(), term()) :: :ok
  def write(output, term), do: IO.binwrite(output, encode(term))

  @spec decode_all(binary()) :: {:ok, [term()]} | {:error, term()}
  def decode_all(binary), do: decode_all(binary, [])

  defp decode_all(<<>>, terms), do: {:ok, Enum.reverse(terms)}

  defp decode_all(
         <<@magic, @protocol_version, size::unsigned-big-32, payload::binary-size(size),
           rest::binary>>,
         terms
       ) do
    case decode_payload(payload) do
      {:ok, term} -> decode_all(rest, [term | terms])
      {:error, reason} -> {:error, reason}
    end
  end

  defp decode_all(<<@magic, version, _rest::binary>>, _terms)
       when version != @protocol_version,
       do: {:error, {:unsupported_protocol, version}}

  defp decode_all(_binary, _terms), do: {:error, :invalid_frame}

  defp read_exact(_input, 0), do: {:ok, <<>>}
  defp read_exact(input, bytes), do: read_exact(input, bytes, [])

  defp read_exact(_input, 0, chunks),
    do: {:ok, chunks |> Enum.reverse() |> IO.iodata_to_binary()}

  defp read_exact(input, remaining, chunks) do
    case IO.binread(input, remaining) do
      :eof when chunks == [] ->
        :eof

      :eof ->
        {:error, :truncated_frame}

      {:error, reason} ->
        {:error, {:io_error, reason}}

      data when is_binary(data) and byte_size(data) > 0 ->
        read_exact(input, remaining - byte_size(data), [data | chunks])

      _other ->
        {:error, :empty_frame_read}
    end
  end

  defp validate_version(@protocol_version), do: :ok
  defp validate_version(version), do: {:error, {:unsupported_protocol, version}}

  defp validate_size(size) when size <= @maximum_frame_bytes, do: :ok
  defp validate_size(size), do: {:error, {:frame_too_large, size}}

  defp decode_payload(<<131, 80, _compressed::binary>>),
    do: {:error, :compressed_external_term_not_allowed}

  defp decode_payload(payload) do
    try do
      {:ok, :erlang.binary_to_term(payload, [:safe])}
    rescue
      ArgumentError -> {:error, :invalid_external_term}
    end
  end
end
