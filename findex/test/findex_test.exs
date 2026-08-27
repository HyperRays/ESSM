defmodule FindexTest do
  use ExUnit.Case
  doctest Findex

  test "greets the world" do
    assert Findex.hello() == :world
  end

  test "prints hello world from C" do
    assert Findex.Nif.hello() == :ok
  end
end
