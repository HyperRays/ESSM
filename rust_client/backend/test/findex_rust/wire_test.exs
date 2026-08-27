defmodule FindexRust.WireTest do
  use ExUnit.Case, async: true

  alias FindexRust.Wire

  test "round-trips raw binaries and the full unsigned 64-bit range" do
    term = %{
      "rank" => <<0, 1, 127, 128, 255>>,
      "request_id" => 18_446_744_073_709_551_615,
      "nested" => [true, false, nil]
    }

    assert {:ok, ^term} = term |> Wire.encode() |> Wire.decode()
  end

  test "rejects malformed, truncated, and unsupported frames" do
    assert {:error, :invalid_frame} = Wire.decode("not a frame")
    assert {:error, :invalid_frame} = Wire.decode(<<"FIDX", 6, 10::unsigned-big-32, 1, 2>>)

    assert {:error, {:unsupported_protocol, 5}} =
             Wire.decode(<<"FIDX", 5, 0::unsigned-big-32>>)
  end

  test "rejects compressed external terms before decoding" do
    payload = :erlang.term_to_binary(String.duplicate("x", 10_000), compressed: 9)
    assert <<131, 80, _compressed::binary>> = payload

    frame = <<"FIDX", 6, byte_size(payload)::unsigned-big-32, payload::binary>>
    assert {:error, :compressed_external_term_not_allowed} = Wire.decode(frame)
  end
end
