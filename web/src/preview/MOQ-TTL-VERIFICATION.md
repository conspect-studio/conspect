# Actual connected authorization expiry

Measured 2026-11-08 with Chrome149.0.7827.55, native VP8 decoder, QUIC-only draft16. Runtime source HEAD at build was d7d25a13a1b26733ed0daba2d73c0546fef253df with no multiview-moq diff; fixture extension committed b0661e06.

Command: `node web/scripts/moq-authorized-probe.mjs /workspaces/conspect/.agents/worktrees/dsh-f9c08465-49b7` with isolated Vite on [::1]:5179. Probe launches real Runtime::listen_files on [::]:4444, reads its mode0600 admission response into memory and passes credentials directly into browser evaluation. No token-bearing URL is printed or committed. Library debug output is suppressed before connection.

Observed 237 painted VideoFrames with advancing timestamps. Server enforced eight-second authorization TTL without a client close call: connection.closed settled at7989ms. Frame count237 at closure,237 after one-second drain,237 after another quiet second. Server-side Demand drop observer confirmed release by11s. Exact sanitized evidence is moq-authorized-evidence.json.

No per-peer isolation claim: this probe used one admitted peer. No audio/AV-sync/HEVC/paid-media/encrypted-media claim. This is a local authenticated runtime lifecycle test, not Cloudflare end-to-end acceptance.
