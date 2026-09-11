# ESSM website

The project website is a static, dependency-free site in this directory. GitHub
Pages publishes it at <https://hyperrays.github.io/ESSM/>.

## Preview locally

From the repository root:

```sh
python3 -m http.server 8000 --directory docs
```

Open <http://localhost:8000>. There is no build step or JavaScript dependency.
All local assets use relative URLs so the site also works under `/ESSM/`.

## Deployment

In the repository's **Settings → Pages → Build and deployment**, set **Source**
to **GitHub Actions**. The workflow in `.github/workflows/pages.yml` deploys
`docs/` whenever its files or the workflow change on `main`. It can also be run
manually from the Actions tab, selecting `main`.

Only this directory is uploaded. The desktop binaries, backend, and repository
files outside `docs/` are not part of the website artifact.

See [GitHub's custom Pages workflow documentation](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages).

## Updating content

- Edit `index.html` for copy, release links, and metadata; edit `style.css` for
  the appearance.
- When publishing a new desktop release, update the download URL, version, and
  platform requirements together. The current download is the verified
  `v0.1.0` Apple Silicon release for macOS 13 or later.
- `assets/essm-icon.png` is exported from `desktop/packaging/ESSM.icns`.
- The demo is a fresh capture made with the app's `--record` option at its
  native Retina resolution, 2720 × 1720. It is encoded directly from full-color
  PNG frames, without passing through the GIF or resizing.
- `assets/essm-scan.webm` uses lossless VP9 with identity RGB color space, so
  text and color detail are preserved exactly. The source advertises VP9
  Profile 1 so browsers that cannot decode it can select the MP4 fallback.
- `assets/essm-scan.mp4` is the broadly compatible H.264 fallback, encoded at
  CRF 8 and the same native resolution. The poster is an original PNG frame.
- The video autoplays muted and inline, with native pause, seek, and fullscreen
  controls. Browser settings may still block autoplay; the play control remains
  available. See [WebKit's video policies](https://webkit.org/blog/6784/new-video-policies-for-ios/).

## Record and encode the demo

Use a fresh directory, and leave the recorder running until the app exits after
showing the treemap, sunburst, graph, and diagnostics. `--anonymize` hides the
account username in displayed paths. Review the captured frames before publishing.

Build the current recorder, then run from the repository root:

```sh
(cd rust_client/backend && mix compile)
cargo build --release --manifest-path desktop/Cargo.toml
recording_dir=$(mktemp -d /tmp/essm-recording.XXXXXX)
desktop/target/release/essm \
  --dark --anonymize --record "$recording_dir" --record-fps 30 /
python3 scripts/encode-site-recording.py "$recording_dir" --poster-frame 300
```

The encoder requires Python 3 and FFmpeg with `libvpx-vp9` and `libx264`. Choose
a poster frame from the new recording; the example selects frame 300. A packaged
app built from the current source supports the same recording options.

The recorder targets 30 captures per second by default; `--record-fps` accepts
1–60. PNG compression runs on a worker thread with a bounded queue so it does
not block rendering. `frames.jsonl` stores monotonic capture timestamps, which
the encoder uses for real-time playback instead of estimating timing from disk
writes. Keep this file with the PNGs. The encoder reports the measured capture
rate and longest interval, and samples the captured frames onto a 30 fps video
timeline without interpolation or acceleration. The post-scan tour holds each
view for four seconds regardless of capture rate. Older recordings without a
timing manifest fall back to PNG modification timestamps.

If the captured window size changes, update the video dimensions in `index.html`
and the aspect ratio in `style.css` together.
