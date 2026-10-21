//! Components mounted in a real `VirtualDom`, for tests that count renders.
//!
//! Driving the app shows *what* a screen looks like, never *why*: a row that
//! re-rendered and a row that did not produce the same pixels. Counting the
//! renders themselves is what lets a test say a write reached only the
//! components it should — see "Driving can confirm a bug" in
//! `docs/interactive-testing.md`.

use std::path::Path;

use dioxus::core::{generation, ScopeId, VirtualDom};

use super::state::AppState;
use crate::config::{SavedList, Settings};

/// How many scope ids [`total_renders`] looks at. Scope ids index a slab, so
/// a tree of a few hundred components stays far below this.
const SCOPE_SCAN: usize = 1 << 14;

/// Gives `dom` an [`AppState`] in its root context and returns it.
///
/// The state is [`AppState::unstarted`]: no background task, no history
/// store, no read of the user's config. The saved list is empty — loaded from
/// a path that does not exist.
pub(crate) fn provide_state(dom: &VirtualDom) -> AppState {
    let state = dom.in_runtime(|| {
        let (saved, _) = SavedList::load(Path::new("/nonexistent/hubro-test/connections.toml"));
        AppState::unstarted(saved, None, Settings::default())
    });
    dom.provide_root_context(state);
    state
}

/// How many times the components alive in `dom` have rendered, summed over
/// all of them. Two readings bracket a change: their difference is the number
/// of renders it caused, as long as nothing mounted or unmounted in between.
pub(crate) fn total_renders(dom: &VirtualDom) -> usize {
    assert!(
        dom.get_scope(ScopeId(SCOPE_SCAN)).is_none(),
        "the tree outgrew the scan, so a count would silently miss components"
    );
    (0..SCOPE_SCAN)
        .map(ScopeId)
        .filter(|&scope| dom.get_scope(scope).is_some())
        .map(|scope| dom.in_scope(scope, generation))
        .sum()
}
