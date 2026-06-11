//! Severity **roll-up** (probe → tile → group → system) and **Boolean virtual
//! alarms** (AND / OR / XOR) (ADR-MV001 / broadcast-multiviewer §4).
//!
//! Roll-up is the X.733 aggregation: a parent's severity is the **maximum** over
//! its children, which is exactly [`PerceivedSeverity::rollup`] (the variants are
//! ordered `Cleared < … < Critical`). A [`RollupNode`] composes a tree of named
//! children so a whole tile/group/system severity is one fold.
//!
//! A [`VirtualAlarm`] combines several input alarms with a Boolean operator —
//! "all three feeds are black" (AND), "any audio fault" (OR), "exactly one path
//! down" (XOR) — and reports a chosen severity when its predicate fires. All of
//! this is pure arithmetic over already-evaluated severities; nothing here
//! touches a clock, a channel or the hot path.
use multiview_core::alarm::PerceivedSeverity;
use serde::{Deserialize, Serialize};

/// A node in the probe → tile → group → system severity tree.
///
/// A node's own [`severity`](RollupNode::rolled_up) is the **maximum** of its
/// intrinsic severity (a leaf probe's current severity) and every child's
/// rolled-up severity. Build leaves with [`RollupNode::leaf`] and parents with
/// [`RollupNode::group`] / [`RollupNode::with_child`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollupNode {
    /// A label for this node (probe id, tile index, group name, "system").
    pub label: String,
    /// This node's intrinsic severity (a leaf's own value; usually
    /// [`PerceivedSeverity::Cleared`] for pure grouping nodes).
    pub own: PerceivedSeverity,
    /// Child nodes whose severities roll up into this one.
    pub children: Vec<RollupNode>,
}

impl RollupNode {
    /// A leaf node carrying its own severity and no children.
    #[must_use]
    pub fn leaf(label: impl Into<String>, own: PerceivedSeverity) -> Self {
        Self {
            label: label.into(),
            own,
            children: Vec::new(),
        }
    }

    /// A grouping node (own severity [`PerceivedSeverity::Cleared`]) with the
    /// given children.
    #[must_use]
    pub fn group(label: impl Into<String>, children: Vec<RollupNode>) -> Self {
        Self {
            label: label.into(),
            own: PerceivedSeverity::Cleared,
            children,
        }
    }

    /// Builder: append a child.
    #[must_use]
    pub fn with_child(mut self, child: RollupNode) -> Self {
        self.children.push(child);
        self
    }

    /// The rolled-up severity of this subtree: the maximum of this node's own
    /// severity and every descendant's rolled-up severity.
    ///
    /// This is the X.733 probe → tile → group → system roll-up. A leaf rolls up
    /// to its own severity; an empty subtree rolls up to its own severity (which
    /// for a pure group is [`PerceivedSeverity::Cleared`]).
    #[must_use]
    pub fn rolled_up(&self) -> PerceivedSeverity {
        let children_max =
            PerceivedSeverity::rollup(self.children.iter().map(RollupNode::rolled_up));
        self.own.max(children_max)
    }
}

/// A Boolean combinator for [`VirtualAlarm`].
///
/// Serialised **tagged** by variant name (repo convention — never `untagged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BoolOp {
    /// Fires only when **every** input is active.
    And,
    /// Fires when **at least one** input is active.
    Or,
    /// Fires when an **odd** number of inputs are active (parity / "exactly one"
    /// for two inputs).
    Xor,
}

impl BoolOp {
    /// Evaluate this operator over a sequence of input *active* booleans.
    ///
    /// * [`BoolOp::And`] — `true` iff there is at least one input and all are
    ///   active (an empty input set is `false`: nothing is "all active").
    /// * [`BoolOp::Or`] — `true` iff any input is active.
    /// * [`BoolOp::Xor`] — `true` iff an odd number of inputs are active.
    #[must_use]
    pub fn evaluate<I: IntoIterator<Item = bool>>(self, inputs: I) -> bool {
        let mut any = false;
        let mut all = true;
        let mut parity = false;
        let mut count = 0_usize;
        for active in inputs {
            count += 1;
            any |= active;
            all &= active;
            parity ^= active;
        }
        match self {
            Self::And => count > 0 && all,
            Self::Or => any,
            Self::Xor => parity,
        }
    }
}

/// A virtual / Boolean alarm: combine several input alarms' active states with a
/// [`BoolOp`] and report a configured severity when the predicate fires.
///
/// Inputs are referenced by label so the control plane can wire arbitrary probes
/// or tiles into a group ("all cameras black", "any encoder fault"). Evaluation
/// is pure: feed it the active state of each named input via
/// [`VirtualAlarm::evaluate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualAlarm {
    /// The group's name.
    pub name: String,
    /// The Boolean combinator.
    pub op: BoolOp,
    /// Labels of the input alarms this group combines.
    pub inputs: Vec<String>,
    /// The severity reported when the predicate fires.
    pub severity: PerceivedSeverity,
}

impl VirtualAlarm {
    /// Construct a virtual alarm.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        op: BoolOp,
        inputs: Vec<String>,
        severity: PerceivedSeverity,
    ) -> Self {
        Self {
            name: name.into(),
            op,
            inputs,
            severity,
        }
    }

    /// Whether the group's predicate fires, given the active state of each input
    /// (in `inputs` order).
    ///
    /// The `active` iterator is zipped with [`inputs`](VirtualAlarm::inputs);
    /// inputs without a corresponding `active` value are treated as **inactive**,
    /// and extra `active` values are ignored.
    #[must_use]
    pub fn evaluate<I: IntoIterator<Item = bool>>(&self, active: I) -> bool {
        let mut states = active.into_iter();
        let resolved = self
            .inputs
            .iter()
            .map(|_| states.next().unwrap_or(false))
            .collect::<Vec<_>>();
        self.op.evaluate(resolved)
    }

    /// The severity to report given the active state of each input: the
    /// configured [`severity`](VirtualAlarm::severity) when the predicate fires,
    /// otherwise [`PerceivedSeverity::Cleared`].
    #[must_use]
    pub fn severity_for<I: IntoIterator<Item = bool>>(&self, active: I) -> PerceivedSeverity {
        if self.evaluate(active) {
            self.severity
        } else {
            PerceivedSeverity::Cleared
        }
    }
}
