defmodule Findex.MixProject do
  use Mix.Project

  def project do
    [
      app: :findex,
      version: "0.1.0",
      elixir: "~> 1.20",
      start_permanent: Mix.env() == :prod,
      aliases: aliases(),
      deps: []
    ]
  end

  def application do
    profiling_applications =
      if Mix.env() == :profile do
        [:tools, :runtime_tools]
      else
        []
      end

    [
      extra_applications: [:logger | profiling_applications]
    ]
  end

  defp aliases do
    [compile: ["cmd --cd native make", "compile"]]
  end
end
