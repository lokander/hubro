//! The JavaScript the UI evaluates, in one place: string escaping, the
//! clipboard write, and the focus tricks the webview needs.
//!
//! Every eval that carries user or database text through it goes via
//! [`js_string`]. That is a *safety* boundary, not formatting: a table name,
//! a SQL buffer or a rebuilt DDL statement is arbitrary text, and pasting it
//! raw into an eval string lets a quote or newline break out of the literal
//! and run as code. Keeping the escape in one function is what makes that
//! checkable.

use dioxus::prelude::*;

/// Renders `text` as a JavaScript string literal, quotes included, for
/// interpolation into an eval.
///
/// JSON string syntax is a subset of JavaScript's (since ES2019, which made
/// the unescaped line separators U+2028/U+2029 legal in string literals — the
/// one thing `serde_json` passes through), so `serde_json` is the escaper.
/// Serializing a `&str` cannot fail; the fallback is an empty literal rather
/// than a panic, so a hypothetical failure yields a no-op eval instead of
/// taking the window down.
pub fn js_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

/// Puts `text` on the system clipboard through the webview. Every Copy
/// button and the grid's Ctrl+C go through here; nothing else in `src/ui`
/// touches a clipboard API (`only_js_rs_writes_the_clipboard`).
///
/// `navigator.clipboard` is the modern path; the hidden-textarea
/// `execCommand` fallback covers a webview that withholds it (no secure
/// context, or a rejected permission), because a copy that silently does
/// nothing is indistinguishable from a broken app. The fallback restores the
/// previously focused element so the grid keeps its keyboard focus.
pub fn write_clipboard(text: &str) {
    document::eval(&clipboard_js(text));
}

/// The eval behind [`write_clipboard`].
fn clipboard_js(text: &str) -> String {
    let json = js_string(text);
    format!(
        r#"(() => {{
  const text = {json};
  const fallback = () => {{
    const prev = document.activeElement;
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.top = '-1000px';
    document.body.appendChild(ta);
    ta.select();
    try {{ document.execCommand('copy'); }} catch (e) {{ /* nothing else to try */ }}
    document.body.removeChild(ta);
    if (prev && prev.focus) prev.focus();
  }};
  if (navigator.clipboard && navigator.clipboard.writeText) {{
    navigator.clipboard.writeText(text).catch(fallback);
  }} else {{
    fallback();
  }}
}})();"#
    )
}

/// Focuses the element an `onmounted` handler fired for.
///
/// `set_focus` is async, so it has to be spawned; the result is dropped
/// because a focus that doesn't land is never worth failing over.
pub fn focus_on_mount(evt: MountedEvent) {
    spawn(async move {
        let _ = evt.set_focus(true).await;
    });
}

/// Focuses `#id` on the next animation frame, if it is still there.
///
/// Deferred a frame because these run *during* a render that is about to
/// change what holds focus: WebKit's own focus fallback (to the body) can
/// land after an `onmounted` `set_focus`, and a caret placed before the
/// re-render would be positioned against the old value.
pub fn focus_by_id_next_frame(id: &str) {
    document::eval(&next_frame(&format!(
        "const el = document.getElementById({}); if (el) el.focus();",
        js_string(id)
    )));
}

/// The attribute every modal overlay's backdrop carries
/// (`"data-dv-modal": "true"`), so a reclaim can tell that one is open. RSX
/// needs the attribute name as a literal, so this is what the overlays'
/// registry test (`every_modal_overlay_is_marked`) checks them against.
pub const MODAL_ATTR: &str = "data-dv-modal";

/// Where keyboard focus is, as [`reclaim_focus_by_id_next_frame`] reads it
/// from the page one frame on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct FocusFacts {
    /// The target element exists.
    pub present: bool,
    /// Nothing holds focus: it is on the body (or nowhere), which is where it
    /// lands when the element that held it is removed.
    pub on_body: bool,
    /// Focus is already inside the target.
    pub in_target: bool,
    /// A modal overlay ([`MODAL_ATTR`]) is open.
    pub modal_open: bool,
}

/// Whether a reclaim may focus its target: only when focus is not somewhere
/// else on purpose — it is on the body or already inside the target — and no
/// modal overlay is open. An overlay owns focus while it is up: the Import
/// dialog stays open after its import finishes, and that finish refreshes the
/// grid, so without the overlay check the grid behind it would take focus the
/// moment the dialog's pressed button was removed, and the dialog would stop
/// hearing Escape.
pub fn may_reclaim(facts: FocusFacts) -> bool {
    facts.present && !facts.modal_open && (facts.on_body || facts.in_target)
}

/// Focuses `#id` on the next animation frame when [`may_reclaim`] says the
/// focus there is free to take. The page reports its [`FocusFacts`] and the
/// decision is made here, in Rust, where a test can execute it.
pub fn reclaim_focus_by_id_next_frame(id: &str) {
    let id = id.to_string();
    spawn(async move {
        let mut facts = document::eval(&focus_facts_js(&id));
        if let Ok(facts) = facts.recv::<FocusFacts>().await {
            if may_reclaim(facts) {
                document::eval(&format!(
                    "const el = document.getElementById({}); if (el) el.focus();",
                    js_string(&id)
                ));
            }
        }
    });
}

/// The eval that reports [`FocusFacts`] for `#id`, a frame from now.
fn focus_facts_js(id: &str) -> String {
    next_frame(&format!(
        "const el = document.getElementById({}); const a = document.activeElement; \
         dioxus.send({{ present: !!el, on_body: !a || a === document.body, \
         in_target: !!(el && a && el.contains(a)), \
         modal_open: !!document.querySelector('[{MODAL_ATTR}]') }});",
        js_string(id)
    ))
}

/// [`focus_by_id_next_frame`] for a text input, leaving the caret at the end
/// of the current value.
pub fn focus_input_end_next_frame(id: &str) {
    document::eval(&next_frame(&format!(
        "const el = document.getElementById({}); \
         if (el) {{ el.focus(); el.setSelectionRange(el.value.length, el.value.length); }}",
        js_string(id)
    )));
}

/// Wraps a statement list in a `requestAnimationFrame` callback.
fn next_frame(body: &str) -> String {
    format!("requestAnimationFrame(() => {{ {body} }});")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_dom::ui_sources;

    #[test]
    fn js_string_escapes_what_would_break_out_of_a_literal() {
        assert_eq!(js_string("plain"), "\"plain\"");
        // The two that end an eval string early.
        assert_eq!(js_string(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(js_string(r"C:\tmp"), r#""C:\\tmp""#);
        // A newline inside a JS literal is a syntax error, so it must escape
        // rather than pass through — this is why multi-line SQL and DDL are
        // safe to interpolate.
        assert_eq!(
            js_string("SELECT 1;\nSELECT 2;"),
            "\"SELECT 1;\\nSELECT 2;\""
        );
        assert_eq!(js_string(""), "\"\"");
        // Non-ASCII is left as-is (valid in a JS literal), not \u-escaped.
        assert_eq!(js_string("naïve — 表"), "\"naïve — 表\"");
    }

    #[test]
    fn next_frame_defers_the_body() {
        assert_eq!(
            next_frame("el.focus();"),
            "requestAnimationFrame(() => { el.focus(); });"
        );
    }

    #[test]
    fn reclaiming_focus_takes_it_only_from_the_body_or_from_inside() {
        let facts = |on_body, in_target| FocusFacts {
            present: true,
            on_body,
            in_target,
            modal_open: false,
        };
        assert!(may_reclaim(facts(true, false)), "from the body");
        assert!(may_reclaim(facts(false, true)), "from inside the target");
        assert!(
            !may_reclaim(facts(false, false)),
            "not from the filter box, or anything else the user focused"
        );
        let gone = FocusFacts {
            present: false,
            ..facts(true, false)
        };
        assert!(!may_reclaim(gone), "no target to focus");
    }

    #[test]
    fn no_reclaim_while_a_modal_overlay_is_open() {
        for (on_body, in_target) in [(true, false), (false, true), (false, false)] {
            let facts = FocusFacts {
                present: true,
                on_body,
                in_target,
                modal_open: true,
            };
            assert!(!may_reclaim(facts), "reclaimed under an overlay: {facts:?}");
        }
    }

    #[test]
    fn the_page_reports_the_facts_the_decision_reads() {
        assert_eq!(
            focus_facts_js("dv-grid"),
            "requestAnimationFrame(() => { const el = document.getElementById(\"dv-grid\"); \
             const a = document.activeElement; \
             dioxus.send({ present: !!el, on_body: !a || a === document.body, \
             in_target: !!(el && a && el.contains(a)), \
             modal_open: !!document.querySelector('[data-dv-modal]') }); });"
        );
    }

    #[test]
    fn the_clipboard_write_falls_back_when_the_modern_api_is_missing_or_refuses() {
        let js = clipboard_js("a \"quoted\"\nline");
        let lines: Vec<&str> = js.lines().map(str::trim).collect();
        // The text arrives escaped, through `js_string`.
        assert!(
            lines.contains(&r#"const text = "a \"quoted\"\nline";"#),
            "{js}"
        );
        // A rejected write (no permission, no user activation) falls back...
        assert!(
            lines.contains(&"navigator.clipboard.writeText(text).catch(fallback);"),
            "{js}"
        );
        // ...and so does a webview with no `navigator.clipboard` at all.
        let guard = lines
            .iter()
            .position(|line| *line == "if (navigator.clipboard && navigator.clipboard.writeText) {")
            .unwrap_or_else(|| panic!("no feature check: {js}"));
        assert_eq!(
            lines[guard + 2..guard + 4],
            ["} else {", "fallback();"],
            "{js}"
        );
        // The fallback is the one that copies without the API.
        assert!(
            lines.contains(
                &"try { document.execCommand('copy'); } catch (e) { /* nothing else to try */ }"
            ),
            "{js}"
        );
    }

    /// The clipboard APIs a copy could reach for directly, instead of going
    /// through [`write_clipboard`].
    const CLIPBOARD_APIS: [&str; 3] = ["navigator.clipboard", "writeText", "execCommand"];

    /// One clipboard path: no file under `src/ui` but this one names a
    /// clipboard API in code (comments are dropped), and in this one only
    /// [`clipboard_js`] does. There used to be two, and the Copy buttons in
    /// the history, saved-query and DDL panels used the one without the
    /// `execCommand` fallback.
    #[test]
    fn only_js_rs_writes_the_clipboard() {
        for (path, source) in ui_sources() {
            if path.len() == 1 && path[0] == "js.rs" {
                continue;
            }
            for line in code_lines(&source) {
                for api in CLIPBOARD_APIS {
                    assert!(
                        !line.contains(api),
                        "{path:?} writes the clipboard itself; call js::write_clipboard: {line}"
                    );
                }
            }
        }
        let own = include_str!("js.rs");
        let (code, _tests) = own
            .split_once("#[cfg(test)]")
            .expect("js.rs has a test module");
        let lines = code_lines(code);
        let start = lines
            .iter()
            .position(|line| line.starts_with("fn clipboard_js("))
            .expect("clipboard_js is gone");
        let end = start
            + lines[start..]
                .iter()
                .position(|line| line == "}")
                .expect("clipboard_js has an end");
        for (i, line) in lines.iter().enumerate() {
            if (start..=end).contains(&i) {
                continue;
            }
            for api in CLIPBOARD_APIS {
                assert!(
                    !line.contains(api),
                    "js.rs has a second clipboard path outside clipboard_js: {line}"
                );
            }
        }
    }

    /// `source` with comment lines dropped and each line's whitespace
    /// collapsed: one entry per remaining line.
    fn code_lines(source: &str) -> Vec<String> {
        source
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .collect()
    }

    /// Every modal overlay, by the component that renders it. Each backdrop
    /// must carry [`MODAL_ATTR`] — that is how [`may_reclaim`] knows it is
    /// open.
    ///
    /// And every full-screen layer anywhere under `src/ui` (walked at test
    /// time, so a new file is covered without being listed) is either marked
    /// or named below as not a modal. A layer is any code line containing
    /// `fixed inset-0` — a plain `class:` literal, a branch of a conditional
    /// `class: if …`, or a `format!` — and each one must be matched by a
    /// marker line. What the scan cannot see: a full-screen layer spelled
    /// without `fixed inset-0` on one line (`fixed top-0 left-0 h-full
    /// w-full`, an inline style, or the two classes split across lines or
    /// strings), and a layer in this file, whose tests quote the class.
    #[test]
    fn every_modal_overlay_is_marked() {
        let marker_line = format!(r#""{MODAL_ATTR}": "true","#);
        let layer = "fixed inset-0";
        let sources = ui_sources();
        let source_of = |file: &[&str]| -> Vec<String> {
            let (_, source) = sources
                .iter()
                .find(|(path, _)| path.iter().map(String::as_str).eq(file.iter().copied()))
                .unwrap_or_else(|| panic!("{file:?} is gone"));
            code_lines(source)
        };
        let overlays: [(&[&str], &str); 7] = [
            (&["shell.rs"], "fn QuitConfirmDialog("),
            (&["shell.rs"], "fn Cheatsheet("),
            (&["connections.rs"], "fn ConnectFormModal("),
            (&["schema_edit.rs"], "fn SchemaEditDialog("),
            (&["import.rs"], "pub fn ImportDialog("),
            (&["schema.rs"], "fn DdlOverlay("),
            // The cell-value popup, inside the grid's own component.
            (
                &["grid", "mod.rs"],
                "if let Some(view) = expanded.read().clone() {",
            ),
        ];
        for (file, start) in overlays {
            let lines = source_of(file);
            let from = lines
                .iter()
                .position(|line| line.contains(start))
                .unwrap_or_else(|| panic!("{file:?}: `{start}` is gone"));
            let backdrop = lines[from..]
                .iter()
                .position(|line| line.contains(layer))
                .map(|i| from + i)
                .unwrap_or_else(|| panic!("{file:?}: no backdrop after `{start}`"));
            assert!(
                backdrop > 0 && lines[backdrop - 1] == marker_line,
                "{file:?}: the overlay at `{start}` does not carry {MODAL_ATTR}"
            );
        }
        // Not a modal: the copy menu's click-catcher closes a dropdown and
        // never holds focus.
        let not_modal: [(&[&str], &str); 1] =
            [(&["grid", "mod.rs"], r#"class: "fixed inset-0 z-30","#)];
        for (path, source) in &sources {
            if path.len() == 1 && path[0] == "js.rs" {
                continue;
            }
            let lines = code_lines(source);
            let layers = lines.iter().filter(|line| line.contains(layer)).count();
            let marked = lines.iter().filter(|line| **line == marker_line).count();
            let excused = not_modal
                .iter()
                .filter(|(file, excused)| {
                    path.iter().map(String::as_str).eq(file.iter().copied())
                        && lines.iter().any(|line| line == excused)
                })
                .count();
            assert_eq!(
                layers,
                marked + excused,
                "{path:?}: a full-screen layer is neither marked as a modal nor named as not one"
            );
        }
    }
}
