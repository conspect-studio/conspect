#!/usr/bin/env python3
"""Regenerate deterministic lossless PCM-in-MOV fixtures with a full FFmpeg CLI.

Run from this directory: python3 generate_audio.py
The pinned LGPL libav build demuxes MOV and decodes pcm_s16le, but its CLI
cannot ingest raw PCM and its libraries cannot demux WAV.
"""

import math
from pathlib import Path
import struct
import subprocess

HERE = Path(__file__).resolve().parent
FS = 48_000
FIXTURES = (
    ("tone_05_500ms.mov", 0.5, 0.5),
    ("tone_01_1ms.mov", 0.1, 0.001),
    ("tone_05_4s.mov", 0.5, 4.0),
    ("tone_quiet_4s.mov", 10 ** (-23.0 / 20.0), 4.0),
    ("tone_025_1s.mov", 0.25, 1.0),
)

for name, amp, seconds in FIXTURES:
    # Match the original Rust sine generator and FFmpeg's f32le -> s16le
    # conversion; the reference meter uses the original in-memory f32 samples.
    samples = bytearray()
    for i in range(round(FS * seconds)):
        sample = struct.pack("<f", amp * math.sin(2 * math.pi * 1000 * i / FS))
        samples.extend(sample * 2)
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
         "-f", "f32le", "-ar", str(FS), "-ac", "2", "-i", "pipe:0",
         "-c:a", "pcm_s16le", "-f", "mov", str(HERE / name)],
        input=samples,
        check=True,
    )
