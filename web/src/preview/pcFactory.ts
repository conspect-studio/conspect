// The peer-connection factory seam (ADR-W023 §1).
//
// Kept in its own module (not WhepPlayer.tsx) so the component file exports only
// components — jsdom has no RTCPeerConnection, and injecting this factory lets
// vitest drive a scripted fake through the whole negotiate/connect/fail state
// space without a browser, with no module-level globals in the component.

/** A factory for the peer connection — overridable so tests inject a fake. */
export type PeerConnectionFactory = (config: RTCConfiguration) => RTCPeerConnection;

/** The default factory: a real browser `RTCPeerConnection`. */
export const defaultPcFactory: PeerConnectionFactory = (config) => new RTCPeerConnection(config);
