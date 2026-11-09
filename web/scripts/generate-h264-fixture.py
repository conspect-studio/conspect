#!/usr/bin/env python3
"""Test-only ffmpeg generator; does not add an encoder to the shipped application."""
import pathlib, struct, subprocess, sys, tempfile
out = pathlib.Path(sys.argv[1])
with tempfile.TemporaryDirectory(dir=out.parent) as directory:
    raw = pathlib.Path(directory) / 'fixture.h264'
    subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i',
        'testsrc2=size=320x180:rate=30', '-t', '1', '-c:v', 'libx264', '-profile:v', 'baseline',
        '-level:v', '3.0', '-pix_fmt', 'yuv420p', '-x264-params',
        'keyint=1:min-keyint=1:scenecut=0:aud=1:repeat-headers=1', '-f', 'h264', str(raw)], check=True)
    data = raw.read_bytes()
    marker = b'\x00\x00\x00\x01\x09'
    starts = [i for i in range(len(data)) if data.startswith(marker, i)]
    assert len(starts) == 30 and starts[0] == 0
    packets = [data[a:b] for a, b in zip(starts, starts[1:] + [len(data)])]
    # Every packet includes SPS/PPS and IDR, independent of looping boundary.
    assert all(b'\x00\x00\x01\x65' in p for p in packets)
    out.write_bytes(b'H264' + b''.join(struct.pack('<I', len(p)) + p for p in packets))
print('Generated 30 all-IDR Annex B test frames, 320x180, 30fps')
