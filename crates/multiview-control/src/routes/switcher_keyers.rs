//! Downstream keyer verbs on the existing switcher routes (ADR-0113 §10,
//! design note `docs/architecture/switcher-keyers.md` §7, §8, slice K4):
//! `keyer_cut`, `keyer_auto`, `keyer_tie` and `keyer_set` through
//! `POST /api/v1/switcher/plan` and `POST /api/v1/switcher/commands/{kind}`,
//! plus the `keyers[]` / `tally` projection of `GET /api/v1/switcher/{me}`.
//!
//! Order of checks (plan and command), so nothing leaks and nothing is
//! burned: role → BOLA on every id the request names (`Control` on the M/E,
//! `Program` scope, `Control` on the keyer object id `<program>/<keyer>`,
//! `Read` on every named source) — **before** any existence answer and before
//! the `Idempotency-Key` is looked at. A scoped principal therefore cannot
//! probe which keyers or sources exist. Then shape (`in:<n>` refused 422);
//! for a command the key lookup (a replay answers its first 202 before any
//! state-dependent check, review H2); existence against the engine's
//! committed definitions, the on-air guard, admission (`capacity`, the Clean
//! rule), and for a command the reservation, the leases and the bounded
//! `try_send` (full → 503, key and leases released).
//!
//! The control plane has one object axis, so `Control` and `Read` on an id
//! are both the per-object grant (`authorize_object`); the role gate (`Write`
//! for a command) is what separates them, as for every other switcher verb.

use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use multiview_config::{
    DownstreamKeyerConfig, KeyerLossPolicy, KeyerLossTrigger, KeyerRect, KeyerSize, KeyerType,
    DEFAULT_KEYER_BUDGET, INPUT_REF_PREFIX, MAX_KEYER_RATE,
};
use multiview_core::switcher::FrameCount;
use multiview_engine::keyer::{KeyerDefinitions, KeyerSnapshot, TallyRole, TallySet};
use multiview_engine::switcher_control::{SwitcherControl, SwitcherSnapshot, SwitcherVerb};
use multiview_events::AuthzScope;
use serde::{Deserialize, Serialize};

use crate::auth::{authorize_object, authorize_scope, scope_permits, Principal};
use crate::error::{ControlError, ControlResult};
use crate::problem::Problem;
use crate::state::AppState;

/// The placeholder for a source id the caller may not read.
pub const REDACTED: &str = "redacted";

/// Whether `kind` is a keyer verb handled by this module.
#[must_use]
pub fn is_keyer_verb(kind: &str) -> bool {
    matches!(kind, "keyer_cut" | "keyer_auto" | "keyer_tie" | "keyer_set")
}

/// `on_air` guard of `keyer_set`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum KeyerOnAir {
    /// Refuse (409 `on-air-protected`) a fill or key change of a keyer on
    /// air — the default.
    #[default]
    Protect,
    /// Change the fill or key of a keyer on air (it needs both fresh now).
    Allow,
}

/// The keyer fields of a switcher request (flattened into it). Every field
/// is optional; which ones a verb takes is checked by shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KeyerFields {
    /// Keyer id within the program (`dsk1`); the object id is
    /// `<program>/<keyer>`. Required by every keyer verb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyer: Option<String>,
    /// `keyer_cut` / `keyer_auto`: the target state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    /// `keyer_tie`: tied to the next background take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tie: Option<bool>,
    /// `keyer_set`: fill source id (a source; `in:<n>` is refused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<String>,
    /// `keyer_set`: key source id; JSON `null` keeps it, `""` clears it (a
    /// self key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// `keyer_set`: `linear` or `luma` (`chroma` is refused until K7).
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub key_type: Option<KeyerType>,
    /// `keyer_set`: luma clip, `[0, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<f32>,
    /// `keyer_set`: luma gain, finite and `>= 0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain: Option<f32>,
    /// `keyer_set`: invert the matte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invert: Option<bool>,
    /// `keyer_set`: mask in fill-source pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub mask: Option<KeyerRect>,
    /// `keyer_set`: destination rectangle in canvas pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub fly: Option<KeyerRect>,
    /// `keyer_set`: admitted source envelope `{w, h}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub max_source: Option<KeyerSize>,
    /// `keyer_set`: AUTO rate in frames, 1..=300.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<u32>,
    /// `keyer_set`: `drop_to_off_air` or `hold_last_good`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub loss: Option<KeyerLossPolicy>,
    /// `keyer_set`: `stale`, `reconnecting` or `no_signal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub loss_trigger: Option<KeyerLossTrigger>,
    /// `keyer_set` defining a new keyer: its z among DSKs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z: Option<i32>,
    /// `keyer_set`: go on at boot through the fresh gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_at_start: Option<bool>,
    /// `keyer_set`: the on-air guard for a fill or key change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_air: Option<KeyerOnAir>,
}

impl KeyerFields {
    /// The source ids the request names (fill and a non-empty key).
    fn sources(&self) -> impl Iterator<Item = &str> {
        self.fill
            .as_deref()
            .into_iter()
            .chain(self.key.as_deref().filter(|k| !k.is_empty()))
    }

    /// Whether the request carries no keyer field at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keyer.is_none()
            && self.on.is_none()
            && self.tie.is_none()
            && !self.has_definition_fields()
    }

    fn has_definition_fields(&self) -> bool {
        self.fill.is_some()
            || self.key.is_some()
            || self.key_type.is_some()
            || self.clip.is_some()
            || self.gain.is_some()
            || self.invert.is_some()
            || self.mask.is_some()
            || self.fly.is_some()
            || self.max_source.is_some()
            || self.rate.is_some()
            || self.loss.is_some()
            || self.loss_trigger.is_some()
            || self.z.is_some()
            || self.on_at_start.is_some()
            || self.on_air.is_some()
    }
}

/// The keyer object id `<program>/<keyer>`.
#[must_use]
pub fn object_id(program: &str, keyer: &str) -> String {
    format!("{program}/{keyer}")
}

/// BOLA for a keyer verb (plan and command, before any existence answer):
/// the M/E object, the Program scope, the keyer object and every named
/// source. A missing `keyer` is a shape error, answered after this.
///
/// # Errors
/// 403 for any denied id.
pub fn authorize(
    principal: &Principal,
    me: &str,
    program: &str,
    fields: &KeyerFields,
) -> ControlResult<()> {
    authorize_object(principal, me)?;
    authorize_scope(principal, AuthzScope::Program(program))?;
    if let Some(keyer) = &fields.keyer {
        authorize_object(principal, &object_id(program, keyer))?;
    }
    for source in fields.sources() {
        authorize_object(principal, source)?;
    }
    Ok(())
}

fn typed(status: u16, kind: &str, detail: impl Into<String>) -> Response {
    Problem::new(status, kind, "Keyer request refused")
        .with_detail(detail)
        .into_response()
}

/// Shape: which fields a verb takes, `in:<n>` refused, keyer present.
fn shape(kind: &str, fields: &KeyerFields, other_refs: bool) -> Result<String, Box<Response>> {
    let Some(keyer) = fields.keyer.clone().filter(|k| !k.trim().is_empty()) else {
        return Err(Box::new(typed(422, "validation", "keyer is required")));
    };
    if keyer.contains('/') {
        return Err(Box::new(typed(422, "validation", "keyer id has no '/'")));
    }
    if other_refs {
        return Err(Box::new(typed(
            422,
            "validation",
            "keyer verbs take no source, input, layout, output or duration besides their own fields",
        )));
    }
    for source in fields.sources() {
        if source.starts_with(INPUT_REF_PREFIX) {
            return Err(Box::new(typed(
                422,
                "keyer-source-not-a-source",
                format!(
                    "{source:?} names a mixer input; keyer fill and key name sources only \
                     (a re-patch must never change what a keyer shows)"
                ),
            )));
        }
    }
    let ok = match kind {
        "keyer_cut" | "keyer_auto" => {
            fields.on.is_some() && fields.tie.is_none() && !fields.has_definition_fields()
        }
        "keyer_tie" => {
            fields.tie.is_some() && fields.on.is_none() && !fields.has_definition_fields()
        }
        "keyer_set" => fields.on.is_none() && fields.tie.is_none(),
        _ => false,
    };
    if ok {
        Ok(keyer)
    } else {
        Err(Box::new(typed(
            422,
            "validation",
            format!("invalid fields for {kind}"),
        )))
    }
}

/// The request-shape checks of a keyer verb (no engine or repository state):
/// `duration_frames` only on `keyer_auto`, the verb's own fields, `in:<n>`
/// refused, a keyer id. Returns the keyer id.
fn request_shape(req: &super::switcher::SwitcherRequest) -> Result<String, Box<Response>> {
    let kind = req.action.as_str();
    if req.duration_frames.is_some() && kind != "keyer_auto" {
        return Err(Box::new(typed(
            422,
            "validation",
            "duration_frames applies to keyer_auto only",
        )));
    }
    if kind == "keyer_auto" {
        auto_rate(req.duration_frames)?;
    }
    shape(kind, &req.keyer, other_refs(req))
}

/// A keyer verb validated against the engine, ready to enqueue.
pub struct Checked {
    /// The engine verb.
    pub verb: SwitcherVerb,
    /// The plan answer.
    pub plan: KeyerPlan,
    /// For `keyer_set`: whether this defines a new keyer (the Clean rule
    /// holds a DSK reservation on the gate from before the enqueue until it
    /// settles, review H3).
    pub defines_new: bool,
    /// For `keyer_set`: the fill and key leases the new definition needs.
    pub sources: Vec<String>,
}

/// Admission cost (ADR-0113 §9). Scoped callers see only their own delta;
/// the totals go to unscoped readers only (ADR-0108 decision 8).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KeyerCost {
    /// The change's cost delta in canvas folds (new cost − old cost).
    pub delta: f64,
    /// Whether it fits the remaining budget.
    pub fits: bool,
    /// Reserved total after the change (unscoped readers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved: Option<f64>,
    /// The keyer budget (unscoped readers only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<f64>,
    /// Every keyer holding a reservation, by object id (unscoped readers
    /// only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holders: Option<Vec<String>>,
}

/// The plan answer of a keyer verb.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KeyerPlan {
    /// `class1`: every keyer verb lands at one tick.
    pub class: &'static str,
    /// The keyer object id.
    pub keyer: String,
    /// Whether the keyer is on air now.
    pub on_air: bool,
    /// `keyer_set`: the admission cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<KeyerCost>,
    /// Whether the command can be taken now.
    pub can_take: bool,
    /// The refusal the command would answer, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<crate::routes::patch::PatchRefusal>,
}

fn refusal(kind: &str, detail: impl Into<String>) -> crate::routes::patch::PatchRefusal {
    crate::routes::patch::PatchRefusal {
        kind: kind.to_owned(),
        detail: detail.into(),
    }
}

/// The configured source ids (existence of a named source).
fn configured_sources(state: &AppState) -> ControlResult<Vec<String>> {
    Ok(crate::boot_model::accepted_resources(state, "sources")?
        .map_or_else(|| state.sources.list(), Ok)?
        .into_iter()
        .map(|v| v.resource.id)
        .collect())
}

/// Validate a keyer verb (after [`authorize`]) against the engine's committed
/// definitions and snapshot. A typed early answer is `Err(response)`; a
/// refusal the plan reports and the command answers with 409 rides
/// [`KeyerPlan::refusal`].
///
/// # Errors
/// Repository errors reading the configured sources.
pub fn check(
    state: &AppState,
    principal: &Principal,
    handle: &SwitcherControl,
    req: &super::switcher::SwitcherRequest,
) -> ControlResult<Result<Checked, Response>> {
    let kind = req.action.as_str();
    let fields = &req.keyer;
    let keyer = match request_shape(req) {
        Ok(keyer) => keyer,
        Err(response) => return Ok(Err(*response)),
    };
    let Some(defs) = handle.keyer_definitions() else {
        return Ok(Err(typed(
            409,
            "keyers-unavailable",
            "this run has no downstream keyer runtime",
        )));
    };
    let ctx = Ctx {
        object: object_id(&req.program, &keyer),
        snapshot: handle.snapshot(),
        defs,
    };
    if kind == "keyer_set" {
        return check_set(state, principal, handle, req, &keyer, &ctx);
    }
    let Some((index, _)) = ctx.defs.find(&keyer) else {
        return Ok(Err(typed(
            404,
            "unknown-keyer",
            format!("no downstream keyer {:?} is defined", ctx.object),
        )));
    };
    let on = fields.on.unwrap_or(false);
    let verb = match kind {
        "keyer_cut" => SwitcherVerb::KeyerCut { keyer: index, on },
        "keyer_auto" => {
            let rate = match auto_rate(req.duration_frames) {
                Ok(rate) => rate,
                Err(response) => return Ok(Err(*response)),
            };
            SwitcherVerb::KeyerAuto {
                keyer: index,
                on,
                rate,
            }
        }
        _ => SwitcherVerb::KeyerTie {
            keyer: index,
            tie: fields.tie.unwrap_or(false),
        },
    };
    let refused =
        (kind == "keyer_auto" && ctx.row(index).is_some_and(|r| r.ramp.is_some())).then(|| {
            refusal(
                "keyer-ramping",
                multiview_engine::keyer::REFUSE_KEYER_RAMPING,
            )
        });
    Ok(Ok(Checked {
        verb,
        plan: ctx.plan(Some(index), None, refused),
        defines_new: false,
        sources: Vec::new(),
    }))
}

/// What every keyer check reads once: the object id, the committed snapshot
/// and the committed definitions.
struct Ctx {
    object: String,
    snapshot: Option<Arc<SwitcherSnapshot>>,
    defs: Arc<KeyerDefinitions>,
}

impl Ctx {
    fn row(&self, index: u8) -> Option<KeyerSnapshot> {
        self.snapshot
            .as_ref()
            .and_then(|s| s.keyers.get(usize::from(index)).copied().flatten())
    }

    fn plan(
        &self,
        index: Option<u8>,
        cost: Option<KeyerCost>,
        refused: Option<crate::routes::patch::PatchRefusal>,
    ) -> KeyerPlan {
        KeyerPlan {
            class: "class1",
            keyer: self.object.clone(),
            on_air: index.and_then(|i| self.row(i)).is_some_and(|r| r.on_air),
            cost,
            can_take: refused.is_none(),
            refusal: refused,
        }
    }
}

/// A new definition with the documented defaults (fill required).
fn new_definition(
    keyer: &str,
    program: &str,
    fields: &KeyerFields,
    defs: &KeyerDefinitions,
) -> Result<DownstreamKeyerConfig, Box<Response>> {
    let Some(fill) = fields.fill.clone() else {
        return Err(Box::new(typed(
            422,
            "validation",
            "keyer_set defining a new keyer needs a fill",
        )));
    };
    Ok(DownstreamKeyerConfig {
        id: keyer.to_owned(),
        program: program.to_owned(),
        fill,
        key: None,
        key_type: KeyerType::Linear,
        clip: 0.0,
        gain: 1.0,
        invert: false,
        mask: None,
        fly: None,
        max_source: None,
        rate: multiview_config::DEFAULT_KEYER_RATE,
        tie: false,
        on_at_start: false,
        z: next_z(defs),
        loss: KeyerLossPolicy::default(),
        loss_trigger: KeyerLossTrigger::default(),
        premultiplied: false,
    })
}

/// `keyer_set`: merge the partial request onto the current definition (or
/// build a new one), validate it, then admit it (§7): the raised cost
/// against the remaining budget, the on-air guard and the Clean rule.
#[allow(clippy::too_many_lines)]
// reason: one linear admission sequence (merge → validate → slot → z →
// cost → guard → Clean → resolve) whose order is the contract.
fn check_set(
    state: &AppState,
    principal: &Principal,
    handle: &SwitcherControl,
    req: &super::switcher::SwitcherRequest,
    keyer: &str,
    ctx: &Ctx,
) -> ControlResult<Result<Checked, Response>> {
    let fields = &req.keyer;
    let defs = &ctx.defs;
    let found = defs.find(keyer);
    let mut cfg = if let Some((_, def)) = found {
        (*def.config).clone()
    } else {
        match new_definition(keyer, &req.program, fields, defs) {
            Ok(cfg) => cfg,
            Err(response) => return Ok(Err(*response)),
        }
    };
    if found.is_some() && fields.z.is_some_and(|z| z != cfg.z) {
        return Ok(Err(typed(
            422,
            "validation",
            "z is fixed once a keyer is defined (draw order is an attach-time property)",
        )));
    }
    let old_sources = (cfg.fill.clone(), cfg.key.clone());
    merge(&mut cfg, fields);
    let sources_change = (cfg.fill.clone(), cfg.key.clone()) != old_sources;
    let (canvas_w, canvas_h) = defs.canvas;
    let configured = configured_sources(state)?;
    let configured: Vec<&str> = configured.iter().map(String::as_str).collect();
    if let Err(error) = cfg.validate_definition(&configured, canvas_w, canvas_h) {
        return Ok(Err(typed(422, "validation", error.to_string())));
    }
    let Some(index) = found.map(|(i, _)| i).or_else(|| defs.free_index()) else {
        return Ok(Err(typed(
            409,
            "capacity",
            format!(
                "a program has at most {} downstream keyers",
                multiview_engine::keyer::MAX_DSK
            ),
        )));
    };
    let others = || {
        defs.defs
            .iter()
            .enumerate()
            .filter(move |(i, _)| *i != usize::from(index))
            .filter_map(|(_, d)| d.as_ref())
    };
    if others().any(|d| d.z == cfg.z) {
        return Ok(Err(typed(
            422,
            "validation",
            format!("another keyer already has z = {}", cfg.z),
        )));
    }
    let old_cost = found.map_or(0.0, |(_, d)| d.cost);
    let new_cost = cfg.cost(canvas_w, canvas_h);
    let reserved_others: f64 = others().map(|d| d.cost).sum();
    let budget = f64::from(if defs.budget.is_finite() {
        defs.budget
    } else {
        DEFAULT_KEYER_BUDGET
    });
    let fits = reserved_others + new_cost <= budget + 1e-9;
    let global = principal.is_global();
    let cost = KeyerCost {
        delta: new_cost - old_cost,
        fits,
        reserved: global.then_some(reserved_others + new_cost),
        budget: global.then_some(budget),
        holders: global.then(|| {
            defs.defs
                .iter()
                .flatten()
                .map(|d| d.object_id.to_string())
                .collect()
        }),
    };
    let engaged = found
        .and_then(|(i, _)| ctx.row(i))
        .is_some_and(|r| r.on_air || r.ramp.is_some() || r.pending);
    let allow = fields.on_air == Some(KeyerOnAir::Allow);
    let defines_new = found.is_none();
    let refused = if !fits {
        Some(refusal(
            "capacity",
            if global {
                format!(
                    "keyer cost {new_cost:.3} canvas folds exceeds the remaining budget \
                     ({reserved_others:.3} of {budget} reserved)"
                )
            } else {
                "the keyer budget cannot admit this definition".to_owned()
            },
        ))
    } else if engaged && sources_change && !allow {
        Some(refusal(
            "on-air-protected",
            multiview_engine::keyer::REFUSE_KEYER_ON_AIR_PROTECTED,
        ))
    } else if defines_new && clean_held(handle) {
        Some(refusal(
            "clean-unavailable-with-dsk",
            "an output carries Clean; defining a downstream keyer would put DSK pixels on it \
             (pre-DSK Clean is not built yet, #435)",
        ))
    } else {
        None
    };
    let def = match handle.resolve_keyer(&cfg) {
        Ok(def) => def,
        Err(detail) => return Ok(Err(typed(422, "validation", detail))),
    };
    let sources = std::iter::once(cfg.fill.clone())
        .chain(cfg.key.clone())
        .collect();
    Ok(Ok(Checked {
        verb: handle.keyer_set_verb(defs, index, def, allow),
        plan: ctx.plan(found.map(|(i, _)| i), Some(cost), refused),
        defines_new,
        sources,
    }))
}

/// The rate override of a `keyer_auto` (`duration_frames`, 1..=300 frames;
/// absent: the keyer's own rate).
fn auto_rate(duration_frames: Option<u32>) -> Result<Option<FrameCount>, Box<Response>> {
    match duration_frames {
        None => Ok(None),
        Some(rate) if (1..=MAX_KEYER_RATE).contains(&rate) => Ok(FrameCount::try_from(rate).ok()),
        Some(rate) => Err(Box::new(typed(
            422,
            "keyer-rate",
            format!("duration_frames {rate} is outside 1..={MAX_KEYER_RATE}"),
        ))),
    }
}

fn next_z(defs: &KeyerDefinitions) -> i32 {
    defs.defs
        .iter()
        .flatten()
        .map(|d| d.z)
        .max()
        .map_or(1, |z| z.saturating_add(1))
}

fn merge(cfg: &mut DownstreamKeyerConfig, f: &KeyerFields) {
    if let Some(fill) = &f.fill {
        cfg.fill.clone_from(fill);
    }
    if let Some(key) = &f.key {
        cfg.key = (!key.is_empty()).then(|| key.clone());
    }
    if let Some(t) = f.key_type {
        cfg.key_type = t;
    }
    if let Some(v) = f.clip {
        cfg.clip = v;
    }
    if let Some(v) = f.gain {
        cfg.gain = v;
    }
    if let Some(v) = f.invert {
        cfg.invert = v;
    }
    if f.mask.is_some() {
        cfg.mask = f.mask;
    }
    if f.fly.is_some() {
        cfg.fly = f.fly;
    }
    if f.max_source.is_some() {
        cfg.max_source = f.max_source;
    }
    if let Some(v) = f.rate {
        cfg.rate = v;
    }
    if let Some(v) = f.loss {
        cfg.loss = v;
    }
    if let Some(v) = f.loss_trigger {
        cfg.loss_trigger = v;
    }
    if let Some(v) = f.z {
        cfg.z = v;
    }
    if let Some(v) = f.on_at_start {
        cfg.on_at_start = v;
    }
}

/// Whether an output carries Clean now or has a take to it pending: the
/// router holds a Clean reservation on the shared gate for each (advisory,
/// for the plan; the command admits through [`reserve_dsk`]).
fn clean_held(handle: &SwitcherControl) -> bool {
    handle.clean_gate().clean_reservations() > 0
}

/// Reserve "a DSK is being defined" on the Clean / DSK gate for a
/// `keyer_set` defining a new keyer (review H3): one CAS that succeeds only
/// while no output holds a Clean reservation, and from then on refuses
/// every Clean take until this reservation drops. Counted, so a sibling
/// request that fails can never release this one. `None`: Clean is routed
/// or a take to it is pending.
#[must_use]
pub fn reserve_dsk(
    handle: &SwitcherControl,
) -> Option<multiview_engine::clean_gate::DskReservation> {
    handle.clean_gate().try_reserve_dsk()
}

/// Take make-before-break leases on a new definition's fill and key before
/// the enqueue (the boot keyer set takes them through the demand manager; a
/// committed `keyer_set` republishes it). `None` when a lease is refused.
#[must_use]
pub fn take_leases(
    state: &AppState,
    object: &str,
    sources: &[String],
) -> Option<Vec<crate::source_demand::SwitcherPreviewLease>> {
    let Some(demand) = state.source_demand.as_ref() else {
        return Some(Vec::new());
    };
    let label = format!("keyer:{object}");
    let mut leases = Vec::with_capacity(sources.len());
    for source in sources {
        let lease = demand.preview_lease(source, &label)?;
        leases.push(crate::source_demand::SwitcherPreviewLease::new(lease));
    }
    Some(leases)
}

/// One `keyers[]` row of `GET /api/v1/switcher/{me}` (design note §8).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[allow(clippy::struct_excessive_bools)]
// reason: independent wire flags of one keyer row (design note §8).
pub struct KeyerRow {
    /// Keyer id within the program.
    pub id: String,
    /// `dsk` (USKs arrive with K3).
    pub family: &'static str,
    /// Fill source id, or `"redacted"`.
    pub fill: String,
    /// Key source id, `"redacted"`, or `null` (self key).
    pub key: Option<String>,
    /// `linear` or `luma`.
    #[serde(rename = "type")]
    pub key_type: &'static str,
    /// Destination rectangle, `null` = full canvas.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub fly: Option<KeyerRect>,
    /// Mask in fill-source pixels.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub mask: Option<KeyerRect>,
    /// On air.
    pub on_air: bool,
    /// Tied to the next background take.
    pub tie: bool,
    /// `{start_tick, rate, to_on}` while ramping; clients compute the weight.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Object>))]
    pub ramp: Option<serde_json::Value>,
    /// AUTO rate in frames.
    pub rate: u32,
    /// `drop_to_off_air` or `hold_last_good`.
    pub loss: &'static str,
    /// Drawing a frozen pair past its loss trigger.
    pub stale: bool,
    /// An operator command is held waiting for fresh sources.
    pub pending: bool,
}

/// The engine tally resolved to source ids (unreadable ids dropped).
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TallyView {
    /// Sources contributing to Program.
    pub program: Vec<String>,
    /// Sources on Preview and keys going on next.
    pub preview: Vec<String>,
}

fn readable(principal: &Principal, id: &str) -> bool {
    scope_permits(&principal.scopes(), AuthzScope::Object(id))
}

fn redact(principal: &Principal, id: &str) -> String {
    if readable(principal, id) {
        id.to_owned()
    } else {
        REDACTED.to_owned()
    }
}

/// The `keyers[]` rows the principal may read (ADR-W025 row filtering by
/// keyer object id), with unreadable source ids redacted.
#[must_use]
pub fn rows(
    principal: &Principal,
    defs: Option<&KeyerDefinitions>,
    snapshot: &SwitcherSnapshot,
) -> Vec<KeyerRow> {
    let Some(defs) = defs else {
        return Vec::new();
    };
    defs.defs
        .iter()
        .zip(snapshot.keyers.iter())
        .filter_map(|(def, state)| {
            let def = def.as_ref()?;
            if !readable(principal, &def.object_id) {
                return None;
            }
            let state = state.unwrap_or(KeyerSnapshot {
                on_air: false,
                tie: def.tie,
                ramp: None,
                stale: false,
                pending: false,
            });
            let cfg = &def.config;
            Some(KeyerRow {
                id: cfg.id.clone(),
                family: "dsk",
                fill: redact(principal, &cfg.fill),
                key: cfg.key.as_deref().map(|k| redact(principal, k)),
                key_type: match cfg.key_type {
                    KeyerType::Luma => "luma",
                    _ => "linear",
                },
                fly: cfg.fly,
                mask: cfg.mask,
                on_air: state.on_air,
                tie: state.tie,
                ramp: state.ramp.map(|r| {
                    serde_json::json!({
                        "start_tick": r.start_tick,
                        "rate": r.rate,
                        "to_on": r.to_on,
                    })
                }),
                rate: cfg.rate,
                loss: match cfg.loss {
                    KeyerLossPolicy::HoldLastGood => "hold_last_good",
                    _ => "drop_to_off_air",
                },
                stale: state.stale,
                pending: state.pending,
            })
        })
        .collect()
}

/// Resolve the tally roles to source ids, dropping ids the principal may
/// not read (and a keyer role whose keyer row it may not read).
#[must_use]
pub fn tally(
    principal: &Principal,
    defs: Option<&KeyerDefinitions>,
    snapshot: &SwitcherSnapshot,
) -> TallyView {
    let resolve = |set: &TallySet| -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for role in set.roles() {
            let id = match role {
                TallyRole::Program => Some(snapshot.program.as_str().to_owned()),
                TallyRole::Preview => Some(snapshot.preview.as_str().to_owned()),
                TallyRole::Fill(k) | TallyRole::Key(k) => defs
                    .and_then(|d| d.defs.get(usize::from(k)))
                    .and_then(Option::as_ref)
                    .filter(|d| readable(principal, &d.object_id))
                    .and_then(|d| match role {
                        TallyRole::Fill(_) => Some(d.fill_id.to_string()),
                        _ => d.key_id.as_deref().map(str::to_owned),
                    }),
            };
            if let Some(id) = id {
                if readable(principal, &id) && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids
    };
    TallyView {
        program: resolve(&snapshot.tally.program),
        preview: resolve(&snapshot.tally.preview),
    }
}

/// Relay committed keyer events onto the shared publisher as
/// `switcher.keyer` (off the clock; bounded drop-oldest on both sides). The
/// event carries real source ids; the per-client projection scopes it by
/// M/E, program and keyer object and redacts unreadable sources.
pub async fn forward_keyer_events(
    control: SwitcherControl,
    publisher: Arc<
        multiview_engine::isolation::EnginePublisher<
            crate::state::EngineStateSnapshot,
            multiview_events::Event,
        >,
    >,
    me: String,
    program: String,
) {
    use multiview_engine::isolation::RecvError;
    let mut events = control.keyer_events();
    loop {
        match events.recv().await {
            Ok(item) => {
                let event = &item.event;
                // Definitions are published before the tick's events.
                let Some(defs) = control.keyer_definitions() else {
                    continue;
                };
                let Some(def) = defs
                    .defs
                    .get(usize::from(event.keyer))
                    .and_then(Option::as_ref)
                else {
                    continue;
                };
                publisher.publish_event(multiview_events::Event::SwitcherKeyer(
                    multiview_events::SwitcherKeyer {
                        me: me.clone(),
                        program: program.clone(),
                        keyer: def.object_id.to_string(),
                        tick: event.tick,
                        on_air: event.on_air,
                        stale: event.stale,
                        reason: reason(event.reason).to_owned(),
                        fill: def.fill_id.to_string(),
                        key: def.key_id.as_deref().map(str::to_owned),
                    },
                ));
            }
            Err(RecvError::Lagged(_)) => {
                // A lagged relay: clients re-fetch GET /switcher/{me}.
            }
            Err(RecvError::Closed) => break,
        }
    }
}

fn reason(reason: multiview_engine::keyer::KeyerReason) -> &'static str {
    use multiview_engine::keyer::KeyerReason;
    match reason {
        KeyerReason::Operator => "operator",
        KeyerReason::Tie => "tie",
        KeyerReason::OnAtStart => "on-at-start",
        KeyerReason::SourceLost => "source-lost",
        KeyerReason::SourceRecovered => "source-recovered",
        KeyerReason::Definition => "definition",
    }
}

/// The plan answer of a keyer verb (`POST /api/v1/switcher/plan`), after
/// [`authorize`]. No change.
///
/// # Errors
/// Repository errors.
pub fn plan(
    state: &AppState,
    principal: &Principal,
    handle: &SwitcherControl,
    req: &super::switcher::SwitcherRequest,
) -> ControlResult<Response> {
    Ok(match check(state, principal, handle, req)? {
        Ok(checked) => (axum::http::StatusCode::OK, axum::Json(checked.plan)).into_response(),
        Err(response) => response,
    })
}

fn other_refs(req: &super::switcher::SwitcherRequest) -> bool {
    req.source.is_some() || req.input.is_some() || req.layout.is_some() || req.output.is_some()
}

/// A pending `keyer_set`: the leases it took (make-before-break) and the
/// definition it enqueued, settled only by a terminal engine answer (its
/// outcome, or the clock past its deadline tick, or the run ended).
struct PendingSet {
    op: String,
    def: Arc<multiview_engine::keyer::KeyerDef>,
    /// The verb's engine deadline tick: once the clock composed it, the
    /// command can no longer apply (review H4).
    deadline: u64,
    /// Held until the outcome settles (RAII; dropping releases them).
    #[allow(dead_code)]
    // reason: an RAII lease vector, kept only for its `Drop`.
    leases: Vec<crate::source_demand::SwitcherPreviewLease>,
    /// The DSK reservation of a `keyer_set` defining a new keyer (RAII:
    /// dropping it releases exactly this count; review H3). Held until a
    /// terminal engine answer; by then an applied definition has set the
    /// gate's committed bit at its tick.
    #[allow(dead_code)]
    // reason: an RAII reservation, kept only for its `Drop`.
    dsk: Option<multiview_engine::clean_gate::DskReservation>,
}

/// How often a settling `keyer_set` that has seen no outcome re-reads the
/// clock's composed tick (a lagged outcome ring, or a stopped run). Not a
/// deadline: settlement never ends before a terminal engine answer.
pub const KEYER_SET_RECHECK: std::time::Duration = std::time::Duration::from_millis(500);

/// Enqueue a checked keyer verb (`POST /api/v1/switcher/commands/{kind}`),
/// after [`authorize`]: typed refusals as 409, the `Idempotency-Key` (a
/// replay answers its first 202), for `keyer_set` the leases and the Clean
/// rule, then the bounded `try_send` (full → 503, key and leases released).
///
/// # Errors
/// Missing key, idempotency capacity, a full mailbox, repository errors.
pub fn submit(
    state: &AppState,
    principal: &Principal,
    handle: &SwitcherControl,
    idem: &crate::concurrency::IdempotencyKey,
    req: &super::switcher::SwitcherRequest,
    scene_version: u64,
) -> ControlResult<Response> {
    use crate::concurrency::Reservation;
    // Authorization ran in the caller; the request shape is stateless.
    if let Err(response) = request_shape(req) {
        return Ok(*response);
    }
    let Some(key) = idem.0.as_deref().filter(|key| !key.trim().is_empty()) else {
        return Err(ControlError::Validation(
            "Idempotency-Key is required".into(),
        ));
    };
    // A retry answers its first 202 before any state-dependent admission
    // (review H2): the ramp a first `keyer_auto` started, or the DSK a first
    // `keyer_set` defined, must not turn its own retry into a 409. The key
    // is scoped per principal and path (`IdempotencyKey`), so this never
    // answers another principal's operation.
    if let Some(op) = state.idempotency.peek(key) {
        return Ok(accepted(op.to_string(), &req.action));
    }
    let checked = match check(state, principal, handle, req)? {
        Ok(checked) => checked,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = &checked.plan.refusal {
        return Ok(typed(409, &refused.kind, refused.detail.clone()));
    }
    let op = match state.idempotency.reserve(Some(key)) {
        Reservation::Capacity => return Err(ControlError::IdempotencyCapacity),
        Reservation::Replay(op) => return Ok(accepted(op.to_string(), &req.action)),
        Reservation::Fresh(op) => op,
    };
    let mut pending = None;
    if let SwitcherVerb::KeyerSet { def, deadline, .. } = &checked.verb {
        // The one admission point for the Clean rule (review H3): the
        // reservation is taken before the enqueue and lives in `pending`.
        let dsk = if checked.defines_new {
            let Some(dsk) = reserve_dsk(handle) else {
                state.idempotency.release(Some(key), &op);
                return Ok(typed(
                    409,
                    "clean-unavailable-with-dsk",
                    "an output carries Clean; defining a downstream keyer would put DSK pixels on it",
                ));
            };
            Some(dsk)
        } else {
            None
        };
        let Some(leases) = take_leases(state, &checked.plan.keyer, &checked.sources) else {
            state.idempotency.release(Some(key), &op);
            // Dropping `dsk` releases this request's reservation only.
            drop(dsk);
            return Ok(typed(
                409,
                "capacity",
                "source lease unavailable (unknown source or lease limit)",
            ));
        };
        pending = Some(PendingSet {
            op: op.to_string(),
            def: Arc::clone(&def.0),
            deadline: *deadline,
            leases,
            dsk,
        });
    }
    // Subscribed before the enqueue, so the outcome cannot be missed.
    let outcomes = pending.as_ref().map(|_| handle.outcomes());
    let command = multiview_engine::switcher_control::SwitcherCommand {
        operation_id: op.to_string(),
        scene_version,
        verb: checked.verb,
    };
    if handle.try_send(command).is_err() {
        state.idempotency.release(Some(key), &op);
        // Dropping `pending` releases every lease and the DSK reservation it
        // took: this request's own count, never a sibling's (review H3).
        drop(pending);
        return Err(ControlError::EngineBusy);
    }
    if let (Some(pending), Some(outcomes)) = (pending, outcomes) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = state.clone();
            let handle = handle.clone();
            runtime.spawn(Box::pin(settle_set(state, handle, outcomes, pending)));
        }
    }
    Ok(accepted(op.to_string(), &req.action))
}

fn accepted(operation_id: String, kind: &str) -> Response {
    (
        axum::http::StatusCode::ACCEPTED,
        axum::Json(super::switcher::SwitcherAccepted {
            operation_id,
            kind: kind.to_owned(),
        }),
    )
        .into_response()
}

/// Settle one `keyer_set` off the clock: wait for a terminal engine answer
/// — its outcome, or the clock having composed its deadline tick (the engine
/// refuses it from then on, so it can no longer apply), or the run ended —
/// never a wall-clock deadline (review H4: releasing while the command could
/// still apply orphaned it). Then decide from the committed definitions (the
/// authority, so a lagged outcome ring cannot mislead it); on `applied`
/// republish the keyer demand set and persist the definitions; in every case
/// release the leases and the DSK reservation it took.
async fn settle_set(
    state: AppState,
    handle: SwitcherControl,
    mut outcomes: multiview_engine::isolation::EventSubscription<
        multiview_engine::switcher_control::SwitcherOutcome,
    >,
    pending: PendingSet,
) {
    // Terminal without an outcome: the clock composed the deadline tick (a
    // drain at or after it refuses the verb; every earlier drain has
    // committed or been rejected by then), or the run is gone.
    let settled =
        |handle: &SwitcherControl| handle.applied_tick() >= pending.deadline || handle.is_closed();
    loop {
        match tokio::time::timeout(KEYER_SET_RECHECK, outcomes.recv()).await {
            Ok(Ok(item)) if item.event.operation_id == pending.op => break,
            Ok(Err(multiview_engine::isolation::RecvError::Closed)) => break,
            Ok(Ok(_) | Err(multiview_engine::isolation::RecvError::Lagged(_))) | Err(_) => {
                if settled(&handle) {
                    break;
                }
            }
        }
    }
    let applied = handle.keyer_definitions().is_some_and(|defs| {
        defs.defs
            .iter()
            .flatten()
            .any(|d| Arc::ptr_eq(d, &pending.def))
    });
    if applied {
        if let Some(defs) = handle.keyer_definitions() {
            if let Some(set) = handle.keyer_demand() {
                set.publish(demand_items(&defs));
            }
            if let Err(error) = persist_keyers(&state, &defs).await {
                tracing::warn!(%error, "keyer_set: applied definition not persisted");
            }
        }
    }
    // The demand set now holds the new sources (make-before-break): the
    // request's own leases are released here, and its DSK reservation (an
    // applied definition already set the gate's committed bit at its tick).
    drop(pending);
}

/// The `keyer` demand items of the committed definitions (one lease per fill
/// and key, labelled with the keyer object id), as at boot.
#[must_use]
pub fn demand_items(defs: &KeyerDefinitions) -> Vec<multiview_engine::SetItem> {
    let mut items = Vec::new();
    for def in defs.defs.iter().flatten() {
        let label: Arc<str> = Arc::clone(&def.object_id);
        for source in std::iter::once(&def.fill_id).chain(def.key_id.as_ref()) {
            items.push(multiview_engine::SetItem {
                id: Arc::clone(source),
                label: Arc::clone(&label),
                size: None,
            });
        }
    }
    items
}

/// Write the committed definitions as `switcher.downstream_keyers` of the
/// accepted state (desired state; written only after the engine applied
/// them, under the config-mutation lock). On-air state is never written.
///
/// # Errors
/// The accepted document cannot be read, validated or committed.
pub async fn persist_keyers(state: &AppState, defs: &KeyerDefinitions) -> ControlResult<()> {
    if state.boot_model.is_none() {
        return Ok(());
    }
    let _mutation = state.lock_config_mutation().await;
    let mut config: multiview_config::MultiviewConfig =
        serde_json::from_value(crate::boot_model::current_desired_config(state)?)
            .map_err(|e| ControlError::Repository(format!("desired configuration: {e}")))?;
    let Some(switcher) = config.switcher.as_mut() else {
        return Ok(());
    };
    switcher.downstream_keyers = defs
        .defs
        .iter()
        .flatten()
        .map(|d| (*d.config).clone())
        .collect();
    config
        .validate()
        .map_err(|e| ControlError::Validation(e.to_string()))?;
    let layouts = crate::boot_model::layout_library(state)?;
    crate::boot_model::commit_desired(state, config, layouts).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use multiview_core::switcher::SourceRef;
    use multiview_engine::keyer::{KeyerTally, MAX_DSK};

    fn principal(objects: &[&str]) -> Principal {
        Principal {
            key_id: "t".into(),
            role: crate::auth::Role::Operator,
            scoped_object_ids: Some(objects.iter().map(|o| (*o).to_owned()).collect()),
            scoped_output_ids: None,
            scoped_discovery_domains: None,
        }
    }

    fn defs() -> KeyerDefinitions {
        let cfg: DownstreamKeyerConfig =
            toml::from_str("id = \"dsk1\"\nprogram = \"main\"\nfill = \"gfx\"\nkey = \"matte\"\n")
                .unwrap();
        let store = Arc::new(multiview_framestore::TileStore::with_defaults("x"));
        let stores: std::collections::HashMap<_, _> = [
            ("gfx".to_owned(), Arc::clone(&store)),
            ("matte".to_owned(), store),
        ]
        .into_iter()
        .collect();
        let def = multiview_engine::keyer::KeyerDef::from_config(&cfg, &stores, 64, 64).unwrap();
        let mut list: [Option<Arc<multiview_engine::keyer::KeyerDef>>; MAX_DSK] =
            [const { None }; MAX_DSK];
        list[0] = Some(Arc::new(def));
        KeyerDefinitions {
            version: 0,
            defs: list,
            canvas: (64, 64),
            budget: 3.0,
        }
    }

    fn snapshot() -> SwitcherSnapshot {
        SwitcherSnapshot {
            tick: 1,
            program: SourceRef::new("cam1"),
            preview: SourceRef::new("cam2"),
            program_input: None,
            preview_input: None,
            inputs_version: 0,
            in_flight: false,
            completion_count: 0,
            keyers: [None; MAX_DSK],
            tally: KeyerTally {
                program: TallySet::from_roles([
                    TallyRole::Program,
                    TallyRole::Fill(0),
                    TallyRole::Key(0),
                ]),
                preview: TallySet::from_roles([TallyRole::Preview]),
            },
        }
    }

    #[test]
    fn tally_resolves_roles_to_ids_and_drops_unreadable_ones() {
        let defs = defs();
        let snapshot = snapshot();
        let unscoped = Principal::local_admin();
        let view = tally(&unscoped, Some(&defs), &snapshot);
        assert_eq!(view.program, vec!["cam1", "gfx", "matte"]);
        assert_eq!(view.preview, vec!["cam2"]);
        // A key source the caller cannot read is dropped, never redacted.
        let scoped = principal(&["cam1", "cam2", "gfx", "main/dsk1"]);
        assert_eq!(
            tally(&scoped, Some(&defs), &snapshot).program,
            vec!["cam1", "gfx"]
        );
        // An unreadable keyer row contributes nothing, even readable sources.
        let no_keyer = principal(&["cam1", "cam2", "gfx", "matte"]);
        assert_eq!(
            tally(&no_keyer, Some(&defs), &snapshot).program,
            vec!["cam1"]
        );
    }

    #[test]
    fn rows_filter_by_keyer_object_and_redact_sources() {
        let defs = defs();
        let snapshot = snapshot();
        assert!(rows(&principal(&["gfx", "matte"]), Some(&defs), &snapshot).is_empty());
        let row = &rows(&principal(&["main/dsk1", "gfx"]), Some(&defs), &snapshot)[0];
        assert_eq!(row.fill, "gfx");
        assert_eq!(row.key.as_deref(), Some(REDACTED));
        assert!(!row.on_air, "no snapshot row reads as off");
    }

    #[test]
    fn the_keyer_scope_needs_all_three_grants() {
        let scope = AuthzScope::Keyer {
            me: "me1",
            program: "main",
            keyer: "main/dsk1",
        };
        let with = |objects: &[&str], program: bool| {
            let mut p = principal(objects);
            p.scoped_output_ids = Some(if program {
                vec!["program:main".to_owned()]
            } else {
                Vec::new()
            });
            scope_permits(&p.scopes(), scope)
        };
        assert!(with(&["me1", "main/dsk1"], true));
        assert!(!with(&["main/dsk1"], true));
        assert!(!with(&["me1"], true));
        assert!(!with(&["me1", "main/dsk1"], false));
        assert!(scope_permits(&Principal::local_admin().scopes(), scope));
    }
}
