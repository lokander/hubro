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
├─ src/main.rs   # entry point and components
├─ tailwind.css  # Tailwind input (compiled by dx)
```
