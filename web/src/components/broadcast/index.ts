// Broadcast UI component library — the production components distilled from
// the approved broadcast-ui prototype (design round 4, correction round 4).
// Presentation-only, fully controlled, no network and no persistence: the
// realtime ledger owns every state transition. API docs and the browser
// regression acceptance notes live in README.md next to this file.
export { AssetDurationLabel } from './AssetDurationLabel';
export type { AssetDurationLabelProps } from './AssetDurationLabel';
export { OperationFeedback } from './OperationFeedback';
export type { OperationFeedbackProps, OperationPhase } from './OperationFeedback';
export { ProducerStatus } from './ProducerStatus';
export type { ProducerStatusProps } from './ProducerStatus';
export { TaskWorkspace } from './TaskWorkspace';
export type { TaskWorkspaceProps } from './TaskWorkspace';
export { TouchAction } from './TouchAction';
export type { TouchActionProps } from './TouchAction';
export {
  describeDurationPrecision,
  describeGrid,
  formatAssetDuration,
  groupThousands,
} from './duration';
export type {
  AssetDurationFormat,
  AssetDurationSource,
  AudioGrid,
  DurationGrid,
  RationalBoundary,
  VideoGrid,
} from './duration';
export { deriveProducerDisplay } from './producerDisplay';
export type {
  DerivedProducerDisplay,
  ProducerDisplay,
  ProducerState,
  ProducerUnknownReason,
} from './producerDisplay';
