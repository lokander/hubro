//! Shared presentation primitives for the app's notices, empty states, and
//! loading indicators. These replace the ad-hoc red `<p>`s, plain
//! "empty" sentences, and bare "Loading…" text that accumulated across
//! earlier milestones, so every error, empty view, and slow-query wait looks
//! the same everywhere.
//!
//! All classes follow the light-base + `dark:` override pattern the rest of
//! the app uses, so both themes stay legible.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dioxus::prelude::*;
use dioxus_icons::lucide::{Info, TriangleAlert, X};

use super::js::{focus_on_mount, take_overlay_focus};
use super::text_field::TextInput;
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

/// How long a confirmation ignores the press that would complete it, after
/// the press that armed it. Longer than a double-click: every two-step
/// confirmation here puts its confirming button where the pointer already
/// is (the saved-queries panel's "Delete for good" sat exactly where
/// "Delete" had been; Import, Run and the grid's Save relabel themselves in
/// place), so without this a double-click is a confirmed action.
pub(crate) const CONFIRM_ARM_DELAY: Duration = Duration::from_millis(500);

/// Whether a confirming press arriving `since` the confirmation was armed is
/// taken. Anything sooner is the press that armed it, repeated.
pub(crate) fn confirm_accepted(since: Duration) -> bool {
    since >= CONFIRM_ARM_DELAY
}

/// What a first press armed, and when. Every two-step confirmation holds its
/// armed state in one of these, so the press that completes it can ask
/// [`Armed::settled`] whether it is a second, deliberate press or the first
/// one repeated — one rule, [`confirm_accepted`], for all of them.
///
/// Comparing two of these compares the instant too, so callers match on
/// [`Armed::subject`].
#[derive(Debug, Clone, PartialEq)]
pub struct Armed<T> {
    subject: T,
    at: Instant,
}

impl<T> Armed<T> {
    /// Arms `subject` now.
    pub(crate) fn new(subject: T) -> Self {
        Self {
            subject,
            at: Instant::now(),
        }
    }

    /// What was armed: the exact request, statement or change list the
    /// confirmation is about.
    pub(crate) fn subject(&self) -> &T {
        &self.subject
    }

    /// Whether a confirming press now is taken (see [`CONFIRM_ARM_DELAY`]).
    pub(crate) fn settled(&self) -> bool {
        confirm_accepted(self.at.elapsed())
    }
}

/// A `keydown` handler for any button whose press can complete a
/// confirmation: a key held down never presses it again. A held Enter on a
/// focused button repeats its click every few tens of milliseconds once the
/// key starts repeating, which outlasts [`CONFIRM_ARM_DELAY`] — measured
/// against schema editing's Run and the import dialog's Import, which a 1.5 s
/// held Enter took straight through their confirm-writes prompts. Blocking
/// the repeat's default action stops the button from being activated.
pub(crate) fn a_held_key_is_not_a_press(evt: KeyboardEvent) {
    if evt.is_auto_repeating() {
        evt.prevent_default();
    }
}

/// The second step of a two-step destructive action: an optional question,
/// the confirm button, and Keep. The caller owns whether it is armed and
/// renders this in place of the button that armed it, so a screen can keep
/// one row armed at a time.
///
/// One click never confirms. The confirm button ignores clicks for
/// [`CONFIRM_ARM_DELAY`] after this mounts and is never pressed by a held
/// key, and focus goes to **Keep**, so a repeating Enter from the arming
/// button lands on the safe choice. Escape
/// cancels, heard on the wrapper while focus is anywhere inside it. Nothing
/// disarms it on its own (no blur, no timeout): it stays until answered,
/// like every confirmation before this component.
#[component]
pub fn ConfirmInline(
    /// The question beside the buttons; `None` when the confirm label says
    /// it all ("Delete for good").
    prompt: Option<String>,
    confirm_label: String,
    on_confirm: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let shown = use_hook(|| Armed::new(()));
    rsx! {
        div {
            class: "flex flex-wrap items-center gap-2",
            onkeydown: move |evt: KeyboardEvent| {
                if evt.code() == Code::Escape {
                    evt.prevent_default();
                    evt.stop_propagation();
                    on_cancel.call(());
                }
            },
            if let Some(prompt) = prompt {
                span { class: "text-xs text-amber-700 dark:text-amber-300", "{prompt}" }
            }
            button {
                class: "cursor-pointer rounded bg-amber-600 px-2 py-0.5 text-xs font-semibold text-slate-950 hover:bg-amber-500",
                onkeydown: a_held_key_is_not_a_press,
                onclick: move |_| {
                    if shown.settled() {
                        on_confirm.call(());
                    }
                },
                "{confirm_label}"
            }
            button {
                class: "cursor-pointer rounded border border-slate-400 dark:border-slate-600 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                onmounted: focus_on_mount,
                onclick: move |_| on_cancel.call(()),
                "Keep"
            }
        }
    }
}

/// A side panel's search field. `search` is the committed filter: it follows
/// the field only on Enter, or at once when the field is cleared, so a list
/// backed by a database query is not re-queried on every keystroke.
#[component]
pub fn SearchBox(placeholder: String, search: Signal<String>) -> Element {
    let mut search = search;
    let mut draft = use_signal(String::new);
    rsx! {
        TextInput {
            class: "w-full rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-1 text-xs text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600",
            placeholder,
            text: "{draft}",
            oninput: move |evt: FormEvent| {
                let value = evt.value();
                // Clearing the box resets the filter immediately.
                if value.trim().is_empty() {
                    search.set(String::new());
                }
                draft.set(value);
            },
            onkeydown: move |evt: KeyboardEvent| {
                if evt.key() == Key::Enter {
                    search.set(draft.peek().clone());
                }
            },
        }
    }
}

/// The SQL editor's right-hand panel frame (history, saved queries): a fixed
/// width column with `header` on top, `children` scrolling in the middle and
/// an optional `footer` pinned at the bottom.
#[component]
pub fn SidePanel(header: Element, footer: Option<Element>, children: Element) -> Element {
    rsx! {
        aside { class: "flex w-80 shrink-0 flex-col border-l border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/50",
            div { class: "flex flex-col gap-2 border-b border-slate-200 dark:border-slate-800 p-2",
                {header}
            }
            div { class: "min-h-0 flex-1 overflow-y-auto", {children} }
            if let Some(footer) = footer {
                div { class: "flex flex-col gap-2 border-t border-slate-200 dark:border-slate-800 p-2",
                    {footer}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::cell::Cell;
    use std::rc::Rc;

    use dioxus::core::{ElementId, Event, Mutation, VirtualDom};
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

    /// An arming button that a [`ConfirmInline`] replaces, counting
    /// confirmations in a root context cell — the shape of every caller.
    #[component]
    fn ConfirmHarness() -> Element {
        let confirms = use_context::<Rc<Cell<usize>>>();
        let mut armed = use_signal(|| false);
        rsx! {
            if armed() {
                ConfirmInline {
                    prompt: "Sure?".to_string(),
                    confirm_label: "Do it",
                    on_confirm: move |_| confirms.set(confirms.get() + 1),
                    on_cancel: move |_| armed.set(false),
                }
            } else {
                button { onclick: move |_| armed.set(true), "arm" }
            }
        }
    }

    /// One click never confirms, run through a real `VirtualDom`: the click
    /// that arms, repeated onto the confirm button that replaced the arming
    /// one (a double-click), does nothing; focus goes to Keep, so a repeating
    /// Enter answers "keep"; Escape cancels; and a deliberate click once the
    /// delay has passed does confirm.
    #[test]
    fn one_click_never_confirms_an_inline_confirmation() {
        let mut dom = VirtualDom::new(ConfirmHarness);
        let confirms = Rc::new(Cell::new(0_usize));
        dom.provide_root_context(confirms.clone());
        let edits = dom.rebuild_to_vec().edits;
        let arm = |dom: &mut VirtualDom, edits: &[Mutation]| {
            let [arm] = listeners(edits, "click")[..] else {
                panic!("expected the arming button alone: {edits:#?}");
            };
            fire(dom, "click", click(), arm);
            let edits = dom.render_immediate_to_vec().edits;
            let [confirm, keep] = listeners(&edits, "click")[..] else {
                panic!("expected confirm and Keep: {edits:#?}");
            };
            // Mount order: the wrapper's Escape, then the confirm button's
            // held-key guard.
            let [wrapper, held] = listeners(&edits, "keydown")[..] else {
                panic!("expected two keydown listeners: {edits:#?}");
            };
            assert_eq!(held, confirm, "the confirm button ignores a held key");
            assert_eq!(
                listeners(&edits, "mounted"),
                [keep],
                "focus should go to Keep, not to the confirm button"
            );
            (confirm, wrapper)
        };

        let (confirm, wrapper) = arm(&mut dom, &edits);
        fire(&dom, "click", click(), confirm);
        fire(&dom, "click", click(), confirm);
        assert_eq!(confirms.get(), 0, "the arming click confirmed");

        fire(&dom, "keydown", key(Key::Escape, Code::Escape), wrapper);
        let edits = dom.render_immediate_to_vec().edits;
        assert_eq!(confirms.get(), 0, "Escape confirmed");

        // Escape disarmed it: the arming button is back, and arms afresh.
        let (confirm, _) = arm(&mut dom, &edits);
        std::thread::sleep(CONFIRM_ARM_DELAY + Duration::from_millis(50));
        fire(&dom, "click", click(), confirm);
        assert_eq!(confirms.get(), 1, "a deliberate click did not confirm");
    }

    /// The held-key guard every confirming button carries: a repeat of a
    /// held key loses its default action (the button's activation), a fresh
    /// press keeps it.
    #[test]
    fn a_held_key_never_presses_a_confirming_button() {
        for repeating in [false, true] {
            let data = KeyboardData::new(SerializedKeyboardData::new(
                Key::Enter,
                Code::Enter,
                Location::Standard,
                repeating,
                Modifiers::empty(),
                false,
            ));
            let evt = Event::new(Rc::new(data), true);
            a_held_key_is_not_a_press(evt.clone());
            assert_eq!(
                evt.default_action_enabled(),
                !repeating,
                "repeating: {repeating}"
            );
        }
    }

    /// [`Armed`] is unsettled when armed and settles after the delay — the
    /// gate the relabelled-in-place buttons (Import, Run, the grid's Save)
    /// put in front of their confirming press.
    #[test]
    fn an_armed_confirmation_settles_only_after_the_delay() {
        let armed = Armed::new("x");
        assert!(!armed.settled(), "settled the instant it was armed");
        assert_eq!(armed.subject(), &"x");
        std::thread::sleep(CONFIRM_ARM_DELAY + Duration::from_millis(50));
        assert!(armed.settled(), "never settled");
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
