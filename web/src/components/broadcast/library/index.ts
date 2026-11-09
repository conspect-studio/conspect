// Media-library UI components — production components distilled from the
// approved library prototype (UX reference only; its simulated store,
// fake jobs and success toasts are NOT carried over). Presentation-only,
// fully controlled, no fetch, no persistence, no timers, no demo data:
// every value is caller-owned and every callback is an INTENT. Contracts,
// data-adapter requirements and the backend capability gaps this folder
// renders honestly live in README.md next to this file.
export { AssetAuditionControls } from './AssetAuditionControls';
export type {
  AnalysisData,
  AnalysisModeOption,
  AssetAuditionControlsProps,
  ChannelOption,
} from './AssetAuditionControls';
export { LibraryBrowser } from './LibraryBrowser';
export type { LibraryBrowserProps } from './LibraryBrowser';
export { MetadataInspector } from './MetadataInspector';
export type {
  DeclaredMetadata,
  LibraryAssetMetadata,
  MeasuredMetadata,
  MetadataInspectorProps,
  ProvenanceRecord,
  UsageRecord,
} from './MetadataInspector';
export { TranscriptRangeEditor } from './TranscriptRangeEditor';
export type { TranscriptRangeEditorProps } from './TranscriptRangeEditor';
export { VTReferenceSummary } from './VTReferenceSummary';
export type { VTReferenceState, VTReferenceSummaryProps } from './VTReferenceSummary';
export {
  AssetKindChip,
  InUseChip,
  OnAirPill,
  RightsChip,
  TranscriptStateChip,
} from './Chips';
export {
  boundaryAtGridIndex,
  compareBoundaries,
  describeBoundaryUnit,
  formatBoundaryLabel,
  isSameGrid,
  stepBoundary,
} from './boundaries';
export type { BoundaryStepDirection } from './boundaries';
export type {
  ActionGate,
  LibraryAssetSummary,
  LibraryBatchAction,
  LibraryFacetGroup,
  LibraryFacetOption,
  LibraryOriginKind,
  LibraryRightsStatus,
  LibraryRowAction,
  LibrarySortOption,
  LibraryTranscriptState,
  RowActionGateResolver,
  TranscriptSelection,
  TranscriptWord,
} from './types';
