//! The SSH tunnel part of a server connection form: its signals as one
//! [`SshTunnelState`], the fieldset that edits them, and the tunnel config the
//! submit builds from them.

use std::path::PathBuf;

use dioxus::prelude::*;

use crate::config::EditPrefill;
use crate::ui::classes::FORM_INPUT;
use crate::ui::text_field::TextInput;

/// The signals a server form's SSH tunnel fields are held in, the way
/// [`ServerFormState`](super::forms::ServerFormState) holds the rest of the
/// form: handed to [`SshTunnelFields`] as one prop, and read by the submit
/// (signals are `Copy`).
#[derive(Clone, Copy, PartialEq)]
pub(super) struct SshTunnelState {
    pub(super) use_tunnel: Signal<bool>,
    pub(super) ssh_host: Signal<String>,
    pub(super) ssh_port: Signal<String>,
    pub(super) ssh_user: Signal<String>,
    /// false = ssh-agent (the default), true = key file.
    pub(super) ssh_use_key: Signal<bool>,
    pub(super) ssh_key_path: Signal<String>,
    /// Never prefilled.
    pub(super) ssh_passphrase: Signal<String>,
}

/// The tunnel fields' signals, seeded from the entry being edited (or the
/// defaults). A hook: it calls `use_signal` seven times, always in this
/// order, so it must be called unconditionally, in the same place.
pub(super) fn use_ssh_tunnel(seed: &EditPrefill) -> SshTunnelState {
    SshTunnelState {
        use_tunnel: use_signal(|| seed.tunnel.is_some()),
        ssh_host: use_signal(|| seed.ssh_host.clone()),
        ssh_port: use_signal(|| seed.ssh_port.clone()),
        ssh_user: use_signal(|| seed.ssh_user.clone()),
        ssh_use_key: use_signal(|| seed.ssh_use_key),
        ssh_key_path: use_signal(|| seed.ssh_key_path.clone()),
        ssh_passphrase: use_signal(String::new),
    }
}

/// Builds the tunnel config from the SSH form fields — `Ok(None)` when the
/// toggle is off. Shared by both engines' forms; errors are
/// user-facing form errors, raised before any connect attempt.
pub(super) fn tunnel_from_form(
    use_tunnel: bool,
    host: &str,
    port: &str,
    user: &str,
    use_key: bool,
    key_path: &str,
) -> Result<Option<crate::tunnel::TunnelConfig>, String> {
    use crate::tunnel::{TunnelAuth, TunnelConfig};
    if !use_tunnel {
        return Ok(None);
    }
    let host = host.trim().to_string();
    if host.is_empty() {
        return Err("SSH host must not be empty".to_string());
    }
    let port_text = port.trim();
    let port = if port_text.is_empty() {
        22
    } else {
        match port_text.parse::<u16>() {
            // 0 parses as a valid u16 but is not a usable port.
            Ok(0) | Err(_) => return Err(format!("invalid SSH port: {port_text}")),
            Ok(port) => port,
        }
    };
    let user = user.trim().to_string();
    if user.is_empty() {
        return Err("SSH user must not be empty".to_string());
    }
    let auth = if use_key {
        let path = key_path.trim().to_string();
        if path.is_empty() {
            return Err("SSH key file path must not be empty".to_string());
        }
        // The placeholder suggests ~/.ssh/…, so honor a leading ~/.
        let path = match path.strip_prefix("~/") {
            Some(rest) => match dirs::home_dir() {
                Some(home) => home.join(rest),
                None => PathBuf::from(path),
            },
            None => PathBuf::from(path),
        };
        TunnelAuth::KeyFile { path }
    } else {
        TunnelAuth::Agent
    };
    Ok(Some(TunnelConfig {
        host,
        port,
        user,
        auth,
    }))
}

/// The "Connect through an SSH tunnel" toggle and its fieldset, in
/// [`ServerForm`](super::forms::ServerForm). `radio_group` names the auth
/// radios' group, one per engine
/// ([`ServerSpec::radio_group`](super::forms::ServerSpec::radio_group)).
#[component]
pub(super) fn SshTunnelFields(tunnel: SshTunnelState, radio_group: String) -> Element {
    let SshTunnelState {
        mut use_tunnel,
        mut ssh_host,
        mut ssh_port,
        mut ssh_user,
        mut ssh_use_key,
        mut ssh_key_path,
        mut ssh_passphrase,
    } = tunnel;
    rsx! {
        label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
            input {
                r#type: "checkbox",
                checked: use_tunnel(),
                onchange: move |evt| use_tunnel.set(evt.checked()),
            }
            "Connect through an SSH tunnel"
        }
        if use_tunnel() {
            div { class: "flex flex-col gap-2 rounded border border-slate-200 dark:border-slate-800 bg-slate-100 dark:bg-slate-900/60 p-3",
                div { class: "flex gap-2",
                    TextInput {
                        class: "{FORM_INPUT} flex-[3]",
                        placeholder: "ssh host",
                        text: "{ssh_host}",
                        oninput: move |evt: FormEvent| ssh_host.set(evt.value()),
                    }
                    TextInput {
                        class: "{FORM_INPUT} flex-1",
                        placeholder: "22",
                        text: "{ssh_port}",
                        oninput: move |evt: FormEvent| ssh_port.set(evt.value()),
                    }
                }
                TextInput {
                    class: FORM_INPUT,
                    placeholder: "ssh user",
                    text: "{ssh_user}",
                    oninput: move |evt: FormEvent| ssh_user.set(evt.value()),
                }
                div { class: "flex gap-4 text-xs text-slate-500 dark:text-slate-400",
                    label { class: "flex items-center gap-2",
                        input {
                            r#type: "radio",
                            name: "{radio_group}",
                            checked: !ssh_use_key(),
                            onchange: move |_| ssh_use_key.set(false),
                        }
                        "ssh-agent"
                    }
                    label { class: "flex items-center gap-2",
                        input {
                            r#type: "radio",
                            name: "{radio_group}",
                            checked: ssh_use_key(),
                            onchange: move |_| ssh_use_key.set(true),
                        }
                        "key file"
                    }
                }
                if ssh_use_key() {
                    TextInput {
                        class: FORM_INPUT,
                        placeholder: "key file path, e.g. ~/.ssh/id_ed25519",
                        text: "{ssh_key_path}",
                        oninput: move |evt: FormEvent| ssh_key_path.set(evt.value()),
                    }
                    TextInput {
                        r#type: "password",
                        class: FORM_INPUT,
                        placeholder: "key passphrase (if the key is encrypted)",
                        text: "{ssh_passphrase}",
                        oninput: move |evt: FormEvent| ssh_passphrase.set(evt.value()),
                    }
                }
            }
        }
    }
}
