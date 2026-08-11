import { EditorView, keymap, lineNumbers, highlightActiveLine } from "@codemirror/view";
import { Annotation, EditorState, Compartment, Prec } from "@codemirror/state";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { autocompletion } from "@codemirror/autocomplete";
import { sql, SQLite, PostgreSQL, MSSQL } from "@codemirror/lang-sql";
import { oneDark } from "@codemirror/theme-one-dark";

const views = new Map();
const debounces = new Map();
const sqlCompartments = new Map();
// Which buffer each editor's document belongs to, and which generation of that
// buffer's text it was built from. Sent back with every message, so the host
// never has to infer from *when* a message arrived which query tab the text in
// it was typed into (FRE-154).
const tokens = new Map();

// Marks the dispatches made on the host's behalf (setDoc), so the sync
// listener can tell them from the user's typing. Without it, replacing the
// document would schedule a send of text the host had just pushed in.
const fromHost = Annotation.define();

// How long typing settles before the document is pushed to the host. The
// trailing edge is what keeps a fast typist from sending a message per
// keystroke; every way *out* of the editor flushes early, so the delay costs
// nothing but a message.
const SYNC_DELAY_MS = 250;

// Builds the sql() language support for a dialect plus a schema namespace
// object (lang-sql SQLNamespace: "table" or "schema.table" keys mapping to
// column-name arrays). Postgres/SQL Server get a defaultSchema so
// `public`/`dbo` tables complete unqualified.
function sqlSupport(dialectName, schema) {
  const dialect =
    dialectName === "postgres" ? PostgreSQL : dialectName === "mssql" ? MSSQL : SQLite;
  const defaultSchema =
    dialectName === "postgres" ? "public" : dialectName === "mssql" ? "dbo" : undefined;
  return sql({
    dialect,
    schema: schema || {},
    defaultSchema,
    upperCaseKeywords: true,
  });
}

// Sends the document to the host now, if a debounced send is waiting.
//
// Every exit from the editor calls this: losing focus, having the document
// replaced, and teardown. Before FRE-154 each of those *cancelled* the pending
// timer instead, silently dropping whatever had been typed in the last
// SYNC_DELAY_MS — on a tab switch, the tail of the outgoing buffer.
//
// The text is read from the view rather than captured when the timer was
// armed, so a flush always sends what is on screen at the moment it fires.
function flush(id) {
  const pending = debounces.get(id);
  if (pending === undefined) return;
  clearTimeout(pending);
  debounces.delete(id);
  const view = views.get(id);
  const token = tokens.get(id);
  if (!view || !token || !window.__dvDoc) return;
  window.__dvDoc({
    id,
    doc: view.state.doc.toString(),
    buffer: token.buffer,
    generation: token.generation,
  });
}

export function create(id, parentId, dialectName, initialDoc, schema, buffer, generation) {
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
          // Same guard as flush(), and for the same reason: a message with no
          // buffer on it stringifies without the field and fails to
          // deserialize on the host, so a defaulted `{}` would turn Ctrl+Enter
          // into a silent no-op rather than an obvious one.
          const token = tokens.get(id);
          if (token && window.__dvRun)
            window.__dvRun({ id, sql: text, selection: !sel.empty, buffer: token.buffer });
          return true;
        },
      },
    ])
  );
  const syncDoc = EditorView.updateListener.of((update) => {
    if (!update.docChanged) return;
    // A document the host pushed is not news to the host.
    if (update.transactions.some((tr) => tr.annotation(fromHost))) return;
    clearTimeout(debounces.get(id));
    debounces.set(
      id,
      setTimeout(() => flush(id), SYNC_DELAY_MS)
    );
  });
  // Losing focus is the earliest point at which a switch away from this editor
  // is visible here: it precedes the click that causes the switch, so the tail
  // is on its way to the host before anything acts on that click (FRE-154).
  const flushOnBlur = EditorView.domEventHandlers({
    blur: () => {
      flush(id);
    },
  });
  // The sql() extension lives in a compartment so updateSchema can swap in
  // fresh completion data after a schema reload without recreating the view.
  const sqlCompartment = new Compartment();
  tokens.set(id, { buffer, generation });
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
        flushOnBlur,
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

export function setDoc(id, text, buffer, generation) {
  const view = views.get(id);
  if (!view) return false;
  // Anything still pending belongs to the document about to be replaced, so it
  // goes out first — under the *old* token, which is what lets the host file it
  // under the buffer it was typed in rather than the one arriving.
  flush(id);
  tokens.set(id, { buffer, generation });
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: text || "" },
    annotations: fromHost.of(true),
  });
  view.focus();
  return true;
}

export function destroy(id) {
  // Best effort, and deliberately not the only guard: teardown runs as the
  // pane unmounts, which is also what closes the channel this message rides.
  // The blur that precedes the unmount is what actually saves the tail; this
  // covers a teardown that took focus away from nothing.
  flush(id);
  const view = views.get(id);
  if (view) {
    view.destroy();
    views.delete(id);
  }
  sqlCompartments.delete(id);
  tokens.delete(id);
  // Teardown deliberately cancels nothing: flush() above takes the pending
  // timer whenever there is one, so a cancel down here could only ever be a
  // no-op — and cancelling on the way out is the pre-FRE-154 behaviour that
  // lost the tail, so keeping one for symmetry would leave the bug one edit
  // away. (`every_way_out_of_the_editor_flushes_rather_than_cancels` reads
  // this function and enforces it.)
}
