defmodule FindexRustBackend.MixProject do
  use Mix.Project

  def project do
    [
      app: :findex_rust_backend,
      version: "0.1.0",
      elixir: "~> 1.20",
      start_permanent: Mix.env() == :prod,
      deps: deps(),
      releases: releases()
    ]
  end

  def application do
    [
      extra_applications: [:logger]
    ]
  end

  defp deps do
    [
      {:findex, path: "../../findex"}
    ]
  end

  # Self-contained OTP release (bundled ERTS + the native library) for
  # packaged applications. The bridge is started with
  # `bin/backend eval "FindexRust.Bridge.run()"`, mirroring backend.exs.
  defp releases do
    [
      backend: [
        include_executables_for: [:unix],
        strip_beams: true
      ]
    ]
  end
end
