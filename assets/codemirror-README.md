# codemirror.js

`codemirror.js` is a prebuilt, committed bundle (CodeMirror 6 + SQL language
+ One Dark theme) so the app works offline with no JS toolchain in the build.

Source: `codemirror-entry.js`. To regenerate:

```sh
npm install codemirror @codemirror/view @codemirror/state \
  @codemirror/commands @codemirror/lang-sql @codemirror/autocomplete \
  @codemirror/theme-one-dark esbuild
npx esbuild codemirror-entry.js --bundle --format=iife \
  --global-name=DVEditor --minify --outfile=codemirror.js
```

The bundle exposes:

- `DVEditor.create(id, parentElementId, dialect, initialDoc, schema)` —
  `schema` is a lang-sql `SQLNamespace` object mapping `"table"` (SQLite) or
  `"schema.table"` (Postgres; literal dots in names escaped as `\.`) to
  arrays of column names. Postgres views get `defaultSchema: "public"`.
- `DVEditor.updateSchema(id, dialect, schema)` — swaps the completion data
  in place (the sql() extension sits in a Compartment), used after schema
  reloads.
- `DVEditor.setDoc(id, text)` and `DVEditor.destroy(id)`.

It calls `window.__dvRun` / `window.__dvDoc` (installed by src/ui/editor.rs
through the dioxus eval channel).
