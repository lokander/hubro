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

/// Copies `text` to the system clipboard.
///
/// Used by the small copy buttons (query history, DDL viewer). The grid's
/// copy path has its own eval with a `document.execCommand` fallback and a
/// completion signal, which this deliberately doesn't try to cover — it still
/// escapes its text with [`js_string`].
pub fn copy_to_clipboard(text: &str) {
    document::eval(&format!(
        "navigator.clipboard.writeText({});",
        js_string(text)
    ));
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
}
