# R3: explicit immutable artifact adoption

Scope: A+B+C only. SAVE and RELEASE_REFERENCE remain dependent work. This source checkpoint is not a native verification result.

## Invariants

The output clock never awaits input, native opening, filesystem work, locks, a client, or a native destructor. Output timestamps remain tick-derived. Opening, probing, priming, cancellation and destruction belong to isolated workers. A failed load retains the old valid picture and audio generation.

POST load must authorize the player, capture job, fill and immutable artifact owner from Principal before resolving a capability through the already-running capture service. No second ArtifactStore, body owner, arbitrary path, URL resolver, download stream permit lifetime, or capture autoload is allowed. Control defines the provider contract; CLI implements it. Control must never depend on CLI.

One pending latest-wins load, one staged clip, two retired native clips and one audio commit slot bound ownership. Replaced requests receive a terminal superseded outcome. Capacity must be reserved before acceptance. Saturation fails closed; no native owner drops on the clock or ingest thread. An owned immutable lease is shared by video and audio generation guards until both retirement acknowledgements arrive.

Native success requires the first primed video image adoption plus matching audio generation adoption, or proven absent audio and explicit silence. The acknowledgement includes operation, source identity, clip generation, actual duration, rational cadence, geometry, audio presence and timeline. Admission, staging and first decode are not completion.

Cancellation uses FFmpeg interrupt callbacks with owned deadlines and panic-contained callback boundaries. Opening and retirement deadlines fail without invalidating the last committed generation. Runtime owner flock precedes recovery claims. A pending journal record is not proof that an old native owner stopped. Native panic paths transfer owners to an isolated retirement worker; no FFI destructor may stick the output path.

## Source and proof plan

Port symbol-scoped player donors 67c408a9 / 81ce5510 / 7cd51c2b into current FFmpeg 9 code. Preserve current HTTP headers and hardened artifact FD-relative access. Do not port old output pipelines, manifests or history.

Required focused chaos/native proofs: A→B real decoder replacement; failed B retains A; pending-load storms stay bounded; stuck opener cancels; retirement stalls preserve cadence; audio adopts exact generation; absent audio selects silence; panic isolates destruction; killed old owner requires flock and boot quiescence. Explicit capture HTTP load must report actual metadata and prove its first image, and PCM only for audio-bearing artifacts.

Current integration base: main 9b6766ae after capture PR225 and production-line PR229. The earlier inactive donor is checkpointed at 951a2415; it is provenance, not a verified release. Preserve main's switcher, MoQ, preview and audio-worker behavior through symbol-scoped additions.

Validation is now active in the task worktree. The initial native HTTP fixture uses a trusted producer that emits real FFV1 MOV media, not a browser, and verifies video/audio adoption. Real browser capture is separately established by PR225; no Windy-to-player or deployed workflow is implied by these two separate proofs. New changes require default and linked builds, parallel-safe budget tests, the chaos cases above, generated API contracts, independent review and green PR CI. Keep shared disk above the shipping reserve and run one Cargo invocation in this lane.
