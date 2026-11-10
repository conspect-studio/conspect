# Actual AVC metadata fixture

`libx264-64x64-idr.h264` is a real Annex-B encoder emission, not synthetic SPS/PPS data. Pure metadata tests read these exact bytes without a native encoder or executable dependency.

Generated with FFmpeg `7.1.5-0+deb13u1`:

```sh
ffmpeg -v error -f lavfi -i color=size=64x64:rate=25 -frames:v 1 \
  -c:v libx264 -preset ultrafast -tune zerolatency -f h264 \
  libx264-64x64-idr.h264
```

Length: 658 bytes. SHA-256:

```text
2dda6a1eb034c4601890735281bd44b7d1d14ede6abc4654eff06844821a383a
```

The feature-gated native integration test separately opens the repository's actual `VideoEncoder`, encodes a frame, and verifies the emitted SPS/PPS metadata. This fixture does not replace that native coverage.
