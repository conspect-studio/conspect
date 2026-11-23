# pluto-526 fixture

A slice of the live English `WebVTT` subtitle rendition of the pluto.tv "ABC News Live"
channel (source `us-abc-news-live` on mda001), recorded 2026-11-18 00:49 UTC for issue #526:
the rendition media playlist (segment URIs made relative, session query removed, `EXT-X-ENDLIST`
added so it plays once) and the first 8 cues of its first two `.vtt` segments, unchanged
otherwise. A few seconds of captions, kept only to pin the real wire shape: no
`X-TIMESTAMP-MAP`, cue times on the MPEG-TS clock of the video (`13:26:15.143` is the video's
raw PTS `48375.143 s`), cue settings after the end time, space-padded lines.
