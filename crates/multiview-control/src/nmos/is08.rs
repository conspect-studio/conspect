//! AMWA **NMOS IS-08** ("Audio Channel Mapping") model.
//!
//! IS-08 maps the **output** channels of a device to **input** channels — the
//! IP-native equivalent of an audio shuffle/router matrix (broadcast-multiviewer
//! brief §3). Multiview uses it so a facility controller can route which embedded
//! audio channels feed the program/monitor bus.
//!
//! The model is a pure **map** plus a staged/active activation, mirroring IS-05.
//! A single **output channel** is fed by exactly one **(input, input-channel)**
//! pair, or by silence (an unrouted channel). The model validates that every
//! mapped input exists in the declared input set so a map cannot reference a
//! phantom input. No sockets — this is the channel-mapping value type; the audio
//! it controls lives in `multiview-audio`.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One output channel's source: an input id + the zero-based channel within it,
/// or `None`/silence when the channel is unrouted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ChannelSource {
    /// The input the channel is fed from, or [`None`] for silence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// The zero-based channel index within that input (ignored when silent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_index: Option<u32>,
}

impl ChannelSource {
    /// A routed channel: input `input`, channel `channel_index`.
    #[must_use]
    pub fn routed(input: impl Into<String>, channel_index: u32) -> Self {
        Self {
            input: Some(input.into()),
            channel_index: Some(channel_index),
        }
    }

    /// An unrouted (silent) channel.
    #[must_use]
    pub fn silence() -> Self {
        Self {
            input: None,
            channel_index: None,
        }
    }

    /// Whether this channel is routed (vs silence).
    #[must_use]
    pub fn is_routed(&self) -> bool {
        self.input.is_some()
    }
}

/// An IS-08 channel-mapping error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MappingError {
    /// A mapped output channel referenced an input not in the declared set.
    #[error("output channel {output_channel:?} maps to unknown input {input:?}")]
    UnknownInput {
        /// The output channel name with the dangling reference.
        output_channel: String,
        /// The input id that does not exist.
        input: String,
    },
}

/// An IS-08 **map**: each named output channel → its [`ChannelSource`].
///
/// A `BTreeMap` so the serialised map and any listing is deterministically
/// ordered (stable diagnostics + golden tests).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ChannelMap {
    /// The set of known input ids this map may reference.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// The output-channel → source assignments.
    #[serde(default)]
    pub map: BTreeMap<String, ChannelSource>,
}

impl ChannelMap {
    /// A fresh map over the given input id set.
    #[must_use]
    pub fn new(inputs: Vec<String>) -> Self {
        Self {
            inputs,
            map: BTreeMap::new(),
        }
    }

    /// Assign an output channel to a source.
    pub fn assign(&mut self, output_channel: impl Into<String>, source: ChannelSource) {
        self.map.insert(output_channel.into(), source);
    }

    /// Validate that every routed channel references a declared input.
    ///
    /// # Errors
    ///
    /// [`MappingError::UnknownInput`] for the first channel routed to an input
    /// not in [`ChannelMap::inputs`].
    pub fn validate(&self) -> Result<(), MappingError> {
        for (channel, source) in &self.map {
            if let Some(input) = &source.input {
                if !self.inputs.iter().any(|i| i == input) {
                    return Err(MappingError::UnknownInput {
                        output_channel: channel.clone(),
                        input: input.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// The number of output channels that are routed (non-silent).
    #[must_use]
    pub fn routed_count(&self) -> usize {
        self.map.values().filter(|s| s.is_routed()).count()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{ChannelMap, ChannelSource, MappingError};

    fn map() -> ChannelMap {
        let mut m = ChannelMap::new(vec!["in-a".to_owned(), "in-b".to_owned()]);
        m.assign("out-0", ChannelSource::routed("in-a", 0));
        m.assign("out-1", ChannelSource::routed("in-b", 1));
        m.assign("out-2", ChannelSource::silence());
        m
    }

    #[test]
    fn map_round_trips_through_json_in_stable_order() {
        let m = map();
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["map"]["out-0"]["input"], "in-a");
        assert_eq!(json["map"]["out-0"]["channel_index"], 0);
        // A silent channel omits input/channel.
        assert!(json["map"]["out-2"].get("input").is_none());
        let back: ChannelMap = serde_json::from_value(json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn valid_map_passes_validation() {
        assert!(map().validate().is_ok());
    }

    #[test]
    fn routed_count_counts_only_non_silent_channels() {
        assert_eq!(map().routed_count(), 2);
    }

    #[test]
    fn validation_rejects_a_reference_to_an_undeclared_input() {
        let mut m = map();
        m.assign("out-3", ChannelSource::routed("ghost-input", 0));
        let err = m.validate().unwrap_err();
        assert_eq!(
            err,
            MappingError::UnknownInput {
                output_channel: "out-3".to_owned(),
                input: "ghost-input".to_owned(),
            }
        );
    }

    #[test]
    fn silence_source_is_not_routed() {
        let silent = ChannelSource::silence();
        assert!(!silent.is_routed());
        assert!(silent.input.is_none());
        assert!(silent.channel_index.is_none());
    }
}
