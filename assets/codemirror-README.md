# codemirror.js

`codemirror.js` is a prebuilt, committed bundle (CodeMirror 6 + SQL language
+ One Dark theme) so the app works offline with no JS toolchain in the build.

Source: `codemirror-entry.js`. To regenerate:

```sh
npm install codemirror @codemirror/view @codemirror/state \
  @codemirror/commands @codemirror/lang-sql @codemirror/theme-one-dark esbuild
npx esbuild codemirror-entry.js --bundle --format=iife \
  --global-name=DVEditor --minify --outfile=codemirror.js
```

The bundle exposes `DVEditor.create(id, parentElementId, dialect, initialDoc)`
and `DVEditor.destroy(id)`; it calls `window.__dvRun` / `window.__dvDoc`
(installed by src/ui/editor.rs through the dioxus eval channel).
