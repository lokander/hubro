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
use crate::ui::classes::INPUT;

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
///
/// A heuristic, not a structural guard. A slow double-click — Windows lets
/// the setting go to 900 ms — or a tremor can put a second click past it,
/// and nothing then tells it from a deliberate one; the relabelled buttons
/// cannot move out from under the pointer, so time is all there is to go
/// on. A press held back inside the window is dropped without feedback.
pub(crate) const CONFIRM_ARM_DELAY: Duration = Duration::from_millis(500);

/// How a press reached a confirmation's decision. A held key's repeats are
/// never deliberate presses, whatever the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
///
/// Its field is private: a gesture is either [`Gesture::CLICK`], which the
/// wiring test holds to guarded buttons, or [`Gesture::key`] for a key handled
/// directly, which it holds to the one shortcut that has one. No struct
/// literal can claim a press is fresh anywhere else.
pub struct Gesture {
    /// The key is being held down: this is one of its auto-repeats.
    repeat: bool,
}

impl Gesture {
    /// A click on a button: the pointer, or a key activating the focused
    /// button. Never a repeat, *because* every button that passes this
    /// carries [`a_held_key_is_not_a_press`], which stops a held key's
    /// repeats before they become clicks — a click event itself cannot say
    /// whether a key produced it. `every_click_gesture_comes_from_a_guarded_button`
    /// holds each one to that.
    pub const CLICK: Gesture = Gesture { repeat: false };

    /// A key handled directly, without going through a button: `repeat` is
    /// whether it is a held key's auto-repeat. Only the pane shortcut, whose
    /// key arrives from the window listener as a message, builds one.
    pub fn key(repeat: bool) -> Self {
        Gesture { repeat }
    }

    /// Whether this is a held key's auto-repeat.
    pub fn is_repeat(self) -> bool {
        self.repeat
    }
}

/// The one rule every confirmation's decision applies: a press completes a
/// confirmation armed `since` ago only when it is a fresh, deliberate press —
/// not a held key's repeat, and not inside [`CONFIRM_ARM_DELAY`] (the press
/// that armed it, repeated). Callers reach it through [`Armed::completes`] or,
/// for the navigation guard, which keeps its own park time, directly.
pub(crate) fn deliberate(since: Duration, gesture: Gesture) -> bool {
    !gesture.repeat && since >= CONFIRM_ARM_DELAY
}

/// What a first press armed, and when. Every two-step confirmation's
/// decision takes its armed state as one of these, plus the [`Gesture`] of
/// the press in front of it, and asks [`Armed::completes`] — so no handler
/// holds a "may confirm" boolean it could get wrong.
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

    /// Armed `ago` already: what a test uses to stand on either side of
    /// [`CONFIRM_ARM_DELAY`] without sleeping.
    #[cfg(test)]
    pub(crate) fn armed_ago(subject: T, ago: Duration) -> Self {
        Self {
            subject,
            at: Instant::now() - ago,
        }
    }

    /// What was armed: the exact request, statement or change list the
    /// confirmation is about.
    pub(crate) fn subject(&self) -> &T {
        &self.subject
    }

    /// Whether `gesture`, now, completes this confirmation ([`deliberate`]).
    pub(crate) fn completes(&self, gesture: Gesture) -> bool {
        deliberate(self.at.elapsed(), gesture)
    }
}

/// A `keydown` handler for every button whose click passes
/// [`Gesture::CLICK`], and for the safe choice focus moves to: a held key
/// never presses it again. A held Enter on a focused button repeats its click
/// every few tens of milliseconds once the key starts repeating, which
/// outlasts [`CONFIRM_ARM_DELAY`] — measured against schema editing's Run and
/// the import dialog's Import, which a 1.5 s held Enter took straight through
/// their confirm-writes prompts. Blocking the repeat's default action stops
/// the button from being activated; only the activation keys are touched.
pub(crate) fn a_held_key_is_not_a_press(evt: KeyboardEvent) {
    let activates = matches!(evt.key(), Key::Enter) || evt.key() == Key::Character(" ".into());
    if activates && evt.is_auto_repeating() {
        evt.prevent_default();
    }
}

/// The second step of a two-step destructive action: an optional question,
/// the confirm button, and Keep. The caller owns whether it is armed and
/// renders this in place of the button that armed it, so a screen can keep
/// one row armed at a time.
///
/// One click never confirms. The confirm button ignores clicks for
/// [`CONFIRM_ARM_DELAY`] after this mounts, neither button is pressed by a
/// held key, and focus goes to **Keep**, the safe choice. Escape
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
                    if shown.completes(Gesture::CLICK) {
                        on_confirm.call(());
                    }
                },
                "{confirm_label}"
            }
            button {
                class: "cursor-pointer rounded border border-slate-400 dark:border-slate-600 px-2 py-0.5 text-xs text-slate-900 dark:text-slate-300 hover:bg-slate-200 dark:hover:bg-slate-800",
                onmounted: focus_on_mount,
                // So a held Enter from the arming button arms and stays
                // armed, rather than arming and then pressing Keep.
                onkeydown: a_held_key_is_not_a_press,
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
            class: "w-full {INPUT}",
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
            // Mount order: the wrapper's Escape, then each button's held-key
            // guard.
            let [wrapper, held_confirm, held_keep] = listeners(&edits, "keydown")[..] else {
                panic!("expected three keydown listeners: {edits:#?}");
            };
            assert_eq!(
                [held_confirm, held_keep],
                [confirm, keep],
                "both buttons ignore a held key"
            );
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
        let blocked = |key: Key, code: Code, repeating: bool| {
            let data = KeyboardData::new(SerializedKeyboardData::new(
                key,
                code,
                Location::Standard,
                repeating,
                Modifiers::empty(),
                false,
            ));
            let evt = Event::new(Rc::new(data), true);
            a_held_key_is_not_a_press(evt.clone());
            !evt.default_action_enabled()
        };
        assert!(blocked(Key::Enter, Code::Enter, true), "a held Enter");
        assert!(
            blocked(Key::Character(" ".into()), Code::Space, true),
            "a held Space"
        );
        assert!(!blocked(Key::Enter, Code::Enter, false), "a fresh Enter");
        // Only activation is touched: a held arrow still walks the sidebar.
        assert!(
            !blocked(Key::ArrowDown, Code::ArrowDown, true),
            "a held arrow"
        );
    }

    /// The rule every confirmation decides with: a press completes an armed
    /// confirmation only when it is fresh (not a held key's repeat) and
    /// deliberate (past the delay).
    #[test]
    fn only_a_fresh_deliberate_press_completes_a_confirmation() {
        let fresh = Gesture::CLICK;
        let repeat = Gesture::key(true);
        let just = Armed::new("x");
        assert!(!just.completes(fresh), "inside the delay");
        assert_eq!(just.subject(), &"x");
        let aged = Armed::armed_ago("x", CONFIRM_ARM_DELAY);
        assert!(aged.completes(fresh), "a settled, fresh press");
        assert!(!aged.completes(repeat), "a held key's repeat, however late");
    }

    /// The one place a confirmation's decision cannot see a held key is a
    /// button click: the click event does not say whether a key produced it.
    /// So every click that tells a decision it is fresh ([`Gesture::CLICK`])
    /// must come from an element where a held key cannot produce one — a
    /// `button` carrying [`a_held_key_is_not_a_press`], or an `a` with no
    /// `href` or `tabindex`, which no key activates. The safe choices focus
    /// moves to (Keep, the write prompt's Cancel) carry the guard too, so a
    /// held Enter that arms a confirmation leaves it armed.
    ///
    /// Read from the source of every file under `src/ui`, comment lines and
    /// test modules left out. From each `Gesture::CLICK`, it walks outward
    /// by indentation to the enclosing element and reads that element's own
    /// attributes. What it cannot see: a `Gesture::CLICK` stored in a binding
    /// and passed on elsewhere, or handed through a closure prop to a child
    /// component (the ↗ links show the shape that *is* covered: the child's
    /// own element passes it); the guard wrapped in a closure of its own; and
    /// an element that a key activates by some other means.
    #[test]
    fn every_click_gesture_comes_from_a_guarded_button() {
        let guard = "onkeydown: a_held_key_is_not_a_press,";
        let indent = |line: &str| line.len() - line.trim_start().len();
        // The element whose body holds `lines[at]`, as the index of its
        // opening line.
        let enclosing = |lines: &[&str], at: usize| -> Option<usize> {
            let mut level = indent(lines[at]);
            for i in (0..at).rev() {
                let line = lines[i];
                if line.trim().is_empty() || indent(line) >= level {
                    continue;
                }
                level = indent(line);
                let opener = line.trim();
                if opener == "button {" || opener == "a {" {
                    return Some(i);
                }
                if opener.starts_with("fn ")
                    || opener.starts_with("pub ")
                    || opener.contains("rsx!")
                {
                    return None;
                }
            }
            None
        };
        // The element's own attribute lines: one level in, up to its close.
        let attributes = |lines: &[&str], open: usize| -> Vec<String> {
            let own = indent(lines[open]) + 4;
            lines[open + 1..]
                .iter()
                .take_while(|line| line.trim().is_empty() || indent(line) > indent(lines[open]))
                .filter(|line| indent(line) == own)
                .map(|line| line.trim().to_string())
                .collect()
        };
        let check = |path: &[String], lines: &[&str], at: usize, what: &str| -> &'static str {
            let Some(open) = enclosing(lines, at) else {
                panic!(
                    "{path:?}: {what} is not inside a button: {}",
                    lines[at].trim()
                );
            };
            let attrs = attributes(lines, open);
            if lines[open].trim() == "button {" {
                assert!(
                    attrs.iter().any(|a| a == guard),
                    "{path:?}: the button around {what} lacks `{guard}`: {}",
                    lines[at].trim()
                );
                "button"
            } else {
                assert!(
                    !attrs
                        .iter()
                        .any(|a| a.starts_with("href") || a.starts_with("tabindex")),
                    "{path:?}: a key can activate the link around {what}: {}",
                    lines[at].trim()
                );
                "a"
            }
        };

        let mut found: Vec<String> = Vec::new();
        let mut safe = Vec::new();
        let mut keyed = 0;
        for (path, source) in ui_sources() {
            // The code outside the test modules, by lines so a CRLF checkout
            // reads the same (see tests/line_endings.rs).
            let shipped = crate::source_scan::shipped(&source);
            let lines: Vec<&str> = shipped
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect();
            let name = path.join("/");
            for at in 0..lines.len() {
                let line = lines[at];
                // A gesture's field is private, so outside notice.rs the only
                // ways to make one are `Gesture::CLICK`, walked below, and
                // `Gesture::key`, which only the pane shortcut may call.
                let squeezed: String = line.split_whitespace().collect();
                if squeezed.contains("Gesture::key(") {
                    assert_eq!(
                        name,
                        "shell.rs",
                        "only the pane shortcut builds a key gesture: {}",
                        line.trim()
                    );
                    keyed += 1;
                }
                if line.contains("Gesture::CLICK") {
                    check(&path, &lines, at, "a click gesture");
                    found.push(name.clone());
                }
                let is_safe_choice = (name == "notice.rs"
                    && line.trim() == "onclick: move |_| on_cancel.call(()),")
                    || (name == "grid/mod.rs" && line.contains("state.dismiss_pending_save("));
                if is_safe_choice {
                    assert_eq!(check(&path, &lines, at, "a safe choice"), "button");
                    safe.push(name.clone());
                }
            }
        }
        // Not vacuous: every site this guards is among what was walked.
        for file in [
            "notice.rs",
            "import.rs",
            "schema_edit.rs",
            "grid/mod.rs",
            "grid/rows.rs",
            "grid/detail.rs",
            "shell.rs",
            "sidebar.rs",
        ] {
            assert!(
                found.iter().any(|f| f == file),
                "no click gesture found in {file}"
            );
        }
        assert_eq!(safe, ["grid/mod.rs", "notice.rs"], "the safe choices moved");
        assert_eq!(keyed, 1, "the pane shortcut's key gesture moved");
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
        let lines = code_lines(&crate::source_scan::shipped(own));
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
