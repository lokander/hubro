//! Text fields that do not lose keystrokes: [`TextInput`] and [`TextArea`].
//!
//! **Why not `input { value: "{signal}", oninput: … }`.** On the desktop
//! renderer an input event reaches Rust synchronously, but the render it
//! causes reaches the webview later, over a separate channel. A `value`
//! attribute bound to the signal the handler writes therefore echoes every
//! keystroke back, and the echo can land after the user has typed the next
//! character: the field is set back to the older text and that character is
//! gone. Dioxus also marks `value` *volatile*, so it is written on every
//! render of the component holding the input, changed or not — a re-render
//! for any other reason writes the same stale snapshot. Driven with
//! `xdotool key p r o d` (keys about 12 ms apart), the connections search
//! read `prd` four times in four, the new-group field turned `production`
//! into `poucin`, and a connect form's host field into `prduton`. Typing
//! slowly hid it, because each echo then landed before the next key.
//!
//! So these fields carry no `value` attribute at all, and drop one a caller
//! passes. The text the field mounts with is rendered as `initial_value` (the
//! element's default value) and never re-rendered, so later text — a
//! password's included — never reaches an attribute. The field keeps a note
//! of the text its element holds: what it last reported through an input
//! event, or what was last pushed into it. A render whose text differs from
//! that note is a change the user did not type (Escape clearing a search, a
//! pasted URL filling the host field, a foreign-key jump installing a
//! filter), and only then is the text written into the element, by
//! [`push_text_js`] — and only if the element still holds the noted text, so
//! a key typed after the change and before the push lands is not wiped out.
//! Typing never writes the field.

use std::sync::atomic::{AtomicU64, Ordering};

use dioxus::prelude::*;

use super::js::js_string;

/// Whether a render has to write `text` into the element, given the text the
/// element is known to hold. Equal means the element already shows it —
/// usually because the user just typed it — and writing it back is exactly
/// the stale echo this module exists to avoid.
pub(super) fn needs_push(text: &str, held: &str) -> bool {
    text != held
}

/// The attribute that names a field's element for [`push_text_js`]. Callers'
/// own `id`s stay theirs (several are focus-shortcut targets), so the field
/// marks its element with a number of its own instead.
const MARKER_ATTR: &str = "data-dv-text";

/// The eval that writes `text` into the field marked `marker`, if the field
/// still holds `held` — the text it was believed to hold when the push was
/// decided. The push is decided in Rust and lands in the webview later; a
/// key typed in between makes the element differ from `held`, and the input
/// event already on its way brings the caller's state up to what the field
/// shows. Overwriting it instead would leave the field empty under a list
/// filtered by what was typed.
pub(super) fn push_text_js(marker: u64, held: &str, text: &str) -> String {
    format!(
        "const el = document.querySelector('[{MARKER_ATTR}=\"{marker}\"]'); \
         if (el && el.value === {held}) el.value = {text};",
        held = js_string(held),
        text = js_string(text)
    )
}

/// The caller's attributes, less the two this module owns. A `value` would
/// bring back the stale echo the module exists to remove (`extends = input`
/// offers it, and it is the attribute a hand reaches for), and an
/// `initial_value` would re-render the text into an attribute.
fn passed_through(attributes: Vec<Attribute>) -> Vec<Attribute> {
    attributes
        .into_iter()
        .filter(|attribute| !matches!(attribute.name, "value" | "initial_value"))
        .collect()
}

/// The bookkeeping both field kinds share: the element's marker, the text
/// the field mounted with, and the text the element is known to hold.
#[derive(Clone, Copy)]
struct HeldText {
    marker: u64,
    initial: CopyValue<String>,
    held: CopyValue<String>,
}

impl HeldText {
    /// Records what an input event says the element now holds. Called before
    /// the caller's handler, so the render that handler causes finds the
    /// text already held and writes nothing.
    fn typed(mut self, evt: &FormEvent) {
        self.held.set(evt.value());
    }
}

/// Mounts the bookkeeping, and writes `text` into the element when it is not
/// what the element holds.
fn use_held_text(text: &str) -> HeldText {
    static NEXT_MARKER: AtomicU64 = AtomicU64::new(0);
    let marker = use_hook(|| NEXT_MARKER.fetch_add(1, Ordering::Relaxed));
    let initial = use_hook(|| CopyValue::new(text.to_string()));
    let mut held = use_hook(|| CopyValue::new(text.to_string()));
    // Decided during the render, not in an effect: the props compare by
    // value, so a changed `text` always re-renders this component, and the
    // comparison makes a render without a change a no-op.
    let before = held.peek().clone();
    if needs_push(text, &before) {
        held.set(text.to_string());
        document::eval(&push_text_js(marker, &before, text));
    }
    HeldText {
        marker,
        initial,
        held,
    }
}

/// A single-line text field. `text` is the field's content as the caller
/// holds it; `oninput` should store the new text where `text` is read from.
/// Every other attribute (`id`, `class`, `placeholder`, `r#type`, …) passes
/// straight through to the `input`. An attribute that both `input` and the
/// global set define (`autofocus`, `spellcheck`) is ambiguous to the props
/// builder, so it is written quoted: `"autofocus": "true"`.
#[derive(Props, Clone)]
pub struct TextInputProps {
    text: String,
    oninput: EventHandler<FormEvent>,
    onkeydown: Option<EventHandler<KeyboardEvent>>,
    onmounted: Option<EventHandler<MountedEvent>>,
    onblur: Option<EventHandler<FocusEvent>>,
    #[props(extends = GlobalAttributes, extends = input)]
    attributes: Vec<Attribute>,
}

/// Compares what is rendered and leaves the handlers out. The caller's
/// closures are new on every one of its renders, so comparing them would
/// re-render the field each time its parent renders; the props machinery
/// repoints the handlers in place either way, so they are never stale.
impl PartialEq for TextInputProps {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.attributes == other.attributes
    }
}

#[component]
pub fn TextInput(props: TextInputProps) -> Element {
    let TextInputProps {
        text,
        oninput,
        onkeydown,
        onmounted,
        onblur,
        attributes,
    } = props;
    let held = use_held_text(&text);
    let attributes = passed_through(attributes);
    let initial = held.initial.cloned();
    rsx! {
        input {
            "data-dv-text": "{held.marker}",
            initial_value: "{initial}",
            oninput: move |evt| {
                held.typed(&evt);
                oninput.call(evt);
            },
            onkeydown: move |evt| {
                if let Some(handler) = onkeydown {
                    handler.call(evt);
                }
            },
            onmounted: move |evt| {
                if let Some(handler) = onmounted {
                    handler.call(evt);
                }
            },
            onblur: move |evt| {
                if let Some(handler) = onblur {
                    handler.call(evt);
                }
            },
            ..attributes,
        }
    }
}

/// [`TextInput`] for a `textarea`.
#[derive(Props, Clone)]
pub struct TextAreaProps {
    text: String,
    oninput: EventHandler<FormEvent>,
    onkeydown: Option<EventHandler<KeyboardEvent>>,
    onmounted: Option<EventHandler<MountedEvent>>,
    onblur: Option<EventHandler<FocusEvent>>,
    #[props(extends = GlobalAttributes, extends = textarea)]
    attributes: Vec<Attribute>,
}

/// As [`TextInputProps`]'s.
impl PartialEq for TextAreaProps {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.attributes == other.attributes
    }
}

#[component]
pub fn TextArea(props: TextAreaProps) -> Element {
    let TextAreaProps {
        text,
        oninput,
        onkeydown,
        onmounted,
        onblur,
        attributes,
    } = props;
    let held = use_held_text(&text);
    let attributes = passed_through(attributes);
    let initial = held.initial.cloned();
    rsx! {
        textarea {
            "data-dv-text": "{held.marker}",
            initial_value: "{initial}",
            oninput: move |evt| {
                held.typed(&evt);
                oninput.call(evt);
            },
            onkeydown: move |evt| {
                if let Some(handler) = onkeydown {
                    handler.call(evt);
                }
            },
            onmounted: move |evt| {
                if let Some(handler) = onmounted {
                    handler.call(evt);
                }
            },
            onblur: move |evt| {
                if let Some(handler) = onblur {
                    handler.call(evt);
                }
            },
            ..attributes,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use dioxus::core::{AttributeValue, Mutation, ScopeId, VirtualDom};
    use dioxus::document::{Document, Eval, NoOpDocument};

    use super::*;
    use crate::ui::test_dom::{listeners, total_renders, type_into, ui_sources};

    #[test]
    fn only_a_text_the_element_does_not_hold_is_pushed() {
        assert!(!needs_push("prod", "prod"));
        assert!(!needs_push("", ""));
        assert!(needs_push("", "prod"));
        assert!(needs_push("prod", "pro"));
    }

    #[test]
    fn the_push_writes_only_over_the_text_it_was_decided_against() {
        assert_eq!(
            push_text_js(7, "pro\"d", "a\"b"),
            "const el = document.querySelector('[data-dv-text=\"7\"]'); \
             if (el && el.value === \"pro\\\"d\") el.value = \"a\\\"b\";"
        );
    }

    /// Records every eval instead of running it.
    struct Recorder(Rc<RefCell<Vec<String>>>);

    impl Document for Recorder {
        fn eval(&self, js: String) -> Eval {
            self.0.borrow_mut().push(js);
            NoOpDocument.eval(String::new())
        }
    }

    #[component]
    fn Field() -> Element {
        let mut text = use_context::<Signal<String>>();
        rsx! {
            TextInput {
                id: "dv-test-field",
                "autofocus": "true",
                text: "{text}",
                oninput: move |evt: FormEvent| text.set(evt.value()),
            }
        }
    }

    /// The writes of attribute `attribute` in `edits`.
    fn writes<'a>(edits: &'a [Mutation], attribute: &str) -> Vec<&'a AttributeValue> {
        edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::SetAttribute { name, value, .. } if *name == attribute => Some(value),
                _ => None,
            })
            .collect()
    }

    /// The `value` writes in `edits`.
    fn value_writes(edits: &[Mutation]) -> Vec<&AttributeValue> {
        writes(edits, "value")
    }

    /// The whole wire, in a real `VirtualDom`: typing reaches the caller's
    /// state and writes nothing back to the element — neither a `value`
    /// attribute nor a push — while a change the user did not type is pushed
    /// exactly once. Typing several characters before a render is the race
    /// from the module docs: each one is a separate input event, and the
    /// renders between them must stay silent.
    #[test]
    fn typing_writes_nothing_back_and_a_programmatic_change_is_pushed() {
        let pushed = Rc::new(RefCell::new(Vec::<String>::new()));
        let mut dom = VirtualDom::new(Field);
        dom.provide_root_context(Rc::new(Recorder(pushed.clone())) as Rc<dyn Document>);
        let mut text = dom.in_runtime(|| Signal::new_in_scope(String::new(), ScopeId::ROOT));
        dom.provide_root_context(text);

        let mounted = dom.rebuild_to_vec().edits;
        assert!(value_writes(&mounted).is_empty(), "{mounted:#?}");
        // The caller's attributes reach the element, the quoted ones too.
        for attribute in ["id", "autofocus", "data-dv-text", "initial_value"] {
            assert!(
                mounted.iter().any(
                    |edit| matches!(edit, Mutation::SetAttribute { name, .. } if *name == attribute)
                ),
                "{attribute} is missing: {mounted:#?}"
            );
        }
        let input = listeners(&mounted, "input");
        assert_eq!(input.len(), 1, "{mounted:#?}");

        // Typing writes neither the value nor the default value: the text
        // the field mounted with stays the only text in an attribute, so a
        // password typed into it never lands in one.
        for typed in ["p", "pr", "pro", "prod"] {
            type_into(&dom, input[0], typed);
            let edits = dom.render_immediate_to_vec().edits;
            assert!(value_writes(&edits).is_empty(), "{typed}: {edits:#?}");
            assert!(
                writes(&edits, "initial_value").is_empty(),
                "{typed}: {edits:#?}"
            );
        }
        assert_eq!(dom.in_runtime(|| text.peek().clone()), "prod");
        assert!(pushed.borrow().is_empty(), "typing pushed: {pushed:?}");

        // Escape clearing a search, say.
        dom.in_runtime(|| text.set(String::new()));
        let edits = dom.render_immediate_to_vec().edits;
        assert!(value_writes(&edits).is_empty(), "{edits:#?}");
        let marker = mounted
            .iter()
            .find_map(|edit| match edit {
                Mutation::SetAttribute {
                    name: "data-dv-text",
                    value: AttributeValue::Text(marker),
                    ..
                } => marker.parse::<u64>().ok(),
                _ => None,
            })
            .expect("the element carries its marker");
        // Pushed over the text the field was last known to hold.
        assert_eq!(*pushed.borrow(), [push_text_js(marker, "prod", "")]);

        // The push is now what the field holds: setting the text back is a
        // second change, pushed over the cleared field. A note left at the
        // typed `prod` would call this no change, and the field would stay
        // empty under a state that says `prod`.
        dom.in_runtime(|| text.set("prod".into()));
        dom.render_immediate_to_vec();
        assert_eq!(
            *pushed.borrow(),
            [
                push_text_js(marker, "prod", ""),
                push_text_js(marker, "", "prod")
            ]
        );

        // Typing after the push is the user's again.
        type_into(&dom, input[0], "prods");
        let edits = dom.render_immediate_to_vec().edits;
        assert!(writes(&edits, "initial_value").is_empty(), "{edits:#?}");
        assert_eq!(pushed.borrow().len(), 2, "{pushed:?}");
    }

    #[component]
    fn Smuggled() -> Element {
        let mut text = use_context::<Signal<String>>();
        rsx! {
            TextInput {
                // The attribute a hand reaches for; `extends = input` lets it
                // compile.
                value: "{text}",
                initial_value: "{text}",
                text: "{text}",
                oninput: move |evt: FormEvent| text.set(evt.value()),
            }
        }
    }

    /// A `value` or `initial_value` passed to the field never reaches the
    /// element: the field's own `initial_value` is the one written at mount,
    /// and typing writes neither.
    #[test]
    fn a_value_passed_to_the_field_never_reaches_the_element() {
        let mut dom = VirtualDom::new(Smuggled);
        let text = dom.in_runtime(|| Signal::new_in_scope("a".to_string(), ScopeId::ROOT));
        dom.provide_root_context(text);
        let mounted = dom.rebuild_to_vec().edits;
        assert!(value_writes(&mounted).is_empty(), "{mounted:#?}");
        assert_eq!(writes(&mounted, "initial_value").len(), 1, "{mounted:#?}");
        let input = listeners(&mounted, "input");
        for typed in ["ab", "abc"] {
            type_into(&dom, input[0], typed);
            let edits = dom.render_immediate_to_vec().edits;
            assert!(value_writes(&edits).is_empty(), "{typed}: {edits:#?}");
            assert!(
                writes(&edits, "initial_value").is_empty(),
                "{typed}: {edits:#?}"
            );
        }
    }

    #[component]
    fn Counted() -> Element {
        let round = use_context::<Signal<u32>>()();
        let mut heard = use_context::<Signal<Vec<u32>>>();
        rsx! {
            TextInput {
                text: "",
                // Captures this render's `round`, as a caller's closure
                // captures its form's fields.
                oninput: move |_: FormEvent| heard.write().push(round),
            }
        }
    }

    /// The props compare without the handlers, so a parent render that only
    /// made new closures does not re-render the field — and the field must
    /// still call the newest closure, not the one it rendered with. A stale
    /// one would, say, submit a connect form with the fields as they were.
    #[test]
    fn a_field_its_parent_re_rendered_calls_the_parents_newest_handler() {
        let mut dom = VirtualDom::new(Counted);
        let (mut round, heard) = dom.in_runtime(|| {
            (
                Signal::new_in_scope(0_u32, ScopeId::ROOT),
                Signal::new_in_scope(Vec::<u32>::new(), ScopeId::ROOT),
            )
        });
        dom.provide_root_context(round);
        dom.provide_root_context(heard);
        let input = listeners(&dom.rebuild_to_vec().edits, "input");
        let renders = total_renders(&dom);

        dom.in_runtime(|| round.set(1));
        dom.render_immediate_to_vec();
        // The parent, and not the field.
        assert_eq!(total_renders(&dom) - renders, 1);

        type_into(&dom, input[0], "x");
        assert_eq!(dom.in_runtime(|| heard.peek().clone()), [1]);
    }

    /// Whether `code` binds `value:` as an attribute of its own — not as the
    /// tail of `initial_value:` or another name.
    fn binds_value(code: &str) -> bool {
        code.match_indices("value:").any(|(at, _)| {
            !code[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    }

    /// The text after the opening brace when `line` opens a text field —
    /// raw or through this module — else `None`.
    fn opens_text_field(line: &str) -> Option<&str> {
        ["input {", "textarea {", "TextInput {", "TextArea {"]
            .iter()
            .find_map(|owner| line.trim_start().strip_prefix(owner))
    }

    /// No text field under `src/ui` binds a `value` attribute: on a raw
    /// `input` or `textarea` it is the pattern that lost keystrokes, and on
    /// `TextInput`/`TextArea` it is dropped, so writing it there reads as
    /// handled when it is not. A `value:` line belongs to the nearest
    /// less-indented line above it, which is how rustfmt lays out `rsx!`; the
    /// owner may carry attributes on its own line (`input { id: "x",`), and
    /// a field written on one line is checked whole. Checkboxes and selects
    /// carry `checked`/`value` on other elements and are not text fields.
    #[test]
    fn no_text_field_binds_a_value_attribute() {
        let mut found = Vec::new();
        for (path, source) in ui_sources() {
            if path.len() == 1 && path[0] == "text_field.rs" {
                // The tests above pass one on purpose.
                continue;
            }
            let lines: Vec<&str> = source.lines().collect();
            for (at, line) in lines.iter().enumerate() {
                let indent = |l: &str| l.len() - l.trim_start().len();
                let hit = if let Some(rest) = opens_text_field(line) {
                    binds_value(rest)
                } else if binds_value(line) && line.trim_start().starts_with("value:") {
                    lines[..at]
                        .iter()
                        .rev()
                        .find(|above| indent(above) < indent(line))
                        .is_some_and(|owner| opens_text_field(owner).is_some())
                } else {
                    false
                };
                if hit {
                    found.push(format!("{}:{}", path.join("/"), at + 1));
                }
            }
        }
        assert!(found.is_empty(), "drop the value attribute: {found:#?}");
    }
}
