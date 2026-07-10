# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`dataview` is a Dioxus 0.7 **desktop-only** app (Rust). Do not add web or mobile platform support.

## Commands

- `dx serve` — run the desktop app with hot reload.
- `dx build` — build the app via the Dioxus CLI.
- `cargo check` / `cargo clippy` — type-check and lint without the Dioxus CLI.
- Tailwind is compiled automatically by `dx serve` (Dioxus 0.7+): it picks up `tailwind.css` next to Cargo.toml and outputs to `assets/tailwind.css`. No npm/Tailwind CLI setup is needed.

There are no tests yet; if added, run with `cargo test`.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) with **subject line only** — no body, no footers (including no Co-Authored-By trailers). Example: `feat: add csv import`.

## Architecture

- All components currently live in `src/main.rs`. `main()` calls `dioxus::launch(App)`; `App` is the root component.
- Static files live in `assets/` and are referenced via the `asset!("/assets/...")` macro (paths are relative to the project root). Stylesheets/favicons are injected with `document::Link` in `App`.
- `Dioxus.toml` holds Dioxus CLI app configuration (currently just the empty `[application]` section).

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

**Never hold a signal read/write borrow across an `await` point** — pending borrows make later reads/writes fail. `clippy.toml` enforces this via `await-holding-invalid-types` for `GenerationalRef(Mut)` and `dioxus_signals::WriteLock`; always run clippy from the project root so this config applies.

### Routing (if added later)

Routes are a single `enum` deriving `Routable`, with variants annotated `#[route("/path")]` (dynamic segments: `/blog/:id` → enum fields). Render with `Router::<Route> {}`; use `#[layout(NavBar)]` plus an `Outlet::<Route> {}` inside the layout component for shared chrome. Requires the `router` cargo feature on the `dioxus` dependency.
