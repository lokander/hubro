import { EditorView, keymap, lineNumbers, highlightActiveLine } from "@codemirror/view";
import { EditorState, Compartment, Prec } from "@codemirror/state";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { autocompletion } from "@codemirror/autocomplete";
import { sql, SQLite, PostgreSQL } from "@codemirror/lang-sql";
import { oneDark } from "@codemirror/theme-one-dark";

const views = new Map();
const debounces = new Map();
const sqlCompartments = new Map();

// Builds the sql() language support for a dialect plus a schema namespace
// object (lang-sql SQLNamespace: "table" or "schema.table" keys mapping to
// column-name arrays). Postgres gets defaultSchema so `public` tables
// complete unqualified.
function sqlSupport(dialectName, schema) {
  const postgres = dialectName === "postgres";
  return sql({
    dialect: postgres ? PostgreSQL : SQLite,
    schema: schema || {},
    defaultSchema: postgres ? "public" : undefined,
    upperCaseKeywords: true,
  });
}

export function create(id, parentId, dialectName, initialDoc, schema) {
  destroy(id);
  const parent = document.getElementById(parentId);
  if (!parent) return false;
  const runKeys = Prec.highest(
    keymap.of([
      {
        key: "Ctrl-Enter",
        mac: "Cmd-Enter",
        run: (view) => {
          const sel = view.state.selection.main;
          const text = sel.empty
            ? view.state.doc.toString()
            : view.state.sliceDoc(sel.from, sel.to);
          if (window.__dvRun) window.__dvRun({ id, sql: text, selection: !sel.empty });
          return true;
        },
      },
    ])
  );
  const syncDoc = EditorView.updateListener.of((update) => {
    if (!update.docChanged) return;
    clearTimeout(debounces.get(id));
    debounces.set(
      id,
      setTimeout(() => {
        if (window.__dvDoc) window.__dvDoc({ id, doc: update.state.doc.toString() });
      }, 250)
    );
  });
  // The sql() extension lives in a compartment so updateSchema can swap in
  // fresh completion data after a schema reload without recreating the view.
  const sqlCompartment = new Compartment();
  const view = new EditorView({
    state: EditorState.create({
      doc: initialDoc || "",
      extensions: [
        lineNumbers(),
        history(),
        highlightActiveLine(),
        autocompletion(),
        keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
        runKeys,
        syncDoc,
        sqlCompartment.of(sqlSupport(dialectName, schema)),
        oneDark,
        EditorView.theme({ "&": { height: "100%" }, ".cm-scroller": { overflow: "auto" } }),
      ],
    }),
    parent,
  });
  views.set(id, view);
  sqlCompartments.set(id, sqlCompartment);
  view.focus();
  return true;
}

export function updateSchema(id, dialectName, schema) {
  const view = views.get(id);
  const sqlCompartment = sqlCompartments.get(id);
  if (!view || !sqlCompartment) return false;
  view.dispatch({ effects: sqlCompartment.reconfigure(sqlSupport(dialectName, schema)) });
  return true;
}

export function setDoc(id, text) {
  const view = views.get(id);
  if (!view) return false;
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: text || "" },
  });
  view.focus();
  return true;
}

export function destroy(id) {
  const view = views.get(id);
  if (view) {
    view.destroy();
    views.delete(id);
  }
  sqlCompartments.delete(id);
  clearTimeout(debounces.get(id));
  debounces.delete(id);
}
