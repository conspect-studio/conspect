# Conspect Cast Web Receiver (ADR-0111)

`index.html`, `receiver.js` and `receiver.css` are the custom CAF v3 Web Receiver the engine serves at
`/cast/receiver/` (embedded with `include_str!` by `src/routes/cast_public.rs`, under the CSP in
`RECEIVER_CSP`).

They are tested by `tools/cast-proof/` (the real CAF SDK against a fake platform) exactly as they are here.
Do not reformat, minify or lint-fix them: change them only together with that proof.
