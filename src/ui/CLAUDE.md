# Dioxus 0.7 — critical API notes

Rules for `src/ui/`. The project-wide guidance is in the root `CLAUDE.md`.

This repo uses Dioxus 0.7, which changed every API. **`cx`, `Scope`, and `use_state` no longer exist** — do not use pre-0.7 patterns from training data. Reference docs: https://dioxuslabs.com/learn/0.7 — or query Context7 (library ID `/dioxuslabs/dioxus`, pick the latest 0.7.x version) when the MCP server is available.

## Components and props

Components are `#[component]` functions returning `Element` (function name must start with a capital letter or contain an underscore). A component re-renders only when its props change (by `PartialEq`) or a reactive state it reads is updated.

- Props must be owned values (`String`, `Vec<T>`, not `&str`/`&[T]`) implementing `PartialEq + Clone`.
- Wrap a prop type in `ReadOnlySignal<T>` to make it reactive and `Copy` — memos/resources reading it re-run when the prop changes.

## RSX syntax

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

## State

State uses signals — a signal tracks where it's read and written, and rerenders/reruns dependents on change:

- `use_signal(|| initial)` — local state. Call `my_signal()` to clone the value, `.read()` for a reference, `.write()` for a mutable reference, `.with_mut(|v| ...)` to mutate in place.
- `use_memo(move || ...)` — memoized derived value, recalculates when signals it reads change.
- `use_resource(move || async move { ... })` — async state; re-runs when signals read in the closure change. Reading it yields `None` while loading, `Some(value)` when loaded.
- Context: parent calls `use_context_provider(|| state)`, children read with `use_context::<T>()` (matched by type).
- `use_memo` has a PartialEq write gate; `Signal::set` is unconditional and dirties every subscriber. Replacing a memo with `use_signal` + `use_effect` + `set` silently loses that gate — in the grid that re-ran the page fetch and `COUNT(*)` on any unrelated connection's open/close (FRE-129/FRE-148). Keep the memo, and `peek` before writing if you also need to retain a previous value across a reload.
- A signal read/written from a root `spawn_forever` task must be `Signal::new_in_scope(.., ScopeId::ROOT)`; a component-scoped one trips a `__copy_value_hoisted` runtime warning and can fail after that scope drops.

**Never hold a signal read/write borrow across an `await` point** — pending borrows make later reads/writes fail. `clippy.toml` enforces this via `await-holding-invalid-types` for `GenerationalRef(Mut)` and `dioxus_signals::WriteLock`; always run clippy from the project root so this config applies.

## Routing (if added later)

Routes are a single `enum` deriving `Routable`, with variants annotated `#[route("/path")]` (dynamic segments: `/blog/:id` → enum fields). Render with `Router::<Route> {}`; use `#[layout(NavBar)]` plus an `Outlet::<Route> {}` inside the layout component for shared chrome. Requires the `router` cargo feature on the `dioxus` dependency.
