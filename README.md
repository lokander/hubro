# dataview

A desktop **database viewer for SQLite, Postgres, and SQL Server**, built with
[Dioxus 0.7](https://dioxuslabs.com/learn/0.7) (Rust), for Linux and macOS
(credentials go to the OS keyring — Secret Service on Linux, Keychain on
macOS; releases ship as `.deb`/AppImage and `.dmg`).

## Features

- **Connect** to SQLite files and Postgres or SQL Server servers (connection
  form or URL), with the OS keyring remembering passwords.
- **Browse** schemas — tables, views, Postgres materialized views, columns,
  indexes, and foreign keys — in an expandable sidebar.
- **Data grid** with sorting, filtering, and paging that stays fast on huge
  tables and large values (windowed rendering, bounded memory).
- **SQL editor** — run queries and multi-statement scripts (wrapped in a
  transaction), with schema-aware autocomplete, syntax highlighting, query
  history, cancellation, and a confirmation before writes.
- **Edit** rows inline — cell edits, inserts, and deletes staged and saved
  atomically, with primary-key/unique-index detection and confirmation before
  destructive operations (views and materialized views are read-only).
- **Export** query and table results to CSV or JSON (streamed).
- **Foreign-key navigation**, keyboard shortcuts (with a cheatsheet),
  dark/light theme, and window/session restore.
- **Secure remote access** — SSH tunnels (agent or key file) with host-key
  verification for Postgres and SQL Server, and Microsoft Entra ID sign-in for
  Azure Postgres and Azure SQL (interactive browser or managed identity).

## Development

Install the Dioxus CLI if you don't have it:

```bash
curl -sSL http://dioxus.dev/install.sh | sh
```

Then run the app with hot reload:

```bash
dx serve
```

Tailwind is compiled automatically by `dx serve` from `tailwind.css` in the project root — no npm or Tailwind CLI needed. The generated output lands in `assets/tailwind.css`.

## Project layout

```
├─ assets/       # static assets, referenced via the asset!() macro
├─ src/main.rs   # thin binary entry point
├─ src/lib.rs    # library root, imported by tests as dataview::
├─ src/db/       # backend-neutral DB layer (sqlite, postgres, schema, paging)
├─ src/ui/       # Dioxus components (shell, sidebar, grid, editor, state)
├─ src/azure.rs  # Entra ID token acquisition; tunnel.rs, secrets.rs, config.rs
├─ tailwind.css  # Tailwind input (compiled by dx)
```

## Releases

Prebuilt Linux packages (`.deb` and `.AppImage`) and a macOS disk image
(`.dmg`, Apple Silicon) are attached to each
[GitHub release](https://github.com/lokander/dataview/releases). The AppImage
is self-contained and runs on most distributions; the `.deb` targets
Debian/Ubuntu and depends on `libwebkit2gtk-4.1-0` and `libgtk-3-0`.

The macOS build is **unsigned** (no Apple Developer ID yet), so Gatekeeper
blocks the first launch. Clear the quarantine flag and it starts normally:

```bash
xattr -dr com.apple.quarantine /Applications/Dataview.app
```

Alternatively, after the blocked first launch, approve the app under System
Settings → Privacy & Security → **Open Anyway** (on macOS 14 and earlier,
right-click → Open also works). Either way this is only needed once.

Releases are cut by pushing a version tag. The
[`release` workflow](.github/workflows/release.yml) then bundles the app with
`dx bundle` and publishes the artifacts:

```bash
# bump the version in Cargo.toml first, then:
git tag v0.1.0
git push origin v0.1.0
```

Bundle metadata (identifier, category, description, icons, package
dependencies) lives in the `[bundle]` section of `Dioxus.toml`. To build a
package locally:

```bash
dx bundle --release --package-types deb --package-types appimage
```
