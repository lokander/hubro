use std::path::PathBuf;

use dioxus::prelude::*;

use crate::config::Theme;
use crate::db::ConnectionId;

use super::editor::SqlEditor;
use super::grid::DataGrid;
use super::sidebar::SchemaSidebar;
use super::state::{ActiveView, AppState};

/// Top-level layout: tab bar over the active view.
#[component]
pub fn Shell() -> Element {
    let state = use_context::<AppState>();
    let active = *state.active.read();
    let dark = state.dark;
    rsx! {
        // The `.dark` class gates every `dark:` utility below it (see the
        // @custom-variant in tailwind.css); toggling it swaps the theme.
        div {
            class: if dark() {
                "dark flex h-screen flex-col bg-white dark:bg-slate-900 text-slate-900 dark:text-slate-100"
            } else {
                "flex h-screen flex-col bg-white dark:bg-slate-900 text-slate-900 dark:text-slate-100"
            },
            TabBar {}
            main { class: "min-h-0 flex-1",
                match active {
                    ActiveView::Connections => rsx! { ConnectionsScreen {} },
                    // Keyed so per-tab hook state never leaks across tabs.
                    ActiveView::Connection(id) => rsx! { ConnectionView { key: "{id:?}", id } },
                }
            }
        }
    }
}

/// One tab per open connection, plus a fixed tab for the connections screen.
#[component]
fn TabBar() -> Element {
    let mut state = use_context::<AppState>();
    let active = *state.active.read();
    // Owned copies so the loop can hand ids/names to event handlers.
    let tabs: Vec<(ConnectionId, String)> = state
        .registry
        .read()
        .iter()
        .map(|c| (c.id, c.name.clone()))
        .collect();
    rsx! {
        header { class: "flex items-center gap-1 border-b border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 pt-1",
            button {
                class: if active == ActiveView::Connections {
                    "rounded-t px-3 py-1.5 text-sm bg-white dark:bg-slate-900 text-slate-900 dark:text-slate-100"
                } else {
                    "rounded-t px-3 py-1.5 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100"
                },
                onclick: move |_| state.active.set(ActiveView::Connections),
                "Connections"
            }
            for (id, name) in tabs {
                div {
                    class: if active == ActiveView::Connection(id) {
                        "flex items-center gap-1 rounded-t bg-white dark:bg-slate-900 px-3 py-1.5 text-sm text-slate-900 dark:text-slate-100"
                    } else {
                        "flex items-center gap-1 rounded-t px-3 py-1.5 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100"
                    },
                    button {
                        onclick: move |_| state.active.set(ActiveView::Connection(id)),
                        "{name}"
                    }
                    button {
                        class: "rounded px-1 text-slate-500 hover:bg-slate-300 dark:hover:bg-slate-700 hover:text-slate-900 dark:hover:text-slate-200",
                        aria_label: "Close connection",
                        onclick: move |_| state.close_connection(id),
                        "×"
                    }
                }
            }
            ThemeToggle {}
        }
    }
}

/// Right-aligned control in the tab bar that cycles System → Light → Dark.
#[component]
fn ThemeToggle() -> Element {
    let state = use_context::<AppState>();
    let theme = *state.theme.read();
    let icon = match theme {
        Theme::System => "◐",
        Theme::Light => "☀",
        Theme::Dark => "☾",
    };
    rsx! {
        button {
            class: "ml-auto rounded px-2 py-1 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100",
            title: "Theme (click to cycle System / Light / Dark)",
            aria_label: "Toggle theme",
            onclick: move |_| state.set_theme(theme.next()),
            "{icon} {theme.label()}"
        }
    }
}

/// Layout for one open connection: schema sidebar left, data grid right.
#[component]
fn ConnectionView(id: ConnectionId) -> Element {
    let state = use_context::<AppState>();
    let name = state.registry.read().get(id).map(|c| c.name.clone());
    let Some(name) = name else {
        // Tab was closed under us; the view switches away on the next render.
        return rsx! {
            div { class: "p-8 text-slate-500 dark:text-slate-400", "This connection is closed." }
        };
    };
    let (selected, pane) = {
        let tab_ui = state.tab_ui.read();
        let ui = tab_ui.get(&id);
        (
            ui.and_then(|ui| ui.selected_table.clone()),
            ui.map(|ui| ui.pane).unwrap_or_default(),
        )
    };
    rsx! {
        div { class: "flex h-full",
            aside { class: "flex w-72 shrink-0 flex-col border-r border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/50",
                h2 { class: "border-b border-slate-200 dark:border-slate-800 px-4 py-3 font-mono text-sm text-slate-900 dark:text-slate-300",
                    "{name}"
                }
                SchemaSidebar { id }
            }
            section { class: "flex min-w-0 flex-1 flex-col",
                div { class: "flex gap-1 border-b border-slate-200 dark:border-slate-800 px-3 py-1.5",
                    PaneButton { id, pane, target: super::state::Pane::Browser, label: "Data" }
                    PaneButton { id, pane, target: super::state::Pane::Sql, label: "SQL" }
                }
                match pane {
                    super::state::Pane::Sql => rsx! {
                        SqlEditor { key: "sql-{id:?}", id }
                    },
                    super::state::Pane::Browser => match selected {
                        // Keyed by table so grid state (page, sort, filter)
                        // resets when another table is selected.
                        Some(table) => rsx! {
                            DataGrid { key: "{table.key()}", id, table: table.clone() }
                        },
                        None => rsx! {
                            div { class: "flex flex-1 items-center justify-center",
                                p { class: "text-slate-500", "Select a table to view its data." }
                            }
                        },
                    },
                }
            }
        }
    }
}

/// One segment of the Data / SQL pane switch.
#[component]
fn PaneButton(
    id: ConnectionId,
    pane: super::state::Pane,
    target: super::state::Pane,
    label: String,
) -> Element {
    let state = use_context::<AppState>();
    rsx! {
        button {
            class: if pane == target {
                "rounded px-3 py-0.5 text-xs bg-slate-300 dark:bg-slate-700 text-slate-900 dark:text-slate-100"
            } else {
                "rounded px-3 py-0.5 text-xs text-slate-500 dark:text-slate-400 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-100"
            },
            onclick: move |_| state.set_pane(id, target),
            "{label}"
        }
    }
}

/// One row of the saved list, precomputed for rendering.
#[derive(Clone, PartialEq)]
struct SavedRow {
    name: String,
    locator: String,
    is_postgres: bool,
    is_open: bool,
    tunnel: Option<crate::tunnel::TunnelConfig>,
}

/// Launch screen: the persisted saved-connections list plus add flows for
/// SQLite (native file picker) and Postgres (form or URL).
#[component]
fn ConnectionsScreen() -> Element {
    let state = use_context::<AppState>();
    let mut show_pg_form = use_signal(|| false);
    let error = state.connect_error.read().clone();
    let prompt = state.password_prompt.read().clone();
    let saved: Vec<SavedRow> = {
        let open = state.open_locators.read();
        state
            .saved
            .read()
            .entries()
            .iter()
            .map(|s| {
                let canonical_locator = match s {
                    crate::config::SavedConnection::Sqlite { path, .. } => {
                        super::state::canonical(path).display().to_string()
                    }
                    crate::config::SavedConnection::Postgres { url, .. } => url.clone(),
                };
                let tunnel = match s {
                    crate::config::SavedConnection::Postgres { tunnel, .. } => tunnel.clone(),
                    _ => None,
                };
                SavedRow {
                    name: s.name().to_string(),
                    locator: s.locator(),
                    is_postgres: matches!(s, crate::config::SavedConnection::Postgres { .. }),
                    is_open: open.iter().any(|(_, l)| *l == canonical_locator),
                    tunnel,
                }
            })
            .collect()
    };

    let pick_file = move |_| {
        spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Add a SQLite database")
                .add_filter("SQLite databases", &["db", "sqlite", "sqlite3"])
                .add_filter("All files", &["*"])
                .pick_file()
                .await;
            if let Some(file) = picked {
                state.add_saved(file.path().to_path_buf());
            }
        });
    };

    rsx! {
        div { class: "flex h-full flex-col items-center justify-center gap-6 overflow-y-auto py-8",
            div { class: "text-center",
                h1 { class: "text-2xl font-semibold text-slate-900 dark:text-slate-200", "dataview" }
                p { class: "mt-1 text-sm text-slate-500 dark:text-slate-400",
                    if saved.is_empty() {
                        "Add a SQLite file or a Postgres server to get started."
                    } else {
                        "Pick a saved connection, or add another database."
                    }
                }
            }
            if !saved.is_empty() {
                ul { class: "w-full max-w-xl divide-y divide-slate-200 dark:divide-slate-800 rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/60",
                    for row in saved {
                        li { class: "flex items-center gap-3 px-4 py-3",
                            button {
                                class: "min-w-0 flex-1 text-left",
                                onclick: {
                                    let row = row.clone();
                                    move |_| {
                                        let row = row.clone();
                                        spawn(async move {
                                            if row.is_postgres {
                                                state
                                                    .connect_postgres(row.locator, row.name, row.tunnel)
                                                    .await;
                                            } else {
                                                state.connect(PathBuf::from(row.locator)).await;
                                            }
                                        });
                                    }
                                },
                                div { class: "flex items-center gap-2",
                                    span { class: "truncate text-sm font-medium text-slate-900 dark:text-slate-200",
                                        "{row.name}"
                                    }
                                    span {
                                        class: if row.is_postgres {
                                            "rounded bg-cyan-100 dark:bg-cyan-900/50 px-1.5 py-0.5 text-xs text-cyan-700 dark:text-cyan-300"
                                        } else {
                                            "rounded bg-slate-200 dark:bg-slate-800 px-1.5 py-0.5 text-xs text-slate-500 dark:text-slate-400"
                                        },
                                        if row.is_postgres { "postgres" } else { "sqlite" }
                                    }
                                    if row.tunnel.is_some() {
                                        span { class: "rounded bg-teal-100 dark:bg-teal-900/50 px-1.5 py-0.5 text-xs text-teal-700 dark:text-teal-300",
                                            "ssh"
                                        }
                                    }
                                    if row.is_open {
                                        span { class: "rounded bg-sky-100 dark:bg-sky-900/60 px-1.5 py-0.5 text-xs text-sky-700 dark:text-sky-300",
                                            "open"
                                        }
                                    }
                                }
                                div { class: "truncate font-mono text-xs text-slate-500",
                                    "{row.locator}"
                                }
                            }
                            button {
                                class: "rounded px-2 py-1 text-xs text-slate-500 hover:bg-slate-200 dark:hover:bg-slate-800 hover:text-slate-900 dark:hover:text-slate-200",
                                aria_label: "Remove saved connection",
                                onclick: {
                                    let locator = row.locator.clone();
                                    move |_| state.remove_saved(&locator)
                                },
                                "Remove"
                            }
                        }
                    }
                }
            }
            if let Some(prompt) = prompt {
                // Keyed on kind too: moving from the SSH-passphrase prompt to
                // the db-password prompt for the same URL resets the input.
                PasswordPromptCard { key: "{prompt.url}:{prompt.kind:?}", prompt }
            }
            div { class: "flex gap-3",
                button {
                    class: "rounded bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500",
                    onclick: pick_file,
                    "Add SQLite file…"
                }
                button {
                    class: "rounded bg-cyan-700 px-4 py-2 text-sm font-medium text-white hover:bg-cyan-600",
                    onclick: move |_| {
                        let showing = *show_pg_form.read();
                        show_pg_form.set(!showing);
                    },
                    "Add Postgres…"
                }
            }
            if show_pg_form() {
                PostgresForm { on_done: move |_| show_pg_form.set(false) }
            }
            if let Some(err) = error {
                p { class: "max-w-xl px-8 text-sm text-red-600 dark:text-red-400", "{err}" }
            }
        }
    }
}

/// Inline secret prompt for a saved Postgres connection: the database
/// password, or the SSH key passphrase when the tunnel's key is encrypted.
/// "Remember" stores the secret in the OS keyring; without it (or when no
/// keyring is available) it lives in session memory only.
#[component]
fn PasswordPromptCard(prompt: super::state::PasswordPrompt) -> Element {
    use super::state::PromptKind;
    let state = use_context::<AppState>();
    let mut password = use_signal(String::new);
    let mut remember = use_signal(|| true);
    let prompt_for_submit = prompt.clone();
    let submit = move || {
        let prompt = prompt_for_submit.clone();
        let entered = password.peek().clone();
        let remember_choice = *remember.peek();
        // spawn_forever: the connect flow clears `password_prompt`, which
        // unmounts this card — a scope-tied `spawn` would be cancelled at
        // its next await, silently abandoning the connect.
        dioxus::core::spawn_forever(async move {
            match prompt.kind {
                PromptKind::DbPassword => {
                    state
                        .connect_postgres_with_password(
                            prompt.url,
                            prompt.name,
                            entered,
                            remember_choice,
                            prompt.tunnel,
                        )
                        .await;
                }
                PromptKind::SshPassphrase => {
                    // An SSH prompt always carries its tunnel config.
                    let Some(tunnel) = prompt.tunnel else { return };
                    state
                        .connect_postgres_with_ssh_passphrase(
                            prompt.url,
                            prompt.name,
                            tunnel,
                            entered,
                            remember_choice,
                        )
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
                input {
                    r#type: "password",
                    class: "min-w-0 flex-1 rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-3 py-2 font-mono text-sm text-slate-900 dark:text-slate-200",
                    autofocus: true,
                    value: "{password}",
                    oninput: move |evt| password.set(evt.value()),
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
                    class: "rounded bg-cyan-700 px-4 py-2 text-sm font-medium text-white hover:bg-cyan-600",
                    onclick: move |_| submit(),
                    "Connect"
                }
                button {
                    class: "rounded px-3 py-2 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| state.password_prompt.clone().set(None),
                    "Cancel"
                }
            }
            label { class: "mt-2 flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                input {
                    r#type: "checkbox",
                    checked: remember(),
                    onchange: move |evt| remember.set(evt.checked()),
                }
                "Remember in the system keyring (falls back to this session only)"
            }
        }
    }
}

/// Add-Postgres panel: individual fields or a pasted URL, plus an optional
/// SSH tunnel.
#[component]
fn PostgresForm(on_done: EventHandler<()>) -> Element {
    use crate::tunnel::{TunnelAuth, TunnelConfig};
    let state = use_context::<AppState>();
    let mut use_url = use_signal(|| false);
    let mut name = use_signal(String::new);
    let mut host = use_signal(String::new);
    let mut port = use_signal(String::new);
    let mut database = use_signal(String::new);
    let mut user = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut remember = use_signal(|| true);
    let mut sslmode = use_signal(|| "prefer".to_string());
    let mut pasted_url = use_signal(String::new);
    let mut use_tunnel = use_signal(|| false);
    let mut ssh_host = use_signal(String::new);
    let mut ssh_port = use_signal(String::new);
    let mut ssh_user = use_signal(String::new);
    // false = ssh-agent (the default), true = key file.
    let mut ssh_use_key = use_signal(|| false);
    let mut ssh_key_path = use_signal(String::new);
    let mut ssh_passphrase = use_signal(String::new);
    let mut form_error = use_signal(|| Option::<String>::None);

    let mut submit = move || {
        // Tunnel settings are validated first so a bad SSH field fails
        // before any connect attempt.
        let tunnel: Option<TunnelConfig> = if *use_tunnel.peek() {
            let host = ssh_host.peek().trim().to_string();
            if host.is_empty() {
                form_error.set(Some("SSH host must not be empty".to_string()));
                return;
            }
            let port_text = ssh_port.peek().trim().to_string();
            let port = if port_text.is_empty() {
                22
            } else {
                match port_text.parse() {
                    Ok(port) => port,
                    Err(_) => {
                        form_error.set(Some(format!("invalid SSH port: {port_text}")));
                        return;
                    }
                }
            };
            let user = ssh_user.peek().trim().to_string();
            if user.is_empty() {
                form_error.set(Some("SSH user must not be empty".to_string()));
                return;
            }
            let auth = if *ssh_use_key.peek() {
                let path = ssh_key_path.peek().trim().to_string();
                if path.is_empty() {
                    form_error.set(Some("SSH key file path must not be empty".to_string()));
                    return;
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
            Some(TunnelConfig {
                host,
                port,
                user,
                auth,
            })
        } else {
            None
        };
        let entered_passphrase = if matches!(
            tunnel,
            Some(TunnelConfig {
                auth: TunnelAuth::KeyFile { .. },
                ..
            })
        ) {
            Some(ssh_passphrase.peek().clone()).filter(|p| !p.is_empty())
        } else {
            None
        };
        // A password pasted inside the URL is used for this connect (and
        // remembered for the session on success) but never persisted.
        let embedded_password = if *use_url.peek() {
            url::Url::parse(pasted_url.peek().trim())
                .ok()
                .and_then(|u| {
                    // Url::password() returns the still-encoded form.
                    u.password().map(|p| {
                        percent_encoding::percent_decode_str(p)
                            .decode_utf8_lossy()
                            .into_owned()
                    })
                })
        } else {
            None
        };
        let built = if *use_url.peek() {
            crate::db::sanitized_url(&pasted_url.peek())
        } else {
            crate::db::build_url(
                &host.peek(),
                &port.peek(),
                &database.peek(),
                &user.peek(),
                &sslmode.peek(),
            )
        };
        let url = match built {
            Ok(url) => url,
            Err(err) => {
                form_error.set(Some(err.to_string()));
                return;
            }
        };
        let display_name = {
            let entered = name.peek().trim().to_string();
            if entered.is_empty() {
                default_pg_name(&url)
            } else {
                entered
            }
        };
        let mut entered_password = password.peek().clone();
        if entered_password.is_empty() {
            if let Some(embedded) = embedded_password {
                entered_password = embedded;
            }
        }
        let remember_choice = *remember.peek();
        form_error.set(None);
        spawn(async move {
            // An entered passphrase seeds session memory so the tunnel open
            // finds it, exactly as if it came from the prompt.
            if let Some(passphrase) = &entered_passphrase {
                state.stash_ssh_passphrase(&url, passphrase.clone());
            }
            if entered_password.is_empty() {
                state
                    .connect_postgres(url.clone(), display_name.clone(), tunnel.clone())
                    .await;
            } else {
                state
                    .connect_postgres_with_password(
                        url.clone(),
                        display_name.clone(),
                        entered_password,
                        remember_choice,
                        tunnel.clone(),
                    )
                    .await;
            }
            // Only save and close the form when the connection worked.
            if state.open_locators.peek().iter().any(|(_, l)| *l == url) {
                if remember_choice && entered_passphrase.is_some() {
                    state.persist_ssh_passphrase(&url).await;
                }
                state.add_saved_postgres(display_name, url, tunnel);
                on_done.call(());
            }
        });
    };

    let field_class = "w-full rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-3 py-2 font-mono text-sm text-slate-900 dark:text-slate-200 placeholder:text-slate-400 dark:placeholder:text-slate-600";
    rsx! {
        div { class: "w-full max-w-xl rounded border border-slate-300 dark:border-slate-700 bg-slate-50 dark:bg-slate-950/80 p-4",
            div { class: "mb-3 flex items-center justify-between",
                span { class: "text-sm font-medium text-slate-900 dark:text-slate-200", "Add a Postgres connection" }
                button {
                    class: "text-xs text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                    onclick: move |_| {
                        let flip = !*use_url.read();
                        use_url.set(flip);
                    },
                    if use_url() { "Use individual fields" } else { "Paste a URL instead" }
                }
            }
            div { class: "flex flex-col gap-2",
                input {
                    class: field_class,
                    placeholder: "display name (optional)",
                    value: "{name}",
                    oninput: move |evt| name.set(evt.value()),
                }
                if use_url() {
                    input {
                        class: field_class,
                        placeholder: "postgres://user@host:5432/database?sslmode=require",
                        value: "{pasted_url}",
                        oninput: move |evt| pasted_url.set(evt.value()),
                    }
                } else {
                    div { class: "flex gap-2",
                        input {
                            class: "{field_class} flex-[3]",
                            placeholder: "host",
                            value: "{host}",
                            oninput: move |evt| host.set(evt.value()),
                        }
                        input {
                            class: "{field_class} flex-1",
                            placeholder: "5432",
                            value: "{port}",
                            oninput: move |evt| port.set(evt.value()),
                        }
                    }
                    div { class: "flex gap-2",
                        input {
                            class: field_class,
                            placeholder: "database",
                            value: "{database}",
                            oninput: move |evt| database.set(evt.value()),
                        }
                        input {
                            class: field_class,
                            placeholder: "user",
                            value: "{user}",
                            oninput: move |evt| user.set(evt.value()),
                        }
                    }
                    div { class: "flex gap-2",
                        select {
                            class: "rounded border border-slate-300 dark:border-slate-700 bg-slate-100 dark:bg-slate-950 px-2 py-2 text-sm text-slate-900 dark:text-slate-300",
                            onchange: move |evt| sslmode.set(evt.value()),
                            option { value: "prefer", selected: *sslmode.read() == "prefer", "sslmode: prefer" }
                            option { value: "require", selected: *sslmode.read() == "require", "sslmode: require" }
                            option { value: "disable", selected: *sslmode.read() == "disable", "sslmode: disable" }
                        }
                    }
                }
                input {
                    r#type: "password",
                    class: field_class,
                    placeholder: "password",
                    value: "{password}",
                    oninput: move |evt| password.set(evt.value()),
                }
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
                            input {
                                class: "{field_class} flex-[3]",
                                placeholder: "ssh host",
                                value: "{ssh_host}",
                                oninput: move |evt| ssh_host.set(evt.value()),
                            }
                            input {
                                class: "{field_class} flex-1",
                                placeholder: "22",
                                value: "{ssh_port}",
                                oninput: move |evt| ssh_port.set(evt.value()),
                            }
                        }
                        input {
                            class: field_class,
                            placeholder: "ssh user",
                            value: "{ssh_user}",
                            oninput: move |evt| ssh_user.set(evt.value()),
                        }
                        div { class: "flex gap-4 text-xs text-slate-500 dark:text-slate-400",
                            label { class: "flex items-center gap-2",
                                input {
                                    r#type: "radio",
                                    name: "ssh-auth",
                                    checked: !ssh_use_key(),
                                    onchange: move |_| ssh_use_key.set(false),
                                }
                                "ssh-agent"
                            }
                            label { class: "flex items-center gap-2",
                                input {
                                    r#type: "radio",
                                    name: "ssh-auth",
                                    checked: ssh_use_key(),
                                    onchange: move |_| ssh_use_key.set(true),
                                }
                                "key file"
                            }
                        }
                        if ssh_use_key() {
                            input {
                                class: field_class,
                                placeholder: "key file path, e.g. ~/.ssh/id_ed25519",
                                value: "{ssh_key_path}",
                                oninput: move |evt| ssh_key_path.set(evt.value()),
                            }
                            input {
                                r#type: "password",
                                class: field_class,
                                placeholder: "key passphrase (if the key is encrypted)",
                                value: "{ssh_passphrase}",
                                oninput: move |evt| ssh_passphrase.set(evt.value()),
                            }
                        }
                    }
                }
                label { class: "flex items-center gap-2 text-xs text-slate-500 dark:text-slate-400",
                    input {
                        r#type: "checkbox",
                        checked: remember(),
                        onchange: move |evt| remember.set(evt.checked()),
                    }
                    "Remember in the system keyring (falls back to this session only)"
                }
                div { class: "flex justify-end gap-2",
                    button {
                        class: "rounded px-3 py-2 text-sm text-slate-500 dark:text-slate-400 hover:text-slate-900 dark:hover:text-slate-100",
                        onclick: move |_| on_done.call(()),
                        "Cancel"
                    }
                    button {
                        class: "rounded bg-cyan-700 px-4 py-2 text-sm font-medium text-white hover:bg-cyan-600",
                        onclick: move |_| submit(),
                        "Connect & save"
                    }
                }
                if let Some(err) = form_error() {
                    p { class: "text-sm text-red-600 dark:text-red-400", "{err}" }
                }
            }
        }
    }
}

/// Fallback display name: "database @ host".
fn default_pg_name(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            let db = parsed.path().trim_start_matches('/');
            let host = parsed.host_str().unwrap_or("?");
            if db.is_empty() {
                host.to_string()
            } else {
                format!("{db} @ {host}")
            }
        }
        Err(_) => url.to_string(),
    }
}
