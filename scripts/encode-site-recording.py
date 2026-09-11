#!/usr/bin/env python3
"""Encode ESSM --record PNGs at native resolution and their captured timing."""

import argparse
import json
import math
import shutil
import statistics
import subprocess
import tempfile
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("frames", type=Path, help="Original --record directory, including frames.jsonl")
    parser.add_argument("--poster-frame", type=int, required=True, help="Zero-based frame for the poster")
    parser.add_argument("--output", type=Path, default=Path("docs/assets"))
    parser.add_argument("--gif-output", type=Path, help="Also create a real-time, 1360px-wide README preview")
    args = parser.parse_args()
    frames = sorted(args.frames.resolve().glob("frame-*.png"))
    if len(frames) < 2:
        parser.error("At least two recorded frames are required")
    if not 0 <= args.poster_frame < len(frames):
        parser.error("Poster frame is outside the recording")
    if not shutil.which("ffmpeg"):
        parser.error("FFmpeg with libvpx-vp9 and libx264 is required")

    timing_file = args.frames / "frames.jsonl"
    if timing_file.exists():
        try:
            timing = [json.loads(line) for line in timing_file.read_text().splitlines()]
            if [entry["file"] for entry in timing] != [frame.name for frame in frames]:
                parser.error("Capture timing must list every PNG exactly once in order")
            timestamps = [float(entry["elapsed_seconds"]) for entry in timing]
        except (ValueError, KeyError, TypeError) as error:
            parser.error("Invalid capture timing: " + str(error))
    else:
        # Compatibility with recordings from before capture-time metadata existed.
        timestamps = [frame.stat().st_mtime_ns / 1e9 for frame in frames]
    durations = [end - start for start, end in zip(timestamps, timestamps[1:])]
    if any(not math.isfinite(value) for value in timestamps) or any(hold <= 0 for hold in durations):
        parser.error("Capture times must be finite and increase in frame order")
    print("Measured capture rate: {:.2f} fps; longest interval: {:.1f} ms.".format(
        (len(frames) - 1) / (timestamps[-1] - timestamps[0]), max(durations) * 1000,
    ), flush=True)
    # A final hold of one capture interval completes the last frame.
    durations.append(statistics.median(durations))
    duration = math.ceil(sum(durations) * 30) / 30
    args.output.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="essm-video-") as temporary:
        temporary = Path(temporary)
        lines = ["ffconcat version 1.0"]
        for index, (frame, hold) in enumerate(zip(frames, durations)):
            # Safe generated filenames also support source paths with spaces or quotes.
            name = "frame-{:04d}.png".format(index)
            (temporary / name).symlink_to(frame)
            lines.extend(["file " + name, "option framerate 1000", "duration {:.9f}".format(hold)])
        # The concat demuxer needs a following frame to honor the final duration.
        lines.extend(["file " + name, "option framerate 1000"])
        manifest = temporary / "frames.ffconcat"
        manifest.write_text("\n".join(lines) + "\n")

        common = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "concat", "-safe", "0", "-i", str(manifest),
            "-an", "-vf", "fps=30", "-t", "{:.9f}".format(duration),
        ]
        # Identity RGB color space and VP9 lossless mode retain every source pixel.
        # A zero target bitrate is necessary to avoid rate control reducing quality.
        print("Encoding lossless RGB WebM…", flush=True)
        subprocess.run(common + [
            "-c:v", "libvpx-vp9", "-lossless", "1", "-crf", "0", "-b:v", "0",
            "-pix_fmt", "gbrp", "-colorspace", "rgb", "-color_range", "pc",
            "-row-mt", "1", "-threads", "8", "-cpu-used", "4",
            str(args.output / "essm-scan.webm"),
        ], check=True)

        print("Encoding high-quality H.264 fallback…", flush=True)
        subprocess.run(common + [
            "-c:v", "libx264", "-preset", "slow", "-crf", "8",
            "-pix_fmt", "yuv420p", "-movflags", "+faststart",
            str(args.output / "essm-scan.mp4"),
        ], check=True)

    shutil.copyfile(frames[args.poster_frame], args.output / "essm-scan-poster.png")
    if args.gif_output:
        args.gif_output.parent.mkdir(parents=True, exist_ok=True)
        print("Encoding real-time README preview…", flush=True)
        # Decode the lossless master so the GIF shares the video's timing.
        subprocess.run([
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-i", str(args.output / "essm-scan.webm"),
            "-filter_complex",
            "fps=30,scale=1360:-1:flags=lanczos,split[a][b];"
            "[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=diff_mode=rectangle",
            "-loop", "0", str(args.gif_output),
        ], check=True)
    print("Encoded {} source frames across {:.3f} seconds.".format(len(frames), duration))


if __name__ == "__main__":
    main()
