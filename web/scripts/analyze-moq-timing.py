#!/usr/bin/env python3
"""Compare video paint and audio decode callbacks, not acoustic presentation."""
import json, sys
results=[]
for path in sys.argv[1:]:
    with open(path) as f:
        result=next(json.loads(line) for line in f if line.startswith('{"frames"'))
    assert result['error'] is None
    assert len(set(result['timestamps'])) >= 20
    assert len(set(result['audioTimestamps'])) >= 40
    audio=list(zip(result['audioTimestamps'],result['audioWallMs']))
    offsets=[]
    for pts,wall in zip(result['timestamps'],result['videoWallMs']):
        nearest=min(audio,key=lambda pair: abs(pair[0]-pts))
        if abs(nearest[0]-pts)<=10000:
            offsets.append(wall-nearest[1])
    assert offsets, 'No overlapping timestamp epoch'
    results.append({'protocol':result['version'],'videoPaints':result['frames'],
        'uniqueVideoPts':len(set(result['timestamps'])),'nativeAudioOutputs':result['audioFrames'],
        'matchedPtsSamples':len(offsets),'videoPaintMinusAudioDecodeMs':{'min':min(offsets),'max':max(offsets),'mean':sum(offsets)/len(offsets)},
        'meaning':'Callback timing only; no speaker/display synchronization bound'})
print(json.dumps(results,indent=2))
