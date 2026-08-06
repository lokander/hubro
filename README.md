<p align="center">
  <img src="assets/icons/128x128.png" width="112" alt="">
</p>

<h1 align="center">Hubro</h1>

<p align="center">
  A fast desktop viewer for SQLite, Postgres, and SQL Server.
</p>

<p align="center">
  <a href="https://github.com/lokander/hubro/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/lokander/hubro?color=1f6feb"></a>
  <a href="https://github.com/lokander/hubro/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/lokander/hubro/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: GPL-3.0" src="https://img.shields.io/badge/license-GPL--3.0-blue"></a>
  <img alt="Linux, macOS, Windows" src="https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-555">
</p>

Open a SQLite file or connect to a Postgres or SQL Server database, browse the
schema, run SQL, edit rows, and export the results — in one small native app
that starts fast and stays responsive on big tables. Passwords go to your
operating system's keyring rather than a config file, and remote servers can be
reached over an SSH tunnel or with Microsoft Entra ID sign-in.

Hubro runs on Linux, macOS, and Windows. It is free software (GPL-3.0).

![Hubro browsing a SQLite database](docs/screenshot.png)

## Install

Download the package for your platform from the
[latest release](https://github.com/lokander/hubro/releases/latest):

| Platform | Download |
| --- | --- |
| Linux | `.AppImage` (self-contained, most distributions) or `.deb` (Debian/Ubuntu; needs `libwebkit2gtk-4.1-0` and `libgtk-3-0`) |
| macOS | `.dmg` (Apple Silicon) |
| Windows | `.msi` or setup `.exe` (x64) |

<details>
<summary>macOS and Windows show a warning on first launch — here's why, and how to get past it</summary>

The builds are **unsigned** — there is no Apple Developer ID or Authenticode
certificate behind them yet — so both systems flag them as coming from an
unidentified developer.

On macOS, Gatekeeper blocks the first launch. Either clear the quarantine flag:

```bash
xattr -dr com.apple.quarantine /Applications/Hubro.app
```

…or, after the blocked launch, approve the app under System Settings → Privacy
& Security → **Open Anyway** (on macOS 14 and earlier, right-click → Open also
works). Either way it's only needed once.

On Windows, SmartScreen shows an "unknown publisher" warning for the
downloaded installer — click **More info → Run anyway**. The app also needs the
WebView2 runtime: Windows 11 and updated Windows 10 machines already have it,
and the installer downloads it during setup otherwise (internet access
required).

</details>

## Features

- **Connect** to SQLite files and Postgres or SQL Server servers (connection
  form or URL), with the OS keyring remembering passwords — Secret Service on
  Linux, Keychain on macOS, Credential Manager on Windows.
- **Browse** schemas — tables, views, and Postgres materialized views in the
  sidebar; columns, indexes, and foreign keys in the schema pane.
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

Hubro is written in Rust with [Dioxus 0.7](https://dioxuslabs.com/learn/0.7),
rendering into the platform webview. Build prerequisites, on top of a stable
Rust toolchain:

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

Then run the app with hot reload, and the tests with cargo:

```bash
dx serve
cargo test
```

Tailwind is compiled automatically by `dx serve` from `tailwind.css` in the
project root — no npm or Tailwind CLI needed. The generated output lands in
`assets/tailwind.css`. Postgres, SQL Server, and SSH-tunnel integration tests
skip unless pointed at a server (see the headers of `tests/db_postgres.rs`,
`tests/db_sqlserver.rs`, and `tests/tunnel.rs` for the `docker run` commands).

### Project layout

```
├─ assets/       # static assets, referenced via the asset!() macro
├─ src/main.rs   # thin binary entry point
├─ src/lib.rs    # library root, imported by tests as hubro::
├─ src/db/       # backend-neutral DB layer (sqlite, postgres, schema, paging)
├─ src/ui/       # Dioxus components (shell, sidebar, grid, editor, state)
├─ src/azure.rs  # Entra ID token acquisition; tunnel.rs, secrets.rs, config.rs
├─ tailwind.css  # Tailwind input (compiled by dx)
```

Notes on the data grid's performance work live in
[`docs/PERFORMANCE.md`](docs/PERFORMANCE.md).

### Cutting a release

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

## License

Hubro is free software under the [GNU General Public License v3.0](LICENSE).
You may use, study, share and modify it; if you distribute a modified version,
you must release your changes under the same license.

Copyright © 2026 Fredrik Lokander. Holding the copyright outright means the
GPL binds redistributors, not the author — so if the terms don't suit your
situation, commercial licensing is available on request. It also means patches
can't be merged without a copyright assignment; bug reports and feature
requests, on the other hand, are very welcome.
