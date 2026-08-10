# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`hubro` is a **desktop-only database viewer** (SQLite and Postgres via sqlx, SQL Server via tiberius) built with Dioxus 0.7 (Rust), shipping on Linux, macOS, and Windows. Do not add web or mobile platform support. Work is tracked in the Linear project "hubro" (team FRE).

Licensed **GPL-3.0-only** (FRE-83), with copyright held solely by Fredrik Lokander — which is what keeps commercial dual-licensing possible. Two consequences: don't add dependencies under licenses GPL-3.0 can't incorporate (AGPL, or proprietary/source-available terms; permissive and MPL are fine), and don't merge outside contributions without a CLA or copyright assignment, since divided copyright permanently removes the ability to relicense.

## Commands

- `dx serve` — run the desktop app with hot reload.
- `dx build` — build the app via the Dioxus CLI.
- `cargo check` / `cargo clippy` — type-check and lint without the Dioxus CLI.
- `cargo fmt --check` — CI enforces rustfmt (the Actions job runs fmt, clippy, and test); run it before every push.
- Tailwind is compiled automatically by `dx serve` (Dioxus 0.7+): it picks up `tailwind.css` next to Cargo.toml and outputs to `assets/tailwind.css`. No npm/Tailwind CLI setup is needed.

- `cargo test` — run all tests: unit tests live in `#[cfg(test)]` modules next to the code, integration tests in `tests/`. Test fixture files go in `tests/fixtures/`. Run a single test with `cargo test <name>`.
- Don't run `cargo test` and `dx build`/`cargo build` concurrently — they contend on the `target/` lock (spurious signal/exit 144); run them sequentially.
- Postgres integration tests skip unless `HUBRO_PG_TEST_URL` is set; SQL Server tests skip unless `HUBRO_MSSQL_TEST_URL` is set; TimescaleDB tests skip unless `HUBRO_TIMESCALE_TEST_URL` is set; Citus tests skip unless `HUBRO_CITUS_TEST_URL` is set; CockroachDB tests skip unless `HUBRO_CRDB_TEST_URL` is set; YugabyteDB tests skip unless `HUBRO_YUGABYTE_TEST_URL` is set; SSH-tunnel tests need `HUBRO_SSH_TEST` (+ `HUBRO_SSH_TEST_KEY`/`_ENC_KEY`). Point them at the Docker `hubro-pg-test` (host port 5433) / `hubro-mssql-test` (14333) / `hubro-timescale-test` (5434) / `hubro-citus-test` (5435) / `hubro-crdb-test` (26257) / `hubro-yugabyte-test` (5436) / `hubro-ssh-test` (2222) containers; the exact `docker run` commands live in the test-file headers (`tests/db_postgres.rs`, `tests/db_sqlserver.rs`, `tests/db_timescale.rs`, `tests/db_citus.rs`, `tests/db_cockroach.rs`, `tests/db_yugabyte.rs`, `tests/tunnel.rs`). The Citus URL needs `?sslmode=disable` — that image ships an X.509 v1 certificate rustls won't parse (FRE-89) — and so does the CockroachDB one, which runs `--insecure` and serves no TLS at all. Pointing the shared suite at YugabyteDB needs `-- --test-threads=1`: that engine refuses concurrent DDL, so parallel tests fail on each other's fixtures rather than on anything real (FRE-91).
- Engine-verification issues (FRE-88 onwards) each add one `tests/db_<engine>.rs` behind its own `HUBRO_<ENGINE>_TEST_URL`, covering only what that engine does differently — the shared surface is verified by pointing the existing suite at the same container. Each file's header records that engine's findings (what needed fixing, what is absent, what is a known gap); FRE-96 assembles the published support matrix from them.
- Postgres-wire engines that aren't PostgreSQL are identified once at connect by `PgFlavor` (`src/db/postgres.rs`, FRE-90), read from the `version()` call that doubles as the liveness check. Keep any flavor branching inside the backend so a new engine is handled in one place; `DbPool::pg_flavor()` exists to report the answer, not for callers to branch on. **Prefer a catalog fact over the flavor whenever one exists** — CockroachDB's reserved catalog schemas are found via `table_type = 'SYSTEM VIEW'`, not via its name, which keeps the FRE-88 rule intact and needs no engine check at all. Likewise, anything varying by *version* within one engine belongs in a catalog query, which reports what the server has rather than what its version implies.

The crate is split into a library (`src/lib.rs`, holds app modules) and a thin binary (`src/main.rs`) so integration tests can import app code as `hubro::...`.

When a live database server (e.g. Postgres) is needed for testing or development, run it in Docker — never install or run database servers directly on the host.

Git pushes use SSH; the key is passphrase-protected. If pushes fail with "Permission denied (publickey)", ask the user to run `ssh-add`, or temporarily switch the remote to HTTPS with the authenticated `gh` CLI (`gh auth setup-git`).

## Interactive testing (screenshots + clicking)

Development happens on multiple machines; pick the recipe for the current platform.

### Linux (KDE Plasma on Wayland)

To drive the app (click, type, screenshot) without touching the user's real cursor, run it inside a nested Xephyr X server:

```bash
Xephyr :2 -screen 900x700 &
DISPLAY=:2 GDK_BACKEND=x11 ./target/dx/hubro/debug/linux/app/hubro &   # binary path from `dx build`
DISPLAY=:2 xdotool search --name "Hubro" windowmove 50 50                  # window is named "Hubro"; may spawn offscreen
DISPLAY=:2 xdotool mousemove X Y click 1                                     # full pointer control
import -display :2 -window root shot.png                                     # screenshot the nested display
```

Driving the app directly on the desktop half-works and isn't worth it: `spectacle -b -n -a -o shot.png` captures windows and `xdotool key` reaches XWayland windows (after a one-time KDE "Remote Control" approval), but KWin ignores XTEST pointer events, so mouse control is impossible outside Xephyr.

Gotchas: native `<select>` dropdowns are driven by click → `key Down/Up` → `key Return` (options aren't clickable elements). Xephyr runs no window manager, so nothing delivers `WindowEvent::CloseRequested` — send a synthetic `WM_DELETE_WINDOW` ClientMessage (a ~30-line Xlib/`gcc -lX11` helper) to test the window-close guard.

### macOS

The app is a native Cocoa bundle — there is no display server to nest, so **synthetic input drives the real cursor** (no Xephyr-style isolation exists). Keep interactions short: screenshot → verify → act, and save/restore the pointer around clicks. Tools: `brew install cliclick smokris/getwindowid/getwindowid`. One-time grants for the terminal app in System Settings → Privacy & Security: **Accessibility** (cliclick/System Events) and **Screen Recording** (screencapture).

```bash
dx build    # bundle: target/dx/hubro/debug/macos/Hubro.app
open target/dx/hubro/debug/macos/Hubro.app    # must go via LaunchServices — see the blank-webview gotcha; quit with pkill -x Hubro
GetWindowID Hubro --list     # titles list as "(null)" — pick the id with the main window's size
screencapture -x -l <id> shot.png                                         # crisp per-window capture, works unfocused
osascript -e 'tell app "System Events" to tell (first process whose unix id is '$(pgrep -x Hubro)') to get {position, size} of window 1'
POS=$(cliclick p | tr -d ' '); cliclick c:X,Y; cliclick "m:$POS"          # click, then restore the cursor
```

Gotchas: on current macOS the webview stays **blank when the binary is exec'd directly** from a terminal (the window opens but WKWebView never paints) — launch through LaunchServices instead (`open path/to/Hubro.app`, then `pkill -x hubro` to quit; note the release bundle's process name is lowercase `hubro`, the debug bundle's is `Hubro`). Click targets are **window position + logical (point) coordinates** from the osascript line — don't derive them from screenshot pixels, which are 2x Retina and include shadow margins. The first click on an unfocused window only focuses it (the webview doesn't accept click-through) — click twice or activate the app first. Keystrokes go via System Events (`keystroke`/`key code`) to the focused window. The window-close guard is testable directly: macOS has a real window manager, so the red button or Cmd+W delivers `CloseRequested` — no synthetic-event helper needed.

### Windows

Everything goes through **posted window messages** (`PostMessage` to the WebView2 child) — no real cursor movement, no focus stealing, and no permission grants needed. Dot-source the checked-in helper `scripts/winauto.ps1` (PowerShell 7) for all of it:

```powershell
dx build    # exe: target\dx\hubro\debug\windows\app\hubro.exe
Start-Process .\target\dx\hubro\debug\windows\app\hubro.exe   # window title "Hubro"
. .\scripts\winauto.ps1
$h = Find-AppWindow                 # top-level HWND (throws if the app isn't running)
Save-WindowShot $h shot.png         # crisp PrintWindow capture, works while occluded; 1:1 with click coords
Send-PostedClick $h X Y             # coords relative to the window rect = the screenshot's pixel coords
Send-PostedText  $h "localhost"     # WM_CHAR into the focused element — click a field first
Send-PostedKey   $h 0x28            # virtual keys: 0x0D Enter, 0x1B Esc, 0x26/0x28 Up/Down, 0x09 Tab
Send-PostedWheel $h X Y -3          # scroll down 3 notches at that point
Send-Close       $h                 # WM_CLOSE → delivers CloseRequested (tests the close guard)
```

Build prerequisites beyond rustup + VS Build Tools: **NASM** (`aws-lc-sys` needs it; GitHub's windows-latest runners have it preinstalled, dev machines don't — drop `nasm.exe` from nasm.us into `~\.cargo\bin`) and the WebView2 runtime (preinstalled on Win11). Install dx from the prebuilt `dx-x86_64-pc-windows-msvc.zip` GitHub release asset rather than `cargo install`.

Gotchas: native `<select>` dropdowns work like on Linux — posted click, then `Send-PostedKey` Down/Up + Enter (the popup is a separate OS window that won't appear in captures; drive it blind by keyboard). **Re-screenshot before deriving coordinates** — form layouts shift as content changes and a click 5px off a field silently does nothing. Screenshots include the title bar and native menu (webview content starts ~56px down). Coordinates are 1:1 only at 100% display scaling (the helper calls `SetProcessDPIAware`; both dev monitors are at 100%). Posted input is verified with the app foreground (its normal state after launch); it never disturbs other windows either way. In the helper, Win32 `FindWindowEx` needs `[NullString]::Value` — a PowerShell `$null` string marshals as `""` and matches nothing.

## Issue workflow

Work is tracked in Linear (team FRE). Docs-only changes (CLAUDE.md, README, etc.) may be committed directly to `main`; all other work follows this flow:

1. Move the Linear issue to In Progress. Create a **git worktree** on the issue's branch (use Linear's suggested branch name, e.g. `lokander/fre-5-set-up-async-database-layer`), and do all work there.
2. Commit (conventional, subject-only), push the branch, and open a **GitHub PR** with `gh pr create`. Reference the issue ID (e.g. FRE-5) in the PR so Linear links it.
3. Spawn a **subagent to review the PR** (correctness, the Dioxus 0.7 rules above, scope vs the issue). `gh pr review --approve` is blocked as self-approval — post findings with `gh pr comment`. Fix blocking findings and re-review by resuming the *same* review subagent via SendMessage (keeps its context). Only proceed once it approves.
4. **Wait for CI to pass** (`gh pr checks <n> --watch`) before merging — local clippy/test runs don't cover everything CI checks (e.g. rustfmt). Then **remove the worktree first**, and rebase-merge (`gh pr merge --rebase --delete-branch`) from the main checkout — merging from inside the worktree fails trying to check out `main`. Run the merge as its own step (chaining a `gh pr comment` + merge in one command trips the self-merge classifier). Move the issue to Done with the PR linked.

### Milestones

Milestones are **scoped deliverables that complete**, not categories. Three rules:

- **Never add an issue to a finished milestone.** Linear computes progress from the issues, so it would drop a 100% milestone below 100% permanently — rewriting the record of what shipped when. Linear has no way to archive or un-complete a milestone, so this is not undoable.
- **Don't create catch-all milestones.** A milestone with no completion condition ("Misc polish", "Follow-ups") sits at partial progress forever. Loose work gets **no milestone** plus a `Feature`/`Bug`/`Improvement` label and lives in the project backlog. `Nice to have (future)` is the one deliberate exception — designed-but-deferred work, not a general backlog.
- **A long list of completed milestones is fine.** They're the project's changelog and the reason leftover work is scopeable later.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) with **subject line only** — no body, no footers (including no Co-Authored-By trailers). Example: `feat: add csv import`.

## Architecture

- All components currently live in `src/main.rs`. `main()` calls `dioxus::launch(App)`; `App` is the root component.
- Static files live in `assets/` and are referenced via the `asset!("/assets/...")` macro (paths are relative to the project root). Stylesheets/favicons are injected with `document::Link` in `App`.
- `Dioxus.toml` holds Dioxus CLI app configuration (currently just the empty `[application]` section).
- New fields on `SavedConnection`/`Settings` use `#[serde(default, skip_serializing_if = ...)]` so older config files still deserialize and unaffected entries serialize unchanged.
- Objects that are the database's own bookkeeping (extension schemas and tables, child partitions) are declared per backend as `TableMeta::internal` during introspection — never inferred from name patterns (FRE-88). The sidebar hides them behind one toggle and the SQL editor demotes them in completion ranking, so every new backend inherits both by filling in that one field. `TableMeta::kind_label` is the matching hook for engine-specific vocabulary (`hypertable`, `continuous aggregate`), rendered as a badge that refines `TableKind` rather than replacing it.
- The app was renamed from `dataview` (FRE-64) before it had any users, so there is deliberately no migration code. The name is load-bearing in `$XDG_CONFIG_HOME/hubro/` (connections, settings, session, SSH known_hosts), `$XDG_DATA_HOME/hubro/history.db`, the `hubro` keyring service, and the `no.lokander.hubro` bundle id — changing it again would strand all four.

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
