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
- `assets/essm-scan.mp4` is an H.264 conversion of the existing
  `desktop/dist/essm-root-scan.gif`, with its original timing. The poster is a
  frame from that recording. Playback is user initiated, with native video
  controls and no autoplay.

To regenerate the recording and its poster with FFmpeg:

```sh
ffmpeg -i desktop/dist/essm-root-scan.gif -movflags +faststart \
  -pix_fmt yuv420p -vf 'scale=1440:-2' -an docs/assets/essm-scan.mp4
ffmpeg -ss 3 -i docs/assets/essm-scan.mp4 -frames:v 1 -update 1 \
  docs/assets/essm-scan-poster.png
```
