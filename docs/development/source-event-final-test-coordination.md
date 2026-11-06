# Finish active test edits before rerunning combined gates

- Stressworker8e690 currently changes run closure to2args; callers service660/674/696 still1arg caused transient compile failure. Finish wholebatch and signalQUIESCENT before next gate.
- cue-adapter runtime623-625 API events private; fix test via serde_json::to_value, not exposing private productionfields.
- Packetcue-clock complete13tests but combined compile blocked above; defaultpipeline gate nonwrap, explicitMPEG33 tested separately.
- Lead native lib+bin clippy excludes cfgtest and can run meanwhile. No fullsuite yet; all targeted.
- Final R3 release measurement required same stable revision, sample p50/p95/max plus emptybaseline and memory/alloc bounds. Debugsample9.47ms full64x256 vs262us empty is material, not realtime approval. Fakeclock exacttick count proves continuity only.
