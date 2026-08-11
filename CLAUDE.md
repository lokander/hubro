# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

More specific guidance lives next to the code it governs, and loads when you work there:

- `src/db/CLAUDE.md` — engine flavors, introspection SQL, retrying, capabilities, object metadata.
- `src/ui/CLAUDE.md` — the Dioxus 0.7 API rules. **Read this before writing any component**; 0.7 changed every API and pre-0.7 patterns from training data will not compile.
- `tests/CLAUDE.md` — which engine needs which env var, the container layout, verifying a new engine, the support matrix.
- `docs/interactive-testing.md` — how to drive the app on Linux, macOS and Windows.

## Project

`hubro` is a **desktop-only database viewer** (SQLite and Postgres via sqlx, SQL Server via tiberius) built with Dioxus 0.7 (Rust), shipping on Linux, macOS, and Windows. Do not add web or mobile platform support. Work is tracked in the Linear project "hubro" (team FRE).

Licensed **GPL-3.0-only** (FRE-83), with copyright held solely by Fredrik Lokander — which is what keeps commercial dual-licensing possible. Two consequences: don't add dependencies under licenses GPL-3.0 can't incorporate (AGPL, or proprietary/source-available terms; permissive and MPL are fine), and don't merge outside contributions without a CLA or copyright assignment, since divided copyright permanently removes the ability to relicense.

## Commands

- `dx serve` — run the desktop app with hot reload.
- `dx build` — build the app via the Dioxus CLI.
- `cargo check` / `cargo clippy` — type-check and lint without the Dioxus CLI.
- `cargo fmt --check` — CI enforces rustfmt (the Actions job runs fmt, clippy, and test); run it before every push.
- **A PR's CI checks Linux only.** The macOS and Windows legs run on push to `main`, so `gh pr checks` going green says nothing about them — and a red `main` is discovered after the merge, by which point several PRs may have inherited it (FRE-160). When a change is platform-shaped, prove it before merging with `gh workflow run ci.yml --ref <branch>` and wait for all three legs. `ci.yml` lists what counts, above `strategy:` — the short version is `cfg` blocks, path *syntax* (separators, drive letters, UNC, `file://`), a test comparing a checked-in file byte for byte or matching one against a literal `\n`, and process/signal/keyring code. The trigger used to say only `cfg(windows)`/`cfg(unix)`, which was too narrow: FRE-160 was three Windows failures across two merged PRs, none of which had a `cfg` block.
- Tailwind is compiled automatically by `dx serve` (Dioxus 0.7+): it picks up `tailwind.css` next to Cargo.toml and outputs to `assets/tailwind.css`. No npm/Tailwind CLI setup is needed.
- `assets/tailwind.css` is generated but tracked, so every UI branch touches it: resolve a conflict by re-running `dx build`, never by hand — a plain `--theirs` silently dropped 41 lines of a sibling PR's classes. The scanner reads Rust **doc comments** too, so naming a CSS property in prose emits a dead utility into the file.

- `cargo test` — run all tests: unit tests live in `#[cfg(test)]` modules next to the code, integration tests in `tests/`. Test fixture files go in `tests/fixtures/`. Run a single test with `cargo test <name>`.
- Don't run `cargo test` and `dx build`/`cargo build` concurrently — they contend on the `target/` lock (spurious signal/exit 144); run them sequentially.
- Don't pipe `cargo test` into `head`/`grep`: SIGPIPE can kill the run mid-fixture and leave an engine database half-set-up, so the *next* run fails on leftover state rather than on code. Redirect to a file and grep that.
- **A green engine-test run can mean nothing ran**: every `tests/db_*.rs` skips *and passes* when its `HUBRO_*_TEST_URL` is unset, so a stopped container or a typo reads as success. Confirm the tests executed, and mutation-check load-bearing assertions (invert one — it must fail). That caught an assertion in an unreached branch, a boundary defended only by a doc comment, and a guard with an escape route. Mutate the **decision**, not just its placement: a source-reading test pins where a call sits and stays green when its polarity flips, which let a mutation store a secret the user had declined (FRE-161) — so extract a consequential policy as a free function over plain data, where a test can execute it. Run one control mutation you expect to *pass*, too; one that failed exposed an assertion that required dead code. When the control does pass, read it as a question rather than a reassurance — it says nothing pins the claim that code was making. FRE-122's control (dropping SQL Server's explicit `NULL`) passed the nine-engine sweep exactly as predicted, which is how a doc comment justifying the keyword by `ANSI_NULL_DFLT_OFF` came to be measured and found false. A comment that justifies code by *engine* behaviour is answerable in minutes against a running container; ask it rather than reasoning about it. Which variable each engine needs, and how to start them, is in `tests/CLAUDE.md`.
- **Two agents cannot share one container set.** Every suite uses fixed fixture names against one database, so concurrent runs drop each other's tables — measured, not theoretical: two `db_cockroach` runs against the same set fail 3-6 tests each, and against separate sets both pass. So parallel work on several issues needs `./scripts/test-db.sh up <N>` per agent (ports are `base + N*100`, container names get a `-N` suffix, set 0 is the layout the headers document), then `env $(./scripts/test-db.sh env <N>) cargo test`, and `rm <N>` to release it. Worktrees isolate the code but **not** the build: a fresh one has no `.cargo/config.toml` (untracked, via `.git/info/exclude`), so it builds into `./target` on the QLC system drive. Write one per worktree pointing at `/mnt/data/cargo-target/hubro-<N>`, and `cp -a --reflink=always` the warm dir into it — `/mnt/data` is btrfs, so the copy is free and the first build starts warm. Process names are shared too: every worktree builds a binary called `hubro`, so `pkill -x hubro` (or `pkill -x Xephyr`) kills the siblings' — kill by recorded PID.

The crate is split into a library (`src/lib.rs`, holds app modules) and a thin binary (`src/main.rs`) so integration tests can import app code as `hubro::...`.

When a live database server (e.g. Postgres) is needed for testing or development, run it in Docker — never install or run database servers directly on the host.

Git pushes use SSH; the key is passphrase-protected. If pushes fail with "Permission denied (publickey)", ask the user to run `ssh-add`, or temporarily switch the remote to HTTPS with the authenticated `gh` CLI (`gh auth setup-git`).

## Interactive testing

Nothing in the suite covers rendering, so a UI change wants the app driven: FRE-148's model tests all passed while the screen stated the reason twice and still offered Export on an unbrowsable object. Three sessions running, driving the app has caught bugs a green suite passed. The per-platform recipes, and the rule about isolating the app's config while doing it, are in **`docs/interactive-testing.md`**.

## Issue workflow

Work is tracked in Linear (team FRE). Docs-only changes (CLAUDE.md, README, etc.) may be committed directly to `main`; all other work follows this flow:

1. Move the Linear issue to In Progress. Create a **git worktree** on the issue's branch (use Linear's suggested branch name, e.g. `lokander/fre-5-set-up-async-database-layer`), and do all work there. **Reproduce before building the fix the issue proposes** — treat its diagnosis as a lead and its fix direction as a hypothesis. All three bugs in the 2026-08-11 batch were wrong about themselves: FRE-156's line numbers had drifted onto struct definitions, FRE-127's stated cause (cargo running test binaries in parallel) is false — cargo runs test *targets* sequentially — and FRE-161's suggested fix was built, driven against a real bastion, and observed to still fail.
2. Commit (conventional, subject-only), push the branch, and open a **GitHub PR** with `gh pr create`. Reference the issue ID (e.g. FRE-5) in the PR so Linear links it.
3. Spawn a **subagent to review the PR** (correctness, the Dioxus 0.7 rules in `src/ui/CLAUDE.md`, scope vs the issue). `gh pr review --approve` is blocked as self-approval — post findings with `gh pr comment`. Fix blocking findings and re-review by resuming the *same* review subagent via SendMessage (keeps its context). Only proceed once it approves. Expect findings to be *claims* rather than broken features — a doc comment, README cell or `# validates` comment asserting a property nothing checks. Prefer making such a claim checkable over restating it (`tests/support_matrix.rs` is the pattern); a guard that cannot fire is worse than none, because it reads as handled. Mutate the **fix** too, not just the code it fixes: a correct fix that no test pins is one commit from regressing, and one such fix survived reverting to the exact pre-fix code with 485 tests still green.
4. **Wait for CI to pass** (`gh pr checks <n> --watch`) before merging — local clippy/test runs don't cover everything CI checks (e.g. rustfmt). With several PRs in flight, rebase each onto `main` locally and build before merging: GitHub's `MERGEABLE` is textual, and a conflict-free merge still failed to compile when a sibling added a field to `SavedConnection`. Then **remove the worktree first**, and rebase-merge (`gh pr merge --rebase --delete-branch`) from the main checkout — merging from inside the worktree fails trying to check out `main`. Run the merge as its own step (chaining a `gh pr comment` + merge in one command trips the self-merge classifier). Move the issue to Done with the PR linked.

### Milestones

Milestones are **scoped deliverables that complete**, not categories. Three rules:

- **Never add an issue to a finished milestone.** Linear computes progress from the issues, so it would drop a 100% milestone below 100% permanently — rewriting the record of what shipped when. Linear has no way to archive or un-complete a milestone, so this is not undoable.
- **Don't create catch-all milestones.** A milestone with no completion condition ("Misc polish", "Follow-ups") sits at partial progress forever. Loose work gets **no milestone** plus a `Feature`/`Bug`/`Improvement` label and lives in the project backlog. `Nice to have (future)` is the one deliberate exception — designed-but-deferred work, not a general backlog.
- **A long list of completed milestones is fine.** They're the project's changelog and the reason leftover work is scopeable later.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) with **subject line only** — no body, no footers (including no Co-Authored-By trailers). Example: `feat: add csv import`.

## Architecture

- `src/main.rs` is a thin binary: it parses CLI arguments and calls `dioxus::launch`. Components live under `src/ui/`, the backends under `src/db/`, and config under `src/config/`; each of those has its own guidance file where one is warranted.
- The rest of the top level, none of which has a guidance file of its own: `src/cli.rs` (argument parsing and the file-association open target), `src/history.rs` (query history), `src/secrets.rs` (the keyring, with a session-only fallback behind `HUBRO_DISABLE_KEYRING=1`), `src/tunnel.rs` (SSH tunnels), `src/azure.rs` (Entra sign-in), `src/util.rs`.
- Static files live in `assets/` and are referenced via the `asset!("/assets/...")` macro (paths are relative to the project root). Stylesheets/favicons are injected with `document::Link` in `App`.
- `Dioxus.toml` holds Dioxus CLI app configuration: the bundle identifier, icons (order is load-bearing — read the comment), and the per-platform file associations from FRE-114. `packaging/README.md` explains why each platform's declaration looks different.
- New fields on `SavedConnection`/`Settings` use `#[serde(default, skip_serializing_if = ...)]` so older config files still deserialize and unaffected entries serialize unchanged.
- The app was renamed from `dataview` (FRE-64) before it had any users, so there is deliberately no migration code. The name is load-bearing in `$XDG_CONFIG_HOME/hubro/` (connections, settings, session, SSH known_hosts), `$XDG_DATA_HOME/hubro/history.db`, the `hubro` keyring service, and the `no.lokander.hubro` bundle id — changing it again would strand all four.
