# Audio decode fixtures

Run `python3 generate_audio.py` in this directory to regenerate these files.
The script synthesizes a 48 kHz stereo 1 kHz sine as raw `f32le`, then invokes
`ffmpeg -f f32le -ar 48000 -ac 2 -i pipe:0 -c:a pcm_s16le -f mov <name>.mov`
for each amplitude and duration listed in the script. The checked-in MOV files
contain lossless 16-bit PCM; the pinned LGPL FFmpeg 8.1.1 can demux MOV and
decode `pcm_s16le`, but cannot demux WAV. Tests require only pinned libav, not
an external CLI or Python. The 4-second clips preserve the loudness meter's
original reference duration and tolerances.
