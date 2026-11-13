//! Numbered mixer inputs (ADR-0110, `docs/architecture/output-routing.md`
//! §12.2): a fixed-size table, owned by the switcher tick driver, from input
//! number to the source patched behind it.
//!
//! Invariant #1: the table is allocated once before the clock starts (one slot
//! per number up to the highest declared input, at most 65 slots). A lookup on
//! the clock is an index plus an integer parse, never an allocation or a lock.
//! The only writes happen on a patch tick, applied from a bounded command.

use multiview_core::switcher::SourceRef;

/// Highest input number (and the most inputs one M/E declares).
pub const MAX_INPUT_NUMBER: u16 = 64;

/// Maximum crosspoints in one mixer patch command.
pub const MAX_PATCH_MEMBERS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputSlot {
    label: Option<String>,
    source: Option<SourceRef>,
}

/// The engine's input table. `slots[n]` is input `n`; `None` = not declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixerInputs {
    slots: Vec<Option<InputSlot>>,
}

/// One input as published on the readback (off the hot path, on change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputReadback {
    /// Input number, 1..=64.
    pub number: u16,
    /// Operator label (configuration, never changed by a patch).
    pub label: Option<String>,
    /// Source currently patched; `None` = unpatched.
    pub source: Option<SourceRef>,
}

/// The engine's live input table at a committed tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputsReadback {
    /// Output tick on which this table was first on air.
    pub tick: u64,
    /// Bumped once per applied patch.
    pub version: u64,
    /// Declared inputs in ascending number order.
    pub inputs: Vec<InputReadback>,
}

impl MixerInputs {
    /// Build the table from `(number, label, source)` entries.
    ///
    /// # Errors
    /// Refuses an empty table, a number outside 1..=64 and a duplicate.
    pub fn new(
        entries: impl IntoIterator<Item = (u16, Option<String>, Option<SourceRef>)>,
    ) -> Result<Self, String> {
        let entries: Vec<_> = entries.into_iter().collect();
        let Some(max) = entries.iter().map(|(n, _, _)| *n).max() else {
            return Err("a mixer input table needs at least one input".into());
        };
        if max > MAX_INPUT_NUMBER || entries.iter().any(|(n, _, _)| *n == 0) {
            return Err(format!("mixer input numbers are 1..={MAX_INPUT_NUMBER}"));
        }
        let mut slots = vec![None; usize::from(max) + 1];
        for (number, label, source) in entries {
            let slot = &mut slots[usize::from(number)];
            if slot.is_some() {
                return Err(format!("mixer input {number} is declared twice"));
            }
            *slot = Some(InputSlot { label, source });
        }
        Ok(Self { slots })
    }

    fn slot(&self, number: u16) -> Option<&InputSlot> {
        self.slots.get(usize::from(number)).and_then(Option::as_ref)
    }

    /// Whether input `number` is declared.
    #[must_use]
    pub fn is_declared(&self, number: u16) -> bool {
        self.slot(number).is_some()
    }

    /// The source patched to input `number`, if declared and patched.
    #[must_use]
    pub fn source(&self, number: u16) -> Option<&SourceRef> {
        self.slot(number).and_then(|slot| slot.source.as_ref())
    }

    /// Resolve an `in:<n>` reference to its patched source; any other ref (or
    /// an unpatched input, which admission never lets on a bus) is returned
    /// unchanged. Allocation-free.
    #[must_use]
    pub fn resolve<'a>(&'a self, reference: &'a SourceRef) -> &'a SourceRef {
        reference
            .as_input()
            .and_then(|number| self.source(number))
            .unwrap_or(reference)
    }

    /// Patch (or unpatch) one declared input. Called only on a patch tick;
    /// the `SourceRef` arrives already allocated from the control plane.
    pub(crate) fn set(&mut self, number: u16, source: Option<SourceRef>) {
        if let Some(Some(slot)) = self.slots.get_mut(usize::from(number)) {
            slot.source = source;
        }
    }

    /// The lowest declared input carrying `source`.
    #[must_use]
    pub fn input_for_source(&self, source: &str) -> Option<u16> {
        self.slots.iter().enumerate().find_map(|(number, slot)| {
            slot.as_ref()
                .filter(|slot| slot.source.as_ref().is_some_and(|s| s.as_str() == source))
                .and_then(|_| u16::try_from(number).ok())
        })
    }

    /// An owned readback (allocates; only on a patch tick or before start).
    #[must_use]
    pub fn readback(&self, tick: u64, version: u64) -> InputsReadback {
        InputsReadback {
            tick,
            version,
            inputs: self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(number, slot)| {
                    let slot = slot.as_ref()?;
                    Some(InputReadback {
                        number: u16::try_from(number).ok()?,
                        label: slot.label.clone(),
                        source: slot.source.clone(),
                    })
                })
                .collect(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sized_once_and_resolves_inputs() {
        let table = MixerInputs::new([
            (1, Some("Source 1".into()), Some(SourceRef::new("abc"))),
            (3, None, None),
        ])
        .unwrap();
        assert_eq!(table.slots.len(), 4);
        assert!(table.is_declared(3) && !table.is_declared(2));
        assert_eq!(table.resolve(&SourceRef::input(1)).as_str(), "abc");
        assert_eq!(table.resolve(&SourceRef::new("abc")).as_str(), "abc");
        assert_eq!(table.resolve(&SourceRef::input(3)).as_str(), "in:3");
        assert_eq!(table.input_for_source("abc"), Some(1));
        assert!(MixerInputs::new([(65, None, None)]).is_err());
        assert!(MixerInputs::new([(1, None, None), (1, None, None)]).is_err());
        assert_eq!(SourceRef::new("in:x").as_input(), None);
        assert_eq!(SourceRef::new("in:+1").as_input(), None);
    }
}
