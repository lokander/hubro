# Hubro

A desktop **database viewer for SQLite, Postgres, and SQL Server**, built with
[Dioxus 0.7](https://dioxuslabs.com/learn/0.7) (Rust), for Linux, macOS, and
Windows (credentials go to the OS keyring — Secret Service on Linux, Keychain
on macOS, Credential Manager on Windows; releases ship as `.deb`/AppImage,
`.dmg`, and `.msi`/setup `.exe`).

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

Per-OS build prerequisites, on top of a stable Rust toolchain:

- **Linux** — the WebKitGTK/GTK development packages CI installs (see
  [`ci.yml`](.github/workflows/ci.yml)): `libwebkit2gtk-4.1-dev`,
  `libgtk-3-dev`, `libxdo-dev`, `libayatana-appindicator3-dev`,
  `librsvg2-dev`, `libssl-dev`, `pkg-config`.
- **macOS** — nothing extra (WKWebView ships with the OS; Xcode command-line
  tools cover the linker).
- **Windows** — Visual Studio Build Tools (MSVC), plus
  [NASM](https://www.nasm.us/) on PATH (`aws-lc-sys` assembles with it; the
  GitHub runners have it preinstalled but dev machines usually don't). The
  WebView2 runtime is preinstalled on Windows 11 / updated Windows 10.

Install the Dioxus CLI if you don't have it (or grab a prebuilt `dx` from the
[Dioxus releases](https://github.com/DioxusLabs/dioxus/releases) — on Windows
that skips a long compile):

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
├─ src/lib.rs    # library root, imported by tests as hubro::
├─ src/db/       # backend-neutral DB layer (sqlite, postgres, schema, paging)
├─ src/ui/       # Dioxus components (shell, sidebar, grid, editor, state)
├─ src/azure.rs  # Entra ID token acquisition; tunnel.rs, secrets.rs, config.rs
├─ tailwind.css  # Tailwind input (compiled by dx)
```

## Releases

Prebuilt Linux packages (`.deb` and `.AppImage`), a macOS disk image
(`.dmg`, Apple Silicon), and Windows installers (`.msi` and setup `.exe`,
x64) are attached to each
[GitHub release](https://github.com/lokander/hubro/releases). The AppImage
is self-contained and runs on most distributions; the `.deb` targets
Debian/Ubuntu and depends on `libwebkit2gtk-4.1-0` and `libgtk-3-0`.

The macOS build is **unsigned** (no Apple Developer ID yet), so Gatekeeper
blocks the first launch. Clear the quarantine flag and it starts normally:

```bash
xattr -dr com.apple.quarantine /Applications/Hubro.app
```

Alternatively, after the blocked first launch, approve the app under System
Settings → Privacy & Security → **Open Anyway** (on macOS 14 and earlier,
right-click → Open also works). Either way this is only needed once.

The Windows installers are likewise **unsigned** (no Authenticode
certificate yet), so SmartScreen shows an "unknown publisher" warning when
you run the downloaded installer — click **More info → Run anyway**. The app needs the WebView2
runtime: Windows 11 and updated Windows 10 machines already have it, and on
a machine without it the installer downloads it during setup (internet
access required).

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
