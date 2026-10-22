//! Components mounted in a real `VirtualDom`, for tests that count renders.
//!
//! Driving the app shows *what* a screen looks like, never *why*: a row that
//! re-rendered and a row that did not produce the same pixels. Counting the
//! renders themselves is what lets a test say a write reached only the
//! components it should — see "Driving can confirm a bug" in
//! `docs/interactive-testing.md`.

use std::any::Any;
use std::rc::Rc;

use dioxus::core::{generation, ElementId, Event, Mutation, ScopeId, VirtualDom};
use dioxus::html::{
    set_event_converter, PlatformEventData, SerializedFormData, SerializedHtmlEventConverter,
};

use super::state::AppState;
use crate::config::{AppPaths, SavedList, Settings};

/// How many scope ids [`total_renders`] looks at. Scope ids index a slab, so
/// a tree of a few hundred components stays far below this.
const SCOPE_SCAN: usize = 1 << 14;

/// Gives `dom` an [`AppState`] in its root context and returns it.
///
/// The state is [`AppState::unstarted`]: no background task, no history
/// store, and an empty saved list. Its files are [`AppPaths::under`] a temp
/// dir of its own, which lives as long as `dom` (it sits in the root
/// context), so whatever the test saves or connects lands there — never in
/// the user's real config, which a connect test once wrote to on every run.
pub(crate) fn provide_state(dom: &VirtualDom) -> AppState {
    let dir = Rc::new(tempfile::tempdir().expect("a temp dir for the test's files"));
    let paths = AppPaths::under(dir.path());
    let connections = paths
        .connections
        .clone()
        .expect("under() resolves every path");
    let state = dom.in_runtime(|| {
        AppState::unstarted(
            SavedList::load(&connections).0,
            None,
            Settings::default(),
            Default::default(),
            paths,
        )
    });
    dom.provide_root_context(dir);
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

/// The text `edits` put on screen: every text node they created or changed.
/// Static text inside an `rsx!` template is not among them — only text that
/// interpolates a value is.
pub(crate) fn texts(edits: &[Mutation]) -> Vec<&str> {
    edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::CreateTextNode { value, .. } | Mutation::SetText { value, .. } => {
                Some(value.as_str())
            }
            _ => None,
        })
        .collect()
}

/// The elements `edits` attached a listener for `event` to (`"input"`, not
/// `"oninput"`), in mount order.
pub(crate) fn listeners(edits: &[Mutation], event: &str) -> Vec<ElementId> {
    edits
        .iter()
        .filter_map(|edit| match edit {
            Mutation::NewEventListener { name, id } if name == event => Some(*id),
            _ => None,
        })
        .collect()
}

/// Delivers an `input` event to `element` as though the user had typed until
/// the field read `value`. Run [`VirtualDom::render_immediate`] after it to
/// see what the handler changed.
///
/// Events reach a handler through a converter the renderer installs, and a
/// bare `VirtualDom` has none; this installs the serialized one, which is
/// process-wide but stateless, so every test that calls this installs the
/// same thing.
pub(crate) fn type_into(dom: &VirtualDom, element: ElementId, value: &str) {
    set_event_converter(Box::new(SerializedHtmlEventConverter));
    let form = SerializedFormData::new(value.to_string(), Vec::new());
    let data: Rc<dyn Any> = Rc::new(PlatformEventData::new(Box::new(form)));
    dom.runtime()
        .handle_event("input", Event::new(data, true), element);
}

/// Every Rust file under `src/ui`, walked at test time, as
/// (path components below `src/ui`, source).
pub(crate) fn ui_sources() -> Vec<(Vec<String>, String)> {
    fn walk(dir: &std::path::Path, below: &[String], out: &mut Vec<(Vec<String>, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("{}: {err}", dir.display()))
            .map(|entry| entry.expect("a readable directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("a UTF-8 file name")
                .to_string();
            let mut here = below.to_vec();
            here.push(name);
            if path.is_dir() {
                walk(&path, &here, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
                out.push((here, source));
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("ui");
    let mut out = Vec::new();
    walk(&root, &[], &mut out);
    assert!(out.len() > 10, "src/ui was not found: {}", root.display());
    out
}
