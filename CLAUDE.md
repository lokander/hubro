# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`dataview` is a **desktop-only database viewer** (SQLite and Postgres, via sqlx) built with Dioxus 0.7 (Rust). Do not add web or mobile platform support. Work is tracked in the Linear project "dataview" (team FRE).

## Commands

- `dx serve` — run the desktop app with hot reload.
- `dx build` — build the app via the Dioxus CLI.
- `cargo check` / `cargo clippy` — type-check and lint without the Dioxus CLI.
- Tailwind is compiled automatically by `dx serve` (Dioxus 0.7+): it picks up `tailwind.css` next to Cargo.toml and outputs to `assets/tailwind.css`. No npm/Tailwind CLI setup is needed.

- `cargo test` — run all tests: unit tests live in `#[cfg(test)]` modules next to the code, integration tests in `tests/`. Test fixture files go in `tests/fixtures/`. Run a single test with `cargo test <name>`.
- Don't run `cargo test` and `dx build`/`cargo build` concurrently — they contend on the `target/` lock (spurious signal/exit 144); run them sequentially.
- Postgres integration tests skip unless `DATAVIEW_PG_TEST_URL` is set; SSH-tunnel tests need `DATAVIEW_SSH_TEST` (+ `DATAVIEW_SSH_TEST_KEY`/`_ENC_KEY`). Point them at the Docker `dataview-pg-test` (host port 5433) / `dataview-ssh-test` (2222) containers.

The crate is split into a library (`src/lib.rs`, holds app modules) and a thin binary (`src/main.rs`) so integration tests can import app code as `dataview::...`.

When a live database server (e.g. Postgres) is needed for testing or development, run it in Docker — never install or run database servers directly on the host.

Git pushes use SSH; the key is passphrase-protected. If pushes fail with "Permission denied (publickey)", ask the user to run `ssh-add`, or temporarily switch the remote to HTTPS with the authenticated `gh` CLI (`gh auth setup-git`).

## Interactive testing (screenshots + clicking)

The dev machine runs KDE Plasma on Wayland. To drive the app (click, type, screenshot) without touching the user's real cursor, run it inside a nested Xephyr X server:

```bash
Xephyr :2 -screen 900x700 &
DISPLAY=:2 GDK_BACKEND=x11 ./target/dx/dataview/debug/linux/app/dataview &   # binary path from `dx build`
DISPLAY=:2 xdotool search --name "dataview" windowmove 50 50                  # window is named "dataview"; may spawn offscreen
DISPLAY=:2 xdotool mousemove X Y click 1                                     # full pointer control
import -display :2 -window root shot.png                                     # screenshot the nested display
```

Driving the app directly on the desktop half-works and isn't worth it: `spectacle -b -n -a -o shot.png` captures windows and `xdotool key` reaches XWayland windows (after a one-time KDE "Remote Control" approval), but KWin ignores XTEST pointer events, so mouse control is impossible outside Xephyr.

Gotchas: native `<select>` dropdowns are driven by click → `key Down/Up` → `key Return` (options aren't clickable elements). Xephyr runs no window manager, so nothing delivers `WindowEvent::CloseRequested` — send a synthetic `WM_DELETE_WINDOW` ClientMessage (a ~30-line Xlib/`gcc -lX11` helper) to test the window-close guard.

## Issue workflow

Work is tracked in Linear (team FRE). Docs-only changes (CLAUDE.md, README, etc.) may be committed directly to `main`; all other work follows this flow:

1. Move the Linear issue to In Progress. Create a **git worktree** on the issue's branch (use Linear's suggested branch name, e.g. `lokander/fre-5-set-up-async-database-layer`), and do all work there.
2. Commit (conventional, subject-only), push the branch, and open a **GitHub PR** with `gh pr create`. Reference the issue ID (e.g. FRE-5) in the PR so Linear links it.
3. Spawn a **subagent to review the PR** (correctness, the Dioxus 0.7 rules above, scope vs the issue). `gh pr review --approve` is blocked as self-approval — post findings with `gh pr comment`. Fix blocking findings and re-review by resuming the *same* review subagent via SendMessage (keeps its context). Only proceed once it approves.
4. **Remove the worktree first**, then rebase-merge (`gh pr merge --rebase --delete-branch`) from the main checkout — merging from inside the worktree fails trying to check out `main`. Run the merge as its own step (chaining a `gh pr comment` + merge in one command trips the self-merge classifier). Move the issue to Done with the PR linked.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) with **subject line only** — no body, no footers (including no Co-Authored-By trailers). Example: `feat: add csv import`.

## Architecture

- All components currently live in `src/main.rs`. `main()` calls `dioxus::launch(App)`; `App` is the root component.
- Static files live in `assets/` and are referenced via the `asset!("/assets/...")` macro (paths are relative to the project root). Stylesheets/favicons are injected with `document::Link` in `App`.
- `Dioxus.toml` holds Dioxus CLI app configuration (currently just the empty `[application]` section).
- New fields on `SavedConnection`/`Settings` use `#[serde(default, skip_serializing_if = ...)]` so older config files still deserialize and unaffected entries serialize unchanged.

## Dioxus 0.7 — critical API notes

This repo uses Dioxus 0.7, which changed every API. **`cx`, `Scope`, and `use_state` no longer exist** — do not use pre-0.7 patterns from training data. Reference docs: https://dioxuslabs.com/learn/0.7 — or query Context7 (library ID `/dioxuslabs/dioxus`, pick the latest 0.7.x version) when the MCP server is available.

### Components and props

Components are `#[component]` functions returning `Element` (function name must start with a capital letter or contain an underscore). A component re-renders only when its props change (by `PartialEq`) or a reactive state it reads is updated.

- Props must be owned values (`String`, `Vec<T>`, not `&str`/`&[T]`) implementing `PartialEq + Clone`.
- Wrap a prop type in `ReadOnlySignal<T>` to make it reactive and `Copy` — memos/resources reading it re-run when the prop changes.

### RSX syntax

```rust
rsx! {
    div {
        class: "container",              // attribute
        color: "red",                    // inline style
        width: if condition { "100%" },  // conditional attribute
        "Hello!"
    }
    for i in 0..5 {          // prefer loops over iterator chains
        div { "{i}" }
    }
    if condition {
        div { "shown conditionally" }
    }
    {children}               // expressions are wrapped in braces
}
```

### State

State uses signals — a signal tracks where it's read and written, and rerenders/reruns dependents on change:

- `use_signal(|| initial)` — local state. Call `my_signal()` to clone the value, `.read()` for a reference, `.write()` for a mutable reference, `.with_mut(|v| ...)` to mutate in place.
- `use_memo(move || ...)` — memoized derived value, recalculates when signals it reads change.
- `use_resource(move || async move { ... })` — async state; re-runs when signals read in the closure change. Reading it yields `None` while loading, `Some(value)` when loaded.
- Context: parent calls `use_context_provider(|| state)`, children read with `use_context::<T>()` (matched by type).
- A signal read/written from a root `spawn_forever` task must be `Signal::new_in_scope(.., ScopeId::ROOT)`; a component-scoped one trips a `__copy_value_hoisted` runtime warning and can fail after that scope drops.

**Never hold a signal read/write borrow across an `await` point** — pending borrows make later reads/writes fail. `clippy.toml` enforces this via `await-holding-invalid-types` for `GenerationalRef(Mut)` and `dioxus_signals::WriteLock`; always run clippy from the project root so this config applies.

### Routing (if added later)

Routes are a single `enum` deriving `Routable`, with variants annotated `#[route("/path")]` (dynamic segments: `/blog/:id` → enum fields). Render with `Router::<Route> {}`; use `#[layout(NavBar)]` plus an `Outlet::<Route> {}` inside the layout component for shared chrome. Requires the `router` cargo feature on the `dioxus` dependency.
