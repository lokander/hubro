//! Shared presentation primitives for the app's notices, empty states, and
//! loading indicators (FRE-18). These replace the ad-hoc red `<p>`s, plain
//! "empty" sentences, and bare "Loading…" text that accumulated across
//! earlier milestones, so every error, empty view, and slow-query wait looks
//! the same everywhere.
//!
//! All classes follow the light-base + `dark:` override pattern the rest of
//! the app uses, so both themes stay legible.

use std::time::Duration;

use dioxus::prelude::*;
use dioxus_icons::lucide::{Info, TriangleAlert, X};

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
    /// The leading icon for this kind (Lucide, FRE-66). Sized to match the
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
    /// A large icon shown above the title (a Lucide component, FRE-66).
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

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
