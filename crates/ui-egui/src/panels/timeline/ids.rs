//! EMBED2: per-instance egui ids for the Timeline and the Graph Editor.
//!
//! The panels keep their gesture state in egui's context memory and give their widgets fixed ids
//! (`"tl-timecode"`, `("grow", uid)`…). In EffectCraft there is one Timeline per context, so the ids
//! are global on purpose. A host that draws the panel next to its own UI, or draws several
//! Timelines, gives each one a scope ([`super::TimelineHost::id_scope`]): while that Timeline draws,
//! every id is salted with it, so its state and widgets never clash with the host's or another
//! instance's. EffectCraft's app has no scope, and its ids stay exactly what they were.
//!
//! The scope is per thread and only set while a panel draws ([`IdScope`] restores the previous one
//! when dropped, also on unwind); it is not process state.

use std::cell::Cell;

thread_local! {
    static SCOPE: Cell<Option<egui::Id>> = const { Cell::new(None) };
}

/// The id of `source` in the Timeline being drawn: `egui::Id::new(source)` without a scope.
pub fn tl_id(source: impl egui::AsId) -> egui::Id {
    match SCOPE.with(Cell::get) {
        None => egui::Id::new(source),
        Some(scope) => scope.with(source),
    }
}

/// Salts [`tl_id`] with `scope` until dropped. Hosts that call Timeline functions outside
/// `timeline::show` (a command that reads the panel's layout, say) wrap them in one too.
#[must_use = "the scope ends when this is dropped"]
pub struct IdScope {
    previous: Option<egui::Id>,
}

impl IdScope {
    pub fn enter(scope: Option<egui::Id>) -> Self {
        Self { previous: SCOPE.with(|s| s.replace(scope)) }
    }
}

impl Drop for IdScope {
    fn drop(&mut self) {
        SCOPE.with(|s| s.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unchanged_without_a_scope_and_salted_within_one() {
        assert_eq!(tl_id(("grow", 7u64)), egui::Id::new(("grow", 7u64)));
        let a = egui::Id::new("card-a");
        {
            let _scope = IdScope::enter(Some(a));
            assert_eq!(tl_id("tl-timecode"), a.with("tl-timecode"));
            {
                let _inner = IdScope::enter(Some(egui::Id::new("card-b")));
                assert_ne!(tl_id("tl-timecode"), a.with("tl-timecode"));
            }
            assert_eq!(tl_id("tl-timecode"), a.with("tl-timecode"), "the inner scope restored the outer one");
        }
        assert_eq!(tl_id("tl-timecode"), egui::Id::new("tl-timecode"));
    }
}
