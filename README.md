# dataview

A desktop app built with [Dioxus 0.7](https://dioxuslabs.com/learn/0.7) (Rust).

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
├─ src/lib.rs    # library root (db + ui modules), imported by tests as dataview::
├─ tailwind.css  # Tailwind input (compiled by dx)
```

## Releases

Prebuilt Linux packages (`.deb` and `.AppImage`) are attached to each
[GitHub release](https://github.com/lokander/dataview/releases). The AppImage
is self-contained and runs on most distributions; the `.deb` targets
Debian/Ubuntu and depends on `libwebkit2gtk-4.1-0` and `libgtk-3-0`.

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
