// The Sources / Outputs / Overlays resource views.
//
// Split into per-page modules (kind-specific typed forms grounded in the Rust
// config schema; see each file). This module re-exports them so existing
// imports (the router) keep working.
export { SourcesPage } from './SourcesPage';
export { OutputsPage } from './OutputsPage';
export { OverlaysPage } from './OverlaysPage';
