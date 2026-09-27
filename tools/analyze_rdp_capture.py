#!/usr/bin/env python3
"""Count visible stripe changes in recordings of rdp_benchmark.html.

Record the complete, unscaled 960x1056 client window at 60 FPS. The HTML page
must be visible throughout the recording. Requires ffmpeg and ffprobe.
"""

import argparse
import json
import subprocess
from pathlib import Path


def stripe_left(row: bytes, width: int) -> int | None:
    # Ignore the bright one-pixel window border. The fixture's white stripe is
    # much wider than any other bright content on the center scanline.
    best_start = None
    best_length = 0
    start = None
    for x in range(4, width - 4):
        bright = min(row[x * 3 : x * 3 + 3]) > 220
        if bright and start is None:
            start = x
        if start is not None and (not bright or x == width - 5):
            length = x - start + int(bright)
            if length > best_length:
                best_start, best_length = start, length
            start = None
    return best_start if best_length >= max(40, width // 20) else None


def analyze(path: Path) -> tuple[int, float, int, int, float]:
    probe = subprocess.run(
        [
            "ffprobe", "-v", "error", "-select_streams", "v:0",
            "-show_entries", "stream=width,height:format=duration", "-of", "json",
            str(path),
        ],
        capture_output=True, text=True, check=True,
    )
    meta = json.loads(probe.stdout)
    stream = meta["streams"][0]
    width, height = stream["width"], stream["height"]
    duration = float(meta["format"]["duration"])
    if width < 100 or height < 100 or duration <= 0:
        raise ValueError("invalid capture dimensions or duration")
    scanline = (height // 2) & ~1  # YUV420 crop origin must be even.
    frame_bytes = width * 2 * 3
    decoder = subprocess.Popen(
        [
            "ffmpeg", "-v", "error", "-i", str(path), "-vf",
            f"crop={width}:2:0:{scanline},format=rgb24", "-f", "rawvideo", "-",
        ],
        stdout=subprocess.PIPE,
    )
    positions = []
    assert decoder.stdout is not None
    while frame := decoder.stdout.read(frame_bytes):
        if len(frame) != frame_bytes:
            decoder.kill()
            raise ValueError("truncated decoded frame")
        positions.append(stripe_left(frame[: width * 3], width))
    if decoder.wait() != 0:
        raise ValueError("ffmpeg could not decode capture")
    missing = positions.count(None)
    if not positions or missing > len(positions) // 20:
        raise ValueError("benchmark stripe missing from more than 5% of frames")
    changes = sum(
        before is not None and after is not None and before != after
        for before, after in zip(positions, positions[1:])
    )
    return len(positions), duration, changes, missing, changes / duration


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("captures", nargs="+", type=Path)
    args = parser.parse_args()
    for path in args.captures:
        frames, duration, changes, missing, rate = analyze(path)
        print(
            f"{path}: {frames} captured frames in {duration:.2f}s; "
            f"{changes} stripe changes ({rate:.2f}/s); "
            f"{missing} frames without stripe"
        )


if __name__ == "__main__":
    main()
