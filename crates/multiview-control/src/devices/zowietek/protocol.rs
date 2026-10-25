//! The **grounded** `ZowieTek` vendor wire shapes (DEV-A4 / upstream issue
//! `aperim/multiview#140`).
//!
//! These models encode exactly what the vendor's HTTP API was measured to
//! accept and return, so the driver never guesses an `opt` or invents a
//! response field again. Corrections captured here (the previous driver got
//! these wrong):
//!
//! 1. Enumerating the decode table is `{"group":"streamplay",
//!    "opt":"streamplay_get_all"}` — a bare `"opt":"streamplay"` returns an
//!    **empty** `data`.
//! 2. The workmode is read with `{"group":"workmode","opt":"get_workmode_id"}`
//!    → `{"workmode_id":N}` (encoder = 0, decoder = 1), not a `venc` probe.
//! 3. Switching a decode slot is **index-scoped**:
//!    `{"group":"streamplay","opt":"streamplay_switch","data":{"index":N,
//!    "switch":0|1}}`.
//! 4. `{"group":"workmode","opt":"change_workmode","data":{"workmode_id":N}}`
//!    is the real workmode switch.
//!
//! The decoder is **single-active**: activating one index makes it the sole
//! active decode and implicitly stops whatever was playing, so every
//! index-scoped switch is an inherently disruptive (Class-2) operation that
//! must be surfaced before apply (invariant #11).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::WorkMode;

/// The `workmode` request group.
pub const WORKMODE_GROUP: &str = "workmode";
/// The `streamplay` request group.
pub const STREAMPLAY_GROUP: &str = "streamplay";

/// `workmode` / `get_workmode_id`: read the current workmode id.
pub const GET_WORKMODE_ID: &str = "get_workmode_id";
/// `workmode` / `change_workmode`: switch the workmode.
pub const CHANGE_WORKMODE: &str = "change_workmode";
/// `streamplay` / `streamplay_get_all`: enumerate the decode table.
pub const STREAMPLAY_GET_ALL: &str = "streamplay_get_all";
/// `streamplay` / `get_decoder_state`: read the decoder state.
pub const GET_DECODER_STATE: &str = "get_decoder_state";
/// `streamplay` / `streamplay_switch`: activate/deactivate one decode index.
pub const STREAMPLAY_SWITCH: &str = "streamplay_switch";
/// The value `switch` takes to make a decode index the active one.
pub const SWITCH_ON: u8 = 1;
/// The value `switch` takes to take a decode index out of service.
pub const SWITCH_OFF: u8 = 0;

/// A grounded group/opt call carrying no `data` payload (`data: null`, the
/// convention every read call in this driver already uses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullDataCall {
    /// `workmode` / `get_workmode_id` — the grounded workmode read.
    GetWorkmodeId,
    /// `streamplay` / `streamplay_get_all` — the grounded decode-table read.
    StreamplayGetAll,
    /// `streamplay` / `get_decoder_state` — the decoder-state read.
    GetDecoderState,
}

impl NullDataCall {
    /// The request group.
    #[must_use]
    pub const fn group(self) -> &'static str {
        match self {
            Self::GetWorkmodeId => WORKMODE_GROUP,
            Self::StreamplayGetAll | Self::GetDecoderState => STREAMPLAY_GROUP,
        }
    }

    /// The request `opt` (never the group name by accident — the legacy driver
    /// sent `"streamplay"` here and got an empty table back).
    #[must_use]
    pub const fn opt(self) -> &'static str {
        match self {
            Self::GetWorkmodeId => GET_WORKMODE_ID,
            Self::StreamplayGetAll => STREAMPLAY_GET_ALL,
            Self::GetDecoderState => GET_DECODER_STATE,
        }
    }

    /// The full `{group,opt,data}` request body.
    #[must_use]
    pub fn body(self) -> Value {
        json!({ "group": self.group(), "opt": self.opt(), "data": Value::Null })
    }
}

/// `{"group":"workmode","opt":"get_workmode_id"}` response body:
/// `{"workmode_id":N}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkmodeIdBody {
    /// The vendor workmode id (encoder = 0, decoder = 1).
    pub workmode_id: u32,
}

impl WorkmodeIdBody {
    /// The typed workmode this id names, if the vendor id is one we know.
    #[must_use]
    pub fn workmode(self) -> Option<WorkMode> {
        WorkMode::from_wire_id(self.workmode_id)
    }
}

/// `{"group":"workmode","opt":"change_workmode","data":{"workmode_id":N}}` —
/// the real workmode switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeWorkmode {
    /// The vendor workmode id to converge to.
    pub workmode_id: u32,
}

impl ChangeWorkmode {
    /// A switch to a known [`WorkMode`].
    #[must_use]
    pub const fn to(mode: WorkMode) -> Self {
        Self {
            workmode_id: mode.wire_id(),
        }
    }

    /// The full `{group,opt,data}` request body.
    #[must_use]
    pub fn body(self) -> Value {
        json!({ "group": WORKMODE_GROUP, "opt": CHANGE_WORKMODE, "data": self })
    }
}

/// `{"group":"streamplay","opt":"streamplay_switch","data":{"index":N,
/// "switch":0|1}}` — the index-scoped decode switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamplaySwitch {
    /// The decode-table index the switch targets.
    pub index: u32,
    /// [`SWITCH_ON`] to make this index the sole active decode,
    /// [`SWITCH_OFF`] to take it out of service.
    pub switch: u8,
}

impl StreamplaySwitch {
    /// Activate `index` as the decoder's sole active decode (Class-2: whatever
    /// was playing stops).
    #[must_use]
    pub const fn activate(index: u32) -> Self {
        Self {
            index,
            switch: SWITCH_ON,
        }
    }

    /// The `data` payload alone (`{index,switch}`).
    #[must_use]
    pub fn data(self) -> Value {
        json!({ "index": self.index, "switch": self.switch })
    }

    /// The full `{group,opt,data}` request body.
    #[must_use]
    pub fn body(self) -> Value {
        json!({ "group": STREAMPLAY_GROUP, "opt": STREAMPLAY_SWITCH, "data": self.data() })
    }
}

/// One row of the `streamplay_get_all` decode table:
/// `{index,name,switch,streamplay_status,url,…}`.
///
/// Unknown vendor fields are tolerated (the device's table is wider than the
/// measured shape); only the grounded fields are typed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeRow {
    /// The decode-table index this row describes.
    pub index: u32,
    /// The operator-visible slot name, if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether this row is the active decode (`1`) or not (`0`), if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch: Option<u8>,
    /// The vendor's play status token, if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streamplay_status: Option<String>,
    /// The configured stream URL, if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl DecodeRow {
    /// Whether this row is the active decode (`switch == 1`).
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self.switch, Some(SWITCH_ON))
    }

    /// The stream's transport kind, derived from the URL scheme when the vendor
    /// does not report a `proto` field (`rtsp` / `srt` / `http`).
    #[must_use]
    pub fn kind(&self) -> String {
        self.url
            .as_deref()
            .and_then(|url| url.split("://").next())
            .filter(|scheme| !scheme.is_empty() && !scheme.contains('/'))
            .unwrap_or("stream")
            .to_ascii_lowercase()
    }
}

/// The grounded decode table: the `data` array of `streamplay_get_all`.
///
/// Only one index is active at a time — see [`DecodeTable::active_index`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodeTable {
    rows: Vec<DecodeRow>,
}

impl DecodeTable {
    /// Build a table from its rows.
    #[must_use]
    pub fn new(rows: Vec<DecodeRow>) -> Self {
        Self { rows }
    }

    /// Parse the `data` block of a `streamplay_get_all` reply.
    ///
    /// The grounded shape is a top-level array; the legacy `{entries:[…]}`
    /// envelope is also accepted so existing fixtures keep working. Anything
    /// else parses to an empty table (never an error — an empty table simply
    /// grounds nothing, and index-scoped convergence then refuses).
    #[must_use]
    pub fn parse(data: &Value) -> Self {
        let array = data
            .as_array()
            .or_else(|| data.get("entries").and_then(Value::as_array));
        let Some(array) = array else {
            return Self::default();
        };
        let rows = array
            .iter()
            .filter_map(|row| {
                // Tolerate the legacy `{index,proto}` rows by mapping the
                // reported `proto` onto a synthetic URL only when no URL is
                // reported; everything else must carry the grounded shape.
                let index = row.get("index").and_then(Value::as_u64)?;
                let mut typed: DecodeRow = serde_json::from_value(row.clone()).ok()?;
                typed.index = u32::try_from(index).unwrap_or(u32::MAX);
                if typed.url.is_none() {
                    typed.url = row
                        .get("proto")
                        .and_then(Value::as_str)
                        .map(|proto| format!("{proto}://"));
                }
                Some(typed)
            })
            .collect();
        Self { rows }
    }

    /// Every row, in device order.
    #[must_use]
    pub fn rows(&self) -> &[DecodeRow] {
        &self.rows
    }

    /// Whether the table is empty (the device reported no decode slots).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The decode index currently active on the **single-active** decoder, if
    /// the device reports one.
    #[must_use]
    pub fn active_index(&self) -> Option<u32> {
        self.rows
            .iter()
            .find(|row| row.is_active())
            .map(|r| r.index)
    }

    /// Whether `index` exists in the table.
    #[must_use]
    pub fn contains(&self, index: u32) -> bool {
        self.rows.iter().any(|row| row.index == index)
    }

    /// The row for `index`, if present.
    #[must_use]
    pub fn row(&self, index: u32) -> Option<&DecodeRow> {
        self.rows.iter().find(|row| row.index == index)
    }
}

/// The `get_decoder_state` reply.
///
/// The measured comment grounds the **call** but not a field set, so this type
/// deliberately invents nothing: it is an opaque, lossless wrapper the driver
/// can log or hand on without claiming a schema the vendor has not been probed
/// for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecoderState(Value);

impl DecoderState {
    /// Wrap a raw reply body.
    #[must_use]
    pub const fn new(value: Value) -> Self {
        Self(value)
    }

    /// The raw reply body.
    #[must_use]
    pub fn raw(&self) -> &Value {
        &self.0
    }
}
