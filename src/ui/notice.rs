//! Shared presentation primitives for the app's notices, empty states, and
//! loading indicators. These replace the ad-hoc red `<p>`s, plain
//! "empty" sentences, and bare "Loading…" text that accumulated across
//! earlier milestones, so every error, empty view, and slow-query wait looks
//! the same everywhere.
//!
//! All classes follow the light-base + `dark:` override pattern the rest of
//! the app uses, so both themes stay legible.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use dioxus::prelude::*;
use dioxus_icons::lucide::{Info, TriangleAlert, X};

use super::js::take_overlay_focus;
use crate::db::TableKind;

/// The severity of a [`Banner`], picking its icon and color scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerKind {
    /// A failure the user should notice — red.
    Error,
    /// A non-fatal caveat — amber.
    Warning,
    /// A neutral note — sky/slate.
    Info,
}

impl BannerKind {
    /// The leading icon for this kind (Lucide). Sized to match the
    /// banner's `text-sm` line height so the icon and first text line align.
    pub fn icon(self) -> Element {
        match self {
            BannerKind::Error | BannerKind::Warning => rsx! {
                TriangleAlert { size: 16, class: "mt-0.5" }
            },
            BannerKind::Info => rsx! {
                Info { size: 16, class: "mt-0.5" }
            },
        }
    }

    /// The container's theme-aware border/background/text classes.
    pub fn container_classes(self) -> &'static str {
        match self {
            BannerKind::Error => {
                "border-red-300 dark:border-red-800/70 bg-red-50 dark:bg-red-950/40 \
                 text-red-700 dark:text-red-300"
            }
            BannerKind::Warning => {
                "border-amber-300 dark:border-amber-800/70 bg-amber-50 dark:bg-amber-950/40 \
                 text-amber-700 dark:text-amber-300"
            }
            BannerKind::Info => {
                "border-sky-300 dark:border-sky-800/70 bg-sky-50 dark:bg-sky-950/40 \
                 text-sky-700 dark:text-sky-300"
            }
        }
    }
}

/// A consistent inline notice: an icon, a message, and — when `on_dismiss`
/// is given — a dismiss button. Used for connect errors, query errors,
/// schema-load failures, and other one-line surfaces so they all read the
/// same way in both themes.
#[component]
pub fn Banner(
    kind: BannerKind,
    message: String,
    /// When present, renders a `×` that calls this handler.
    on_dismiss: Option<EventHandler<()>>,
) -> Element {
    rsx! {
        div {
            role: "alert",
            class: "flex items-start gap-2 rounded border px-3 py-2 text-sm {kind.container_classes()}",
            span { class: "shrink-0 select-none leading-5", {kind.icon()} }
            span { class: "min-w-0 flex-1 break-words leading-5", "{message}" }
            if let Some(on_dismiss) = on_dismiss {
                button {
                    class: "shrink-0 rounded px-1 py-1 leading-none opacity-60 hover:opacity-100",
                    aria_label: "Dismiss",
                    onclick: move |_| on_dismiss.call(()),
                    X { size: 14 }
                }
            }
        }
    }
}

/// A designed centered empty state: a muted icon glyph, a title line, a
/// muted hint, and an optional action passed as `children` (e.g. a button).
/// Used for the "no saved connections", "empty database", "empty table", and
/// "no filter matches" views so each is distinct but consistent.
#[component]
pub fn EmptyState(
    /// A large icon shown above the title (a Lucide component).
    icon: Element,
    /// The bold one-line summary.
    title: String,
    /// A muted supporting sentence (omitted when empty).
    hint: String,
    /// Optional action area, rendered under the hint.
    children: Element,
) -> Element {
    rsx! {
        div { class: "flex flex-col items-center justify-center gap-2 px-6 py-12 text-center",
            div { class: "select-none leading-none text-slate-300 dark:text-slate-600",
                {icon}
            }
            p { class: "text-sm font-medium text-slate-700 dark:text-slate-300", "{title}" }
            if !hint.is_empty() {
                p { class: "max-w-xs text-xs text-slate-500 dark:text-slate-500", "{hint}" }
            }
            div { class: "mt-1 empty:hidden", {children} }
        }
    }
}

/// A small theme-aware ring spinner (a spinning bordered circle). Pure CSS
/// (`animate-spin`), so no SVG attribute plumbing.
#[component]
pub fn Spinner() -> Element {
    rsx! {
        div {
            class: "h-4 w-4 shrink-0 animate-spin rounded-full border-2 \
                    border-slate-300 border-t-slate-500 \
                    dark:border-slate-700 dark:border-t-slate-400",
            aria_label: "Loading",
        }
    }
}

/// A spinner + label line, for a load that is already known to be in flight.
#[component]
pub fn LoadingLine(label: String) -> Element {
    rsx! {
        div { class: "flex items-center gap-2 px-4 py-3 text-sm text-slate-500 dark:text-slate-400",
            Spinner {}
            span { "{label}" }
        }
    }
}

/// How long a load must run before its spinner appears. Fast queries finish
/// under this and never flash an indicator; only genuinely slow ones show it.
pub(crate) const SPINNER_DELAY: Duration = Duration::from_millis(300);

/// A [`LoadingLine`] that only appears once a load has been running for
/// [`SPINNER_DELAY`]. Mount this while a query/introspection is pending: fast
/// results replace it before the timer fires (no flash), slow ones reveal the
/// spinner. Unmounting (the result arriving) cancels the pending timer.
#[component]
pub fn DelayedLoading(label: String) -> Element {
    let mut show = use_signal(|| false);
    use_future(move || async move {
        tokio::time::sleep(SPINNER_DELAY).await;
        // No borrow is held across the await above.
        show.set(true);
    });
    rsx! {
        if show() {
            LoadingLine { label }
        }
    }
}

/// The badge naming a schema object's kind, wherever one is listed — the
/// sidebar's table list and the schema pane's heading.
///
/// A plain table gets none: it is the default, and badging every row would
/// leave nothing for the badge to distinguish. An engine-specific
/// `kind_label` ("hypertable", "continuous aggregate") is rendered separately
/// *after* this one, since it refines the kind rather than replacing it.
#[component]
pub fn KindBadge(kind: TableKind) -> Element {
    let (class, label) = match kind {
        TableKind::View => (
            "rounded bg-violet-100 dark:bg-violet-900/50 px-1 text-xs text-violet-700 dark:text-violet-300",
            "view",
        ),
        TableKind::MaterializedView => (
            "rounded bg-fuchsia-100 dark:bg-fuchsia-900/50 px-1 text-xs text-fuchsia-700 dark:text-fuchsia-300",
            "matview",
        ),
        TableKind::Table => return rsx! {},
    };
    rsx! {
        span { class, "{label}" }
    }
}

/// Where an [`Overlay`] puts its panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAlign {
    /// Centered in the window; for panels that fit (or scroll themselves).
    Center,
    /// Top-aligned, and the backdrop scrolls, so a form taller than the
    /// window stays reachable instead of clipping at the top. A panel with
    /// `my-auto` still centers while it fits.
    Top,
}

impl OverlayAlign {
    /// The backdrop's classes. Whole literals rather than a shared prefix,
    /// so the Tailwind scanner sees every class.
    fn backdrop_classes(self) -> &'static str {
        match self {
            OverlayAlign::Center => {
                "fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4 outline-none"
            }
            OverlayAlign::Top => {
                "fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4 outline-none"
            }
        }
    }
}

/// The ids [`Overlay`]s focus themselves by, one per mount.
static NEXT_OVERLAY: AtomicUsize = AtomicUsize::new(0);

/// The one modal scaffold: a dimmed full-window backdrop with `children` as
/// its panel. Every modal in the app is one of these —
/// `every_modal_is_an_overlay` fails on a hand-rolled backdrop — so they all
/// dismiss the same way:
///
/// - **Escape always closes**, handled here. The backdrop takes focus when it
///   mounts, so Escape works before anything inside is clicked, and keydowns
///   from the fields bubble up to it. The window key listener deliberately
///   has no Escape action: it ignores keys while a text field has focus, and
///   `dioxus-desktop` 0.7 does not stop the real event when a handler calls
///   `stop_propagation`, so a second, global Escape path would see the same
///   key and close something else. With one path, one Escape closes only the
///   overlay that holds focus — the topmost, since each takes focus on mount.
/// - **A backdrop click closes only when `dismiss_on_backdrop`.** Pass it for
///   overlays that only show something; not for one holding typed input or a
///   decision, where a stray click would throw the work away.
/// - **A click inside never reaches the backdrop.** The panel sits in a
///   `contents` wrapper (no box of its own, so `children` lay out against the
///   backdrop as before) that stops the click there.
///
/// Focus goes back where it was once the overlay closes, when nothing else
/// has taken it (see [`take_overlay_focus`]).
#[component]
pub fn Overlay(
    on_close: EventHandler<()>,
    dismiss_on_backdrop: bool,
    align: OverlayAlign,
    children: Element,
) -> Element {
    let id = use_hook(|| {
        format!(
            "dv-overlay-{}",
            NEXT_OVERLAY.fetch_add(1, Ordering::Relaxed)
        )
    });
    let mount_id = id.clone();
    rsx! {
        div {
            id,
            // A modal over the app: see `js::MODAL_ATTR`.
            "data-dv-modal": "true",
            class: align.backdrop_classes(),
            tabindex: "-1",
            onmounted: move |_| take_overlay_focus(&mount_id),
            onkeydown: move |evt: KeyboardEvent| {
                if evt.code() == Code::Escape {
                    evt.prevent_default();
                    evt.stop_propagation();
                    on_close.call(());
                }
            },
            onclick: move |_| {
                if dismiss_on_backdrop {
                    on_close.call(());
                }
            },
            div {
                class: "contents",
                onclick: move |evt| evt.stop_propagation(),
                {children}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::cell::Cell;
    use std::rc::Rc;

    use dioxus::core::{ElementId, Event, VirtualDom};
    use dioxus::html::{
        set_event_converter, Location, Modifiers, PlatformEventData, SerializedHtmlEventConverter,
        SerializedKeyboardData, SerializedMouseData,
    };

    use super::*;
    use crate::ui::js::MODAL_ATTR;
    use crate::ui::test_dom::{code_lines, listeners, ui_sources};

    #[test]
    fn each_banner_kind_maps_to_a_distinct_theme_aware_style() {
        for kind in [BannerKind::Error, BannerKind::Warning, BannerKind::Info] {
            let classes = kind.container_classes();
            // Every kind carries a light base and a dark override, so no
            // theme is left with a bare/invisible surface.
            assert!(classes.contains("dark:"), "{kind:?} lacks a dark: override");
            assert!(
                classes.contains("bg-") && classes.contains("border-") && classes.contains("text-"),
                "{kind:?} is missing a border/background/text class"
            );
        }
        // The three kinds are visually distinct (different palettes).
        assert!(BannerKind::Error.container_classes().contains("red-"));
        assert!(BannerKind::Warning.container_classes().contains("amber-"));
        assert!(BannerKind::Info.container_classes().contains("sky-"));
    }

    /// An [`Overlay`] around one button, counting its closes in a root
    /// context cell.
    #[component]
    fn Harness(dismiss_on_backdrop: bool) -> Element {
        let closes = use_context::<Rc<Cell<usize>>>();
        rsx! {
            Overlay {
                on_close: move |_| closes.set(closes.get() + 1),
                dismiss_on_backdrop,
                align: OverlayAlign::Center,
                button { "inside" }
            }
        }
    }

    /// Delivers `name` with `data` to `element`, bubbling as the webview
    /// would.
    fn fire(dom: &VirtualDom, name: &str, data: PlatformEventData, element: ElementId) {
        set_event_converter(Box::new(SerializedHtmlEventConverter));
        let data: Rc<dyn Any> = Rc::new(data);
        dom.runtime()
            .handle_event(name, Event::new(data, true), element);
    }

    fn key(key: Key, code: Code) -> PlatformEventData {
        PlatformEventData::new(Box::new(SerializedKeyboardData::new(
            key,
            code,
            Location::Standard,
            false,
            Modifiers::empty(),
            false,
        )))
    }

    fn click() -> PlatformEventData {
        PlatformEventData::new(Box::new(SerializedMouseData::default()))
    }

    /// The dismissal policy every modal shares, run through a real
    /// `VirtualDom`: Escape closes, any other key does not; a backdrop click
    /// closes only when `dismiss_on_backdrop` says so; a click inside the
    /// panel never does, whatever it says.
    #[test]
    fn an_overlay_closes_on_escape_and_on_the_backdrop_only_when_asked() {
        for dismiss_on_backdrop in [false, true] {
            let mut dom = VirtualDom::new_with_props(
                Harness,
                HarnessProps {
                    dismiss_on_backdrop,
                },
            );
            let closes = Rc::new(Cell::new(0));
            dom.provide_root_context(closes.clone());
            let edits = dom.rebuild_to_vec().edits;
            let keydown = listeners(&edits, "keydown");
            let clicks = listeners(&edits, "click");
            assert_eq!(keydown.len(), 1, "{edits:#?}");
            // Mount order: the backdrop, then the panel's wrapper.
            let [backdrop, inside] = clicks[..] else {
                panic!("expected two click listeners: {edits:#?}");
            };
            assert_eq!(keydown[0], backdrop, "Escape is heard on the backdrop");
            // Focus-on-mount is what lets Escape work before anything inside
            // is clicked.
            assert_eq!(
                listeners(&edits, "mounted"),
                [backdrop],
                "the backdrop takes focus when it mounts"
            );

            fire(
                &dom,
                "keydown",
                key(Key::Character("a".into()), Code::KeyA),
                backdrop,
            );
            assert_eq!(closes.get(), 0, "a key other than Escape closed it");
            fire(&dom, "keydown", key(Key::Escape, Code::Escape), backdrop);
            assert_eq!(closes.get(), 1, "Escape did not close it");

            fire(&dom, "click", click(), inside);
            assert_eq!(
                closes.get(),
                1,
                "a click inside the panel reached the backdrop (dismiss_on_backdrop: {dismiss_on_backdrop})"
            );
            fire(&dom, "click", click(), backdrop);
            assert_eq!(
                closes.get(),
                1 + usize::from(dismiss_on_backdrop),
                "a backdrop click with dismiss_on_backdrop: {dismiss_on_backdrop}"
            );
        }
    }

    /// Every modal in the app is an [`Overlay`], and each one's backdrop
    /// policy is the one stated here, with the reason.
    ///
    /// Three checks over every file under `src/ui` (walked at test time, so a
    /// new file is covered without being listed): nothing outside this file
    /// carries [`MODAL_ATTR`] or a full-screen `fixed inset-0` layer, bar the
    /// one named below — so no modal hand-rolls its backdrop and with it its
    /// own Escape and click rules; every `Overlay` call is one of the modals
    /// listed; and each listed modal passes the `dismiss_on_backdrop` listed
    /// for it. A call is found however it is laid out — on its own line, all
    /// on one line, split before its brace, or through a path
    /// (`notice::Overlay {`). What the scan cannot see: a full-screen layer
    /// spelled another way (`fixed top-0 left-0 h-full w-full`, an inline
    /// style, the two classes split across lines or strings), an `Overlay`
    /// imported under another name (`use … Overlay as Modal`), and a modal
    /// built from a string or a macro of its own.
    #[test]
    fn every_modal_is_an_overlay() {
        let marker_line = format!(r#""{MODAL_ATTR}": "true","#);
        let layer = "fixed inset-0";
        let sources = ui_sources();
        // (file, where the modal starts, dismiss_on_backdrop)
        let modals: [(&[&str], &str, bool); 7] = [
            // Holding a half-typed connection.
            (&["connections.rs"], "fn ConnectFormModal(", false),
            // Holding a column mapping and import options.
            (&["import.rs"], "pub fn ImportDialog(", false),
            // Holding a drafted change, maybe a hand-edited statement.
            (&["schema_edit.rs"], "fn SchemaEditDialog(", false),
            // A decision about unsaved edits: Escape cancels, a missed
            // button does nothing.
            (&["shell.rs"], "fn QuitConfirmDialog(", false),
            // The rest only show something.
            (&["shell.rs"], "fn Cheatsheet(", true),
            (&["schema.rs"], "fn DdlOverlay(", true),
            (
                &["grid", "mod.rs"],
                "if let Some(view) = expanded.read().clone() {",
                true,
            ),
        ];
        let lines_of = |file: &[&str]| -> Vec<String> {
            let (_, source) = sources
                .iter()
                .find(|(path, _)| path.iter().map(String::as_str).eq(file.iter().copied()))
                .unwrap_or_else(|| panic!("{file:?} is gone"));
            code_lines(source)
        };
        // Every place `text` calls `Overlay`: the name as a whole word (a
        // path before it is fine, `DdlOverlay` is not) followed by its brace.
        let calls_in = |text: &str| -> Vec<usize> {
            text.match_indices("Overlay")
                .filter(|&(at, word)| {
                    let before = text[..at].chars().next_back();
                    let whole = !before.is_some_and(|c| c.is_alphanumeric() || c == '_');
                    whole && text[at + word.len()..].trim_start().starts_with('{')
                })
                .map(|(at, _)| at)
                .collect()
        };
        let mut claimed = Vec::new();
        for (file, start, dismiss) in modals {
            let text = lines_of(file).join("\n");
            let from = text
                .find(start)
                .unwrap_or_else(|| panic!("{file:?}: `{start}` is gone"));
            let call = calls_in(&text)
                .into_iter()
                .find(|&at| at > from)
                .unwrap_or_else(|| panic!("{file:?}: `{start}` renders no Overlay"));
            claimed.push((file, call));
            // The first policy after the call is its own: props come before
            // the panel, so a nested overlay's would come later.
            let prop = "dismiss_on_backdrop:";
            let after = &text[call..];
            let value = after
                .find(prop)
                .map(|at| after[at + prop.len()..].trim_start())
                .unwrap_or_else(|| panic!("{file:?}: the overlay at `{start}` passes no policy"));
            let expected = dismiss.to_string();
            assert!(
                value.starts_with(&expected)
                    && !value[expected.len()..]
                        .starts_with(|c: char| c.is_alphanumeric() || c == '_'),
                "{file:?}: the overlay at `{start}` should pass `{prop} {dismiss}`, has `{}`",
                value.lines().next().unwrap_or_default()
            );
        }
        claimed.sort();
        claimed.dedup();
        assert_eq!(
            claimed.len(),
            modals.len(),
            "two modals claimed one Overlay"
        );

        // Not a modal: the copy menu's click-catcher closes a dropdown and
        // never holds focus. It excuses exactly one line, so a second layer
        // spelled the same way is still caught.
        let not_modal: (&[&str], &str) = (&["grid", "mod.rs"], r#"class: "fixed inset-0 z-30","#);
        let mut calls = 0;
        let mut excused = 0;
        for (path, source) in &sources {
            if path.len() == 1 && path[0] == "notice.rs" {
                continue;
            }
            let lines = code_lines(source);
            calls += calls_in(&lines.join("\n")).len();
            for line in &lines {
                assert!(
                    *line != marker_line,
                    "{path:?} marks a modal of its own instead of using Overlay"
                );
                if !line.contains(layer) {
                    continue;
                }
                let is_exempt = path
                    .iter()
                    .map(String::as_str)
                    .eq(not_modal.0.iter().copied())
                    && line == not_modal.1;
                assert!(
                    is_exempt,
                    "{path:?} hand-rolls a full-screen layer instead of using Overlay: {line}"
                );
                excused += 1;
            }
        }
        assert_eq!(
            excused, 1,
            "the copy menu's click-catcher should be the one exempt layer"
        );
        assert_eq!(
            calls,
            modals.len(),
            "an Overlay is rendered that the list above does not name, with its backdrop policy"
        );

        // And the one backdrop is marked, so a reclaim sees it open.
        let own = include_str!("notice.rs");
        let (code, _tests) = own
            .split_once("#[cfg(test)]")
            .expect("notice.rs has a test module");
        let lines = code_lines(code);
        let overlay = lines
            .iter()
            .position(|line| line.starts_with("pub fn Overlay("))
            .expect("Overlay is gone");
        let marked: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line == marker_line)
            .map(|(i, _)| i)
            .collect();
        assert!(
            marked.len() == 1 && marked[0] > overlay,
            "Overlay's backdrop should be the one element carrying {MODAL_ATTR}: {marked:?}"
        );
    }
}
