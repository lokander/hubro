//! The cards that take over when a connect parks on a question: the database
//! password or SSH key passphrase, trusting an unknown SSH host key, and the
//! Microsoft Entra sign-in. Two of them carry a "remember" box, whose answer
//! decides what the keyring keeps.

use dioxus::prelude::*;

use crate::ui::classes::{BTN_CANCEL, BTN_PRIMARY, FORM_INPUT};
use crate::ui::state::{AppState, Remember, ServerBackend};
use crate::ui::text_field::TextInput;

/// Cancel for the three connect prompt cards (password/passphrase, host-key
/// trust, Entra sign-in): clears the card, and with it any saved-connection
/// edit still waiting for the parked connect to confirm it — an
/// abandoned connect must never rewrite the entry it started from.
fn cancel_prompt<T: 'static>(state: AppState, mut prompt: Signal<Option<T>>) {
    state.clear_pending_edit();
    prompt.set(None);
}

/// Inline secret prompt for a saved Postgres or SQL Server connection: the
/// database password, or the SSH key passphrase when the tunnel's key is
/// encrypted (Postgres only). "Remember" stores the secret in the OS keyring;
/// without it (or when no keyring is available) it lives in session memory
/// only.
#[component]
pub(super) fn PasswordPromptCard(prompt: crate::ui::state::PasswordPrompt) -> Element {
    use crate::ui::state::PromptKind;
    let state = use_context::<AppState>();
    let mut password = use_signal(String::new);
    // Seeded from the prompt, not from `true`: a re-prompt after a rejected
    // passphrase carries the answer the user already gave.
    //
    // The seed only decides the box when this card was actually unmounted in
    // between — its `key:` is url + kind, which a re-prompt for the same
    // connection reuses. It is unmounted in practice, because the submit
    // clears `password_prompt` before the retry awaits. Both routes land on
    // the same value anyway: a surviving signal is holding the choice the user
    // just made, which is what the prompt now carries.
    let offered = prompt.remember;
    let mut remember = use_signal(move || offered);
    let prompt_for_submit = prompt.clone();
    let submit = move || {
        let prompt = prompt_for_submit.clone();
        let entered = password.peek().clone();
        let remember_choice = *remember.peek();
        // spawn_forever: the connect flow clears `password_prompt`, which
        // unmounts this card — a scope-tied `spawn` would be cancelled at
        // its next await, silently abandoning the connect.
        dioxus::core::spawn_forever(async move {
            // The prompt carries the backend the parked connect was for, so
            // the retry resumes on the same engine.
            match prompt.kind {
                PromptKind::DbPassword => {
                    state
                        .connect_server_with_password(
                            ServerBackend::of(prompt.backend),
                            prompt.url,
                            prompt.name,
                            entered,
                            Some(remember_choice),
                            prompt.tunnel,
                        )
                        .await;
                }
                PromptKind::SshPassphrase => {
                    state
                        .connect_server_with_ssh_passphrase(prompt, entered, remember_choice)
                        .await;
                }
            }
        });
    };
    rsx! {
        div { class: "w-full max-w-xl rounded border border-cyan-300 dark:border-cyan-800 bg-slate-50 dark:bg-slate-950/80 p-4",
            p { class: "mb-2 text-sm text-slate-900 dark:text-slate-300",
                match prompt.kind {
                    PromptKind::DbPassword => "Password for ",
                    PromptKind::SshPassphrase => "SSH key passphrase for ",
                }
                span { class: "font-mono text-cyan-700 dark:text-cyan-300", "{prompt.name}" }
            }
            div { class: "flex gap-2",
                TextInput {
                    r#type: "password",
                    class: "min-w-0 flex-1 {FORM_INPUT}",
                    "autofocus": "true",
                    text: "{password}",
                    oninput: move |evt: FormEvent| password.set(evt.value()),
                    onkeydown: {
                        let submit = submit.clone();
                        move |evt: KeyboardEvent| {
                            if evt.key() == Key::Enter {
                                submit();
                            }
                        }
                    },
                }
                button {
                    class: "{BTN_PRIMARY} bg-cyan-700 hover:bg-cyan-600",
                    onclick: move |_| submit(),
                    "Connect"
                }
                button {
                    class: BTN_CANCEL,
                    onclick: move |_| cancel_prompt(state, state.password_prompt),
                    "Cancel"
                }
            }
            label { class: "mt-2 flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                input {
                    r#type: "checkbox",
                    checked: remember().checkbox(),
                    onchange: move |evt| remember.set(Remember::from_checkbox(evt.checked())),
                }
                "Remember in the system keyring (falls back to this session only)"
            }
            if let Some(hint) = remember_hint(prompt.kind, prompt.stored) {
                p { class: "mt-1 text-xs text-amber-700 dark:text-amber-400", "{hint}" }
            }
        }
    }
}

/// The line under the password prompt's "remember" box, saying what an
/// unticked box does to a password already in the keyring — `None` where it
/// does nothing, and the label alone is the whole story.
///
/// Only a database-password prompt's answer can delete a stored copy, and only
/// once a *different* password has connected
/// (`stored_password_after_connect`); an unticked SSH-passphrase prompt never
/// deletes one. When the connect could not tell whether a copy is stored, the
/// line says so rather than staying silent about a deletion that may happen.
pub(super) fn remember_hint(
    kind: crate::ui::state::PromptKind,
    stored: crate::ui::state::StoredCopy,
) -> Option<&'static str> {
    use crate::ui::state::{PromptKind, StoredCopy};
    match (kind, stored) {
        (PromptKind::DbPassword, StoredCopy::Present) => Some(
            "A password for this connection is stored in the system keyring. \
             Leaving this unticked removes it once a different password connects.",
        ),
        (PromptKind::DbPassword, StoredCopy::Unknown) => Some(
            "If a password for this connection is stored in the system keyring, \
             leaving this unticked removes it once a different password connects.",
        ),
        (PromptKind::DbPassword, StoredCopy::Absent) | (PromptKind::SshPassphrase, _) => None,
    }
}

/// Trust-on-first-use prompt for an unrecognized SSH host key. Shows the
/// server's fingerprint so the user can compare it out-of-band; trusting
/// records the key in hubro's known_hosts store and retries the connect.
#[component]
pub(super) fn HostKeyPromptCard(prompt: crate::ui::state::HostKeyPrompt) -> Element {
    let state = use_context::<AppState>();
    let prompt_for_trust = prompt.clone();
    let trust = move || {
        let prompt = prompt_for_trust.clone();
        // spawn_forever: trusting clears `host_key_prompt`, unmounting this
        // card — a scope-tied `spawn` would be cancelled mid-connect.
        dioxus::core::spawn_forever(async move {
            state.trust_host_and_connect(prompt).await;
        });
    };
    rsx! {
        div { class: "w-full max-w-xl rounded border border-amber-400 dark:border-amber-700 bg-amber-50 dark:bg-amber-950/40 p-4",
            p { class: "mb-1 text-sm font-medium text-amber-800 dark:text-amber-300",
                "Unrecognized SSH host key"
            }
            p { class: "mb-2 text-xs text-slate-600 dark:text-slate-400",
                "The server "
                span { class: "font-mono text-slate-900 dark:text-slate-200",
                    "{prompt.info.host}:{prompt.info.port}"
                }
                " is not in your known_hosts. Verify the fingerprint below matches the server before trusting it."
            }
            div { class: "mb-3 rounded bg-slate-100 dark:bg-slate-950 px-3 py-2 font-mono text-xs text-slate-800 dark:text-slate-200",
                div { "{prompt.info.key_type}" }
                div { class: "break-all", "{prompt.info.fingerprint}" }
            }
            div { class: "flex gap-2",
                button {
                    class: "{BTN_PRIMARY} bg-amber-600 hover:bg-amber-500",
                    onclick: move |_| trust(),
                    "Trust and connect"
                }
                button {
                    class: BTN_CANCEL,
                    onclick: move |_| cancel_prompt(state, state.host_key_prompt),
                    "Cancel"
                }
            }
        }
    }
}

/// Interactive Entra sign-in prompt. A Postgres or SQL Server connect
/// needs a Microsoft browser sign-in (no cached refresh token); the button
/// opens the browser and completes the connect, Cancel abandons it.
/// "Remember" keeps the sign-in's refresh token in the OS keyring, so later
/// connects and startup sign in silently; without it the sign-in lasts this
/// session only.
#[component]
pub(super) fn EntraSignInCard(prompt: crate::ui::state::EntraPrompt) -> Element {
    let state = use_context::<AppState>();
    // Seeded from the prompt, which carries the answer back after a failed
    // sign-in, so a cleared box is not re-ticked.
    let offered = prompt.remember;
    let mut remember = use_signal(move || offered);
    let prompt_for_signin = prompt.clone();
    let sign_in = move || {
        let mut prompt = prompt_for_signin.clone();
        prompt.remember = *remember.peek();
        // spawn_forever: signing in clears `entra_prompt`, unmounting this card;
        // a scope-tied spawn would be cancelled mid sign-in.
        dioxus::core::spawn_forever(async move {
            let backend = ServerBackend::of(prompt.backend);
            state
                .connect_server_with_entra_signin(backend, prompt)
                .await;
        });
    };
    rsx! {
        div { class: "w-full max-w-xl rounded border border-sky-300 dark:border-sky-800 bg-slate-50 dark:bg-slate-950/80 p-4",
            p { class: "mb-1 text-sm font-medium text-slate-900 dark:text-slate-200",
                "Sign in with Microsoft"
            }
            p { class: "mb-3 text-xs text-slate-600 dark:text-slate-400",
                "Connecting to "
                span { class: "font-mono text-cyan-700 dark:text-cyan-300", "{prompt.name}" }
                " opens your browser to sign in to Microsoft Entra ID."
            }
            div { class: "flex gap-2",
                button {
                    class: "{BTN_PRIMARY} bg-sky-600 hover:bg-sky-500",
                    onclick: move |_| sign_in(),
                    "Sign in with Microsoft"
                }
                button {
                    class: BTN_CANCEL,
                    onclick: move |_| cancel_prompt(state, state.entra_prompt),
                    "Cancel"
                }
            }
            label { class: "mt-2 flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                input {
                    r#type: "checkbox",
                    checked: remember().checkbox(),
                    onchange: move |evt| remember.set(Remember::from_checkbox(evt.checked())),
                }
                "Stay signed in: remember in the system keyring (falls back to this session only)"
            }
            if let Some(hint) = entra_remember_hint(prompt.stored) {
                p { class: "mt-1 text-xs text-amber-700 dark:text-amber-400", "{hint}" }
            }
        }
    }
}

/// The line under the sign-in card's "stay signed in" box, saying what an
/// unticked box does to a sign-in already remembered in the keyring: once the
/// sign-in connects, `place_entra_refresh` answers `Delete` and the remembered
/// refresh token goes. `None` where no remembered sign-in is known to remain.
fn entra_remember_hint(stored: crate::ui::state::StoredCopy) -> Option<&'static str> {
    use crate::ui::state::StoredCopy;
    match stored {
        StoredCopy::Present => Some(
            "A sign-in for this connection is remembered in the system keyring. \
             Leaving this unticked removes it once this sign-in connects.",
        ),
        StoredCopy::Unknown => Some(
            "If a sign-in for this connection is remembered in the system keyring, \
             leaving this unticked removes it once this sign-in connects.",
        ),
        StoredCopy::Absent => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::connections::test_support::shipped_code_of;

    #[test]
    fn the_prompt_warns_that_unticking_removes_a_stored_password_only_when_it_can() {
        use crate::ui::state::{PromptKind, StoredCopy};
        let stored = remember_hint(PromptKind::DbPassword, StoredCopy::Present)
            .expect("a stored database password must be mentioned under the box");
        assert!(
            stored.contains("is stored") && stored.contains("unticked removes it"),
            "{stored}"
        );
        let unknown = remember_hint(PromptKind::DbPassword, StoredCopy::Unknown)
            .expect("a possibly stored database password must be mentioned under the box");
        assert!(
            unknown.starts_with("If ") && unknown.contains("unticked removes it"),
            "an unconfirmed copy must be hedged, not asserted: {unknown}"
        );
        // Nothing stored: the label alone, as before.
        assert_eq!(
            remember_hint(PromptKind::DbPassword, StoredCopy::Absent),
            None
        );
        // An unticked passphrase prompt deletes nothing, whatever is stored.
        for stored in [StoredCopy::Present, StoredCopy::Absent, StoredCopy::Unknown] {
            assert_eq!(remember_hint(PromptKind::SshPassphrase, stored), None);
        }
    }

    #[test]
    fn the_sign_in_card_warns_about_a_remembered_sign_in_only_when_one_remains() {
        use crate::ui::state::StoredCopy;
        let present = entra_remember_hint(StoredCopy::Present)
            .expect("a remembered sign-in an unticked box deletes must be mentioned");
        assert!(
            present.contains("is remembered") && present.contains("unticked removes it"),
            "{present}"
        );
        let unknown = entra_remember_hint(StoredCopy::Unknown)
            .expect("a possibly remembered sign-in must be mentioned");
        assert!(unknown.starts_with("If "), "{unknown}");
        assert_eq!(entra_remember_hint(StoredCopy::Absent), None);
    }

    #[test]
    fn the_prompt_card_offers_the_choice_the_prompt_carries() {
        // The last hop of the "remember passphrase" wire, and the one that
        // can silently undo all of it: `withdraw_ssh_remember` can compute
        // the right answer and `PasswordPrompt` can carry it faithfully while
        // the card initialises
        // its checkbox from a literal `true` — which is exactly the code this
        // replaced. Every state-side test stays green through that.
        //
        // A component needs a Dioxus runtime to render, so the seed is checked
        // at its source: the card is a function, and the initialiser is one
        // line of it. Read as code alone, the card found exactly once in the
        // module and cut at its own closing brace ([`shipped_code_of`]), then
        // split at `rsx!` into the setup and what renders.
        let card = shipped_code_of("fn PasswordPromptCard(");
        let renders_at = card
            .find("rsx!")
            .expect("the card must still exist and render");
        let (body, rendered) = card.split_at(renders_at);
        assert!(
            !body.contains("use_signal(|| true)")
                && !body.contains("use_signal(|| Remember::Keyring)"),
            "the card re-ticks the remember box on every prompt, so unticking \
             it and then mistyping the passphrase silently restores the \
             decision to store it: {body}"
        );
        // Polarity, not merely the mention: `let offered = !prompt.remember;`
        // reads as correctly wired up while offering every user the opposite
        // of what they chose.
        assert!(
            body.contains("let offered = prompt.remember;")
                && body.contains("use_signal(move || offered)"),
            "the card ignores the choice the prompt carries, or inverts it: {body}"
        );
        // The other end of the same value, which this slice already covers:
        // what the box holds when the card submits. This read predates the
        // fix that built the rest of the wire and was unchanged by it, but it
        // is the same one-line inversion at the same seam — and
        // `!*remember.peek()` would send every user the opposite of the box
        // they ticked, on the read this test is already looking at.
        assert!(
            body.contains("let remember_choice = *remember.peek();"),
            "the card submits something other than the box as ticked, so the \
             answer the user gave is not the one that reaches the keyring \
             decision: {body}"
        );
        // ...and hands it on as an answer. `None` means "nobody was asked",
        // which leaves a stale stored password in place whatever the box said.
        // (The card always renders its box, so it always has an answer.)
        assert!(
            body.contains("Some(remember_choice),"),
            "the card no longer hands the database password its answer: {body}"
        );
        // And the third place the same value passes through: what the rendered
        // box displays. Rendering is not testable without a runtime, but the
        // *binding* is text like the other two, and `checked: !remember().checkbox()`
        // shows every user the opposite of the state they are about to submit.
        // Split at `rsx!`, so all three ends of the value — seed, display,
        // submit — are covered by one test.
        assert!(
            rendered.contains("checked: remember().checkbox(),"),
            "the checkbox displays something other than the choice it holds, \
             so the box the user sees and the answer the card submits can \
             disagree: {rendered}"
        );
    }
}
