import { EditorView, keymap, lineNumbers, highlightActiveLine } from "@codemirror/view";
import { EditorState, Prec } from "@codemirror/state";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { sql, SQLite, PostgreSQL } from "@codemirror/lang-sql";
import { oneDark } from "@codemirror/theme-one-dark";

const views = new Map();
const debounces = new Map();

export function create(id, parentId, dialectName, initialDoc) {
  destroy(id);
  const parent = document.getElementById(parentId);
  if (!parent) return false;
  const dialect = dialectName === "postgres" ? PostgreSQL : SQLite;
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
  const view = new EditorView({
    state: EditorState.create({
      doc: initialDoc || "",
      extensions: [
        lineNumbers(),
        history(),
        highlightActiveLine(),
        keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
        runKeys,
        syncDoc,
        sql({ dialect }),
        oneDark,
        EditorView.theme({ "&": { height: "100%" }, ".cm-scroller": { overflow: "auto" } }),
      ],
    }),
    parent,
  });
  views.set(id, view);
  view.focus();
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
  clearTimeout(debounces.get(id));
  debounces.delete(id);
}
