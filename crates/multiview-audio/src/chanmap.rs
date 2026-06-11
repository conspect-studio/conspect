//! Channel mapping / shuffle / de-embed matrix.
//!
//! [`ChannelMatrix`] is a pure linear routing matrix from `N` input channels to
//! `M` output channels, with a gain per crosspoint. It covers the broadcast
//! operations the metering/monitoring surface needs:
//!
//! * **De-embed** — pull an arbitrary subset of channels (e.g. a stereo pair)
//!   out of a 16-channel embedded group (SMPTE ST 299-1 / AES3 model).
//! * **Shuffle** — re-order channels (swap L/R, re-map a discrete layout).
//! * **Sum / fold-down** — mix several inputs into one output with per-route
//!   gain (e.g. a −6 dB mono fold-down).
//!
//! The matrix is stored sparsely as a list of routes per output, so a 16→2
//! de-embed costs two multiplies, not 32. Out-of-range routes are rejected at
//! construction. This is pure DSP — the libav de-embed/decode that fills the
//! input channels lives behind the `ffmpeg` feature.
use serde::{Deserialize, Serialize};

use crate::error::{AudioError, Result};

/// A single crosspoint: take input channel `from`, scale by `gain`, and add it
/// to output channel `to`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// Source (input) channel index.
    pub from: usize,
    /// Destination (output) channel index.
    pub to: usize,
    /// Linear gain applied across this crosspoint.
    pub gain: f32,
}

/// A sparse `inputs × outputs` channel routing matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelMatrix {
    inputs: usize,
    outputs: usize,
    routes: Vec<Route>,
}

impl ChannelMatrix {
    /// An identity matrix of `n` channels (`out[i] = in[i]`).
    #[must_use]
    pub fn identity(n: usize) -> Self {
        let routes = (0..n)
            .map(|i| Route {
                from: i,
                to: i,
                gain: 1.0,
            })
            .collect();
        Self {
            inputs: n,
            outputs: n,
            routes,
        }
    }

    /// Build a matrix from `(from, to, gain)` crosspoints over an
    /// `inputs × outputs` grid.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidFormat`] if any route references an input or
    /// output channel outside the declared grid.
    pub fn from_routes(
        inputs: usize,
        outputs: usize,
        routes: &[(usize, usize, f32)],
    ) -> Result<Self> {
        let mut built = Vec::with_capacity(routes.len());
        for &(from, to, gain) in routes {
            if from >= inputs {
                return Err(AudioError::InvalidFormat(
                    "route input channel out of range",
                ));
            }
            if to >= outputs {
                return Err(AudioError::InvalidFormat(
                    "route output channel out of range",
                ));
            }
            built.push(Route { from, to, gain });
        }
        Ok(Self {
            inputs,
            outputs,
            routes: built,
        })
    }

    /// Number of input channels.
    #[must_use]
    pub const fn inputs(&self) -> usize {
        self.inputs
    }

    /// Number of output channels.
    #[must_use]
    pub const fn outputs(&self) -> usize {
        self.outputs
    }

    /// The routes (crosspoints) of this matrix.
    #[must_use]
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Map one input *frame* (`inputs` samples) to one output frame (`outputs`
    /// samples).
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::RaggedBlock`] if `frame.len() != self.inputs()`.
    pub fn apply(&self, frame: &[f32]) -> Result<Vec<f32>> {
        if frame.len() != self.inputs {
            return Err(AudioError::RaggedBlock {
                samples: frame.len(),
                channels: self.inputs,
            });
        }
        let mut out = vec![0.0f32; self.outputs];
        self.apply_into(frame, &mut out);
        Ok(out)
    }

    /// Map interleaved (frame-major) input PCM to interleaved output PCM.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::RaggedBlock`] if the sample count is not a whole
    /// number of input frames.
    pub fn apply_interleaved(&self, samples: &[f32]) -> Result<Vec<f32>> {
        if self.inputs == 0 || samples.len() % self.inputs != 0 {
            return Err(AudioError::RaggedBlock {
                samples: samples.len(),
                channels: self.inputs,
            });
        }
        let frames = samples.len() / self.inputs;
        let mut out = vec![0.0f32; frames.saturating_mul(self.outputs)];
        for (f, in_frame) in samples.chunks_exact(self.inputs).enumerate() {
            let base = f * self.outputs;
            if let Some(out_frame) = out.get_mut(base..base + self.outputs) {
                self.apply_into(in_frame, out_frame);
            }
        }
        Ok(out)
    }

    /// Accumulate one input frame into a pre-zeroed output frame slice.
    fn apply_into(&self, frame: &[f32], out: &mut [f32]) {
        for route in &self.routes {
            let (Some(&x), Some(slot)) = (frame.get(route.from), out.get_mut(route.to)) else {
                continue;
            };
            *slot += x * route.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn identity_roundtrips() {
        let m = ChannelMatrix::identity(3);
        assert_eq!(m.apply(&[1.0, 2.0, 3.0]).unwrap(), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn rejects_bad_route() {
        assert!(ChannelMatrix::from_routes(2, 2, &[(0, 9, 1.0)]).is_err());
    }
}
