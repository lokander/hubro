# codemirror.js

`codemirror.js` is a prebuilt, committed bundle (CodeMirror 6 + SQL language
+ One Dark theme) so the app works offline with no JS toolchain in the build.

Source: `codemirror-entry.js`. **Editing that file changes nothing until you
rebuild** — the bundle is what ships, and the rebuild is manual:

```sh
npm install          # the pinned versions in package.json
npx esbuild codemirror-entry.js --bundle --format=iife \
  --global-name=DVEditor --minify --outfile=codemirror.js
```

`editor.rs`'s `the_committed_bundle_was_rebuilt_after_the_entry_file_changed`
is a partial backstop for skipping that step: it reads `codemirror.js` and
fails if the wire format or the blur handler is missing from it, which catches
a bundle that was reverted or never rebuilt at all. It cannot catch an
entry-file edit whose effect is confined to code it doesn't name — only running
esbuild in CI would, and the point of committing the bundle is that the build
needs no JS toolchain. So the rebuild remains a step you have to remember.

The bundle exposes:

- `DVEditor.create(id, parentElementId, dialect, initialDoc, schema, buffer,
  generation)` — `dialect` is `"sqlite"`, `"postgres"`, or `"mssql"`
  (anything else falls back to SQLite). `schema` is a lang-sql `SQLNamespace`
  object mapping `"table"` (SQLite) or `"schema.table"` (Postgres/SQL Server;
  literal dots in names escaped as `\.`) to arrays of column names. Postgres
  views get `defaultSchema: "public"`; SQL Server gets `defaultSchema:
  "dbo"`. `buffer` and `generation` are the query tab this document belongs
  to and its document generation — see below.
- `DVEditor.updateSchema(id, dialect, schema)` — swaps the completion data
  in place (the sql() extension sits in a Compartment), used after schema
  reloads.
- `DVEditor.setDoc(id, text, buffer, generation)` — replaces the document and
  takes the new token with it.
- `DVEditor.destroy(id)`.

It calls `window.__dvRun` / `window.__dvDoc` (installed by src/ui/editor.rs
through the dioxus eval channel).

## Document sync (FRE-154)

Typing is pushed to Rust on a 250 ms trailing timer, and **every way out of
the editor flushes that timer rather than cancelling it**: losing focus,
`setDoc`, and `destroy`. Cancelling is what used to drop the last 250 ms of
typing whenever a query tab or a pane was switched away from, silently.

Flushing on the way out means a message describing the *outgoing* buffer
arrives after the switch, on purpose. So every message carries the
`buffer`/`generation` token the editor held when the text was typed, and the
Rust side files it by that rather than by whichever buffer is active on
arrival. The generation is what distinguishes the two cases that would
otherwise look alike: a tab switch replaces no document, so the token still
matches and the tail is kept; a load *does* replace one and moves the
generation, so the editor's report of the text it replaced is discarded
instead of undoing the load.

Of the three flush points, blur is the one that does the work — it fires on
the mousedown that begins the switch, well before anything acts on it. The
`destroy` flush is best effort: teardown is also what closes the channel the
message rides.
