/// <reference types="vite/client" />

// Ambient module declarations for Vite's side-effect asset imports (`*.css`,
// `*.svg`, `?raw`, `?url`, …) and the typed `import.meta.env`. Vite ships these
// in `vite/client`; the standard scaffold references them from this file.
//
// Required since TypeScript 6.0: side-effect imports of non-code modules
// (`import "./index.css"` in src/main.tsx) now error with TS2882 unless an
// ambient module declaration covers them. Earlier TypeScript tolerated the
// missing declaration; 6.0 does not.
