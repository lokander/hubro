//! The page the grid shows: what the user asked for — the page number, the
//! sort, the filter and a refresh — and what was fetched for it.
//!
//! [`use_grid_query`] holds the asking side and [`use_grid_page`] the fetching
//! side, as two hooks rather than one because [`DataGrid`]'s reset effect
//! sits between them: it watches the query and has to be installed before the
//! fetch, the order the effects have always run in. The paging rules both
//! the Next button and PageDown obey are plain functions over the resources'
//! data, here beside the resources they read.

use super::*;

pub(super) const PAGE_SIZE: u32 = 100;

/// The table's row count, as far as paging may rely on it: `None` while the
/// count is running or has failed. A *re-running* count still holds its old
/// value (`use_resource` keeps it and flips its state to Pending), so it is
/// unknown rather than stale — new rows are never paired with a count from
/// before a filter change or a write.
pub(super) fn settled_count<E>(pending: bool, count: Option<&Result<u64, E>>) -> Option<u64> {
    if pending {
        return None;
    }
    count.and_then(|count| count.as_ref().ok().copied())
}

/// Whether `page`, holding `loaded` rows, is the last one there is — the one
/// rule both Next and PageDown obey. `loaded` is `None` while the page is
/// still being fetched, and that is the end for now: the rows on screen are
/// the previous page's, and a full previous page would otherwise wave the
/// user on past rows nobody has seen — measured on a view whose deep pages
/// take seconds, PageDown queued a fetch per press, ahead of all of them.
///
/// A known count decides it. An unknown one (see [`settled_count`]) does not
/// block paging: a short page is the end whatever the count says, and a full
/// one may have more behind it. Treating unknown as the end left a slow
/// `COUNT(*)` holding the grid on its page for the whole count — 38 s on a
/// large view — and a failed count holding it there for good. The price is a
/// table of exactly a multiple of [`PAGE_SIZE`] rows, whose last full page
/// offers a next one that comes back empty; that page is short, so it is the
/// end, and the clamp pulls it back once the count is known.
pub(super) fn at_end(count: Option<u64>, page: u64, loaded: Option<u64>) -> bool {
    let Some(loaded) = loaded else {
        return true;
    };
    match count {
        Some(count) => page * PAGE_SIZE as u64 + loaded >= count,
        None => loaded < PAGE_SIZE as u64,
    }
}

/// The page to pull the grid back to when `page` is past the last one, or
/// `None` to leave it where it is.
///
/// A delete or an external write (via Refresh) can leave the table with fewer
/// pages than the one being viewed, and [`at_end`] lets the user page ahead of
/// a count still running — onto an empty page, when the rows are an exact
/// multiple of [`PAGE_SIZE`]. Either way the footer would read "rows 301–300
/// of 250" over an empty page. Only a known count moves the page: while it is
/// unknown, paging ahead of it is what the user asked for.
pub(super) fn clamped_page(count: Option<u64>, page: u64) -> Option<u64> {
    let last_page = count?.saturating_sub(1) / PAGE_SIZE as u64;
    (page > last_page).then_some(last_page)
}

/// What the rows resource holds: the page, the hidden key column it fetched
/// beside the visible ones, and the offset it was fetched at.
pub(super) type FetchedPage = (Page, Option<String>, u64);

/// The offset a page fetch was made at and the rows it returned, out of the
/// rows resource; `None` while it has nothing (or failed).
pub(super) fn fetched_rows<E>(fetched: Option<&Result<FetchedPage, E>>) -> Option<(u64, u64)> {
    match fetched? {
        Ok((page, _, offset)) => Some((*offset, page.result.rows.len() as u64)),
        Err(_) => None,
    }
}

/// Whether Next and PageDown may move on from `page` — the whole rule, for
/// both, from the resources' plain data.
///
/// `fetched` is the offset the rows on screen were fetched at and how many
/// there are. While the page asked for is still being fetched those are the
/// previous page's rows, and a full previous page says nothing about the one
/// after: answering from them let PageDown run ahead of a slow deep-page
/// fetch, queueing a fetch per press. So rows count only when they were
/// fetched for `page` itself — compared by offset, which does not depend on
/// the resource having flipped to Pending by the time the next key arrives.
pub(super) fn next_page_offered<E>(
    count_pending: bool,
    count: Option<&Result<u64, E>>,
    page: u64,
    fetched: Option<(u64, u64)>,
) -> bool {
    let loaded =
        fetched.and_then(|(offset, rows)| (offset == page * PAGE_SIZE as u64).then_some(rows));
    !at_end(settled_count(count_pending, count), page, loaded)
}

/// The footer's account of the rows on screen: fetched at `offset`, `rows` of
/// them, out of `count` when it is known. Driven by the fetched offset rather
/// than the page asked for, so it never labels one page's rows with another's
/// range while a fetch is in flight. A page with nothing on it says so: past
/// the first it is the empty page beyond a table of an exact multiple of
/// [`PAGE_SIZE`] rows, which a failed count leaves in place for good.
pub(super) fn row_range(offset: u64, rows: u64, count: Option<u64>) -> String {
    let of = count.map_or_else(|| "…".to_string(), |count| count.to_string());
    match (rows, offset) {
        (0, 0) => "no rows".to_string(),
        (0, _) => "no more rows".to_string(),
        _ => format!("rows {}–{} of {of}", offset + 1, offset + rows),
    }
}

/// The staged filter inputs and the filter the fetch actually runs with.
///
/// The inputs only reach the query when applied, so typing doesn't fire a
/// query per keystroke. `applied` is what the page fetch, the row count and
/// an export read.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct GridFilter {
    pub(super) column: Signal<String>,
    pub(super) op: Signal<FilterOp>,
    pub(super) text: Signal<String>,
    pub(super) applied: Signal<Option<Filter>>,
}

/// What the user asked the grid to show: which page, in which order, through
/// which filter, and this grid's refresh nonce. Everything that re-fetches
/// the rows reads from here.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct GridQuery {
    pub(super) page: Signal<u64>,
    pub(super) sort: Signal<Option<(String, SortDir)>>,
    pub(super) filter: GridFilter,
    /// This grid's own entry of [`AppState::grid_refresh`] — see
    /// [`use_grid_query`] for why it is a memo.
    pub(super) refresh: Memo<Option<u64>>,
}

impl GridQuery {
    /// Applies the staged filter inputs and resets to the first page.
    pub(super) fn apply_filter(self) {
        let GridFilter {
            column,
            op,
            text,
            mut applied,
        } = self.filter;
        let mut page = self.page;
        let column = column.peek().clone();
        let value = text.peek().clone();
        if column.is_empty() || value.is_empty() {
            applied.set(None);
        } else {
            applied.set(Some(Filter::Column {
                column,
                op: *op.peek(),
                value,
            }));
        }
        page.set(0);
    }

    /// Clears the filter and returns to the first page — what both "Clear"
    /// affordances do (the toolbar's button and the no-rows-match empty
    /// state).
    ///
    /// Both the applied filter and the input text go: leaving the text behind
    /// would show a filter that isn't in effect.
    pub(super) fn clear_filter(self) {
        let mut applied = self.filter.applied;
        let mut text = self.filter.text;
        let mut page = self.page;
        applied.set(None);
        text.set(String::new());
        page.set(0);
    }

    /// A header click: the next step of `column`'s sort cycle, from the first
    /// page.
    pub(super) fn sort_by(self, column: &str) {
        let mut sort = self.sort;
        let mut page = self.page;
        let next = next_sort(&sort.peek(), column);
        sort.set(next);
        page.set(0);
    }
}

/// Sort cycles none → asc → desc → none per column.
pub(super) fn next_sort(
    current: &Option<(String, SortDir)>,
    column: &str,
) -> Option<(String, SortDir)> {
    match current {
        Some((c, SortDir::Asc)) if c == column => Some((column.to_string(), SortDir::Desc)),
        Some((c, SortDir::Desc)) if c == column => None,
        _ => Some((column.to_string(), SortDir::Asc)),
    }
}

/// The page, sort, filter and refresh nonce of one table's grid — see
/// [`GridQuery`].
///
/// The filter is seeded from a pending foreign-key focus targeting this
/// table, so a jump paints the referenced row with no unfiltered flash; the
/// effect here consumes that focus (and handles a same-table jump, where the
/// grid stays mounted and only the filter changes).
pub(super) fn use_grid_query(id: ConnectionId, table: &TableRef) -> GridQuery {
    let state = use_context::<AppState>();
    let mut page = use_signal(|| 0u64);
    let sort = use_signal(|| Option::<(String, SortDir)>::None);
    // The filter inputs are staged locally and only hit the query when
    // applied, so typing doesn't fire a query per keystroke.
    let filter_column = use_signal(String::new);
    let filter_op = use_signal(|| FilterOp::Contains);
    let filter_text = use_signal(String::new);
    // Seed the filter from a pending foreign-key focus targeting this table,
    // so a jump paints the referenced row with no unfiltered flash.
    // The consuming effect below clears the focus (and handles a same-table
    // jump, where the grid stays mounted and only the filter changes).
    let seed_table = table.clone();
    let mut applied_filter = use_signal(move || {
        state
            .pending_focus
            .peek()
            .get(&id)
            .filter(|focus| focus.table == seed_table)
            .and_then(|focus| focus.filter.clone())
    });
    let focus_table = table.clone();
    use_effect(move || {
        let mut state = state;
        let matched = state
            .pending_focus
            .read()
            .get(&id)
            .filter(|focus| focus.table == focus_table)
            .cloned();
        if let Some(focus) = matched {
            applied_filter.set(focus.filter);
            page.set(0);
            // Reading pending_focus above subscribes this effect; removing the
            // consumed entry re-runs it once (now no match) — no loop.
            state.pending_focus.write().remove(&id);
        }
    });

    // This grid's own refresh nonce, pulled out of the whole-map signal
    // through a memo: signal subscription is per-signal, not per-key, so a
    // raw `grid_refresh.read()` re-runs its readers on ANY write to the map —
    // a save bumping another table's nonce, or `close_connection` pruning a
    // closed tab's entries. The memo re-runs on those writes too, but its
    // PartialEq gate stops the propagation there: the grid's reset effect and
    // the resources in `use_grid_page` only re-run when THIS grid's nonce
    // actually changed.
    let nonce_table_key = table.key();
    let refresh_nonce = use_memo(move || {
        state
            .grid_refresh
            .read()
            .get(&(id, nonce_table_key.clone()))
            .copied()
    });

    GridQuery {
        page,
        sort,
        filter: GridFilter {
            column: filter_column,
            op: filter_op,
            text: filter_text,
            applied: applied_filter,
        },
        refresh: refresh_nonce,
    }
}

/// One table's page as fetched and as the grid renders and navigates it:
/// the two resources, the browsability gate they obey, and the three
/// derivations every reader shares.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct GridPage {
    /// The page itself, with the hidden key column and the offset it was
    /// fetched at.
    pub(super) rows: Resource<Result<FetchedPage, crate::db::DbError>>,
    /// `COUNT(*)` under the applied filter, fetched apart from the page.
    pub(super) count: Resource<Result<u64, crate::db::DbError>>,
    /// Why this object has no rows to list, when the backend declares so:
    /// the last answer known, kept across a schema reload.
    pub(super) unbrowsable: Signal<Option<&'static str>>,
    /// The introspected metadata every row renders against.
    pub(super) render_meta: Memo<Shared<TableRenderMeta>>,
    /// The fetched page reduced to what the grid renders.
    pub(super) view: Memo<PageView>,
    /// The keyboard-navigation model of the visible page.
    pub(super) nav: Memo<GridNav>,
}

/// Fetches the page and the row count `query` describes, keeps the page
/// number inside the table, and derives the render and navigation models
/// from what arrived.
pub(super) fn use_grid_page(id: ConnectionId, table: &TableRef, query: GridQuery) -> GridPage {
    let state = use_context::<AppState>();
    let GridQuery {
        mut page,
        sort,
        filter,
        refresh: refresh_nonce,
    } = query;
    let applied_filter = filter.applied;

    // Rowid-identity tables (SQLite, keyless) need the rowid in every
    // fetched row to build row locators, but `SELECT *` doesn't include
    // it — ask the page reader for it explicitly. The fetch's extra
    // column is returned alongside the result so rendering hides
    // exactly what this fetch prepended (never a stale render-time
    // guess).
    //
    // The bounded fetch also needs the table's columns (to preview large
    // ones) and the columns that must NOT be previewed: identity keys and
    // foreign-key columns, whose truncation would misaddress rows or
    // misdirect a jump.
    //
    // Derived through a memo because `registry` and `schemas` are whole-map
    // signals: a raw read in the resource would re-issue this grid's SQL
    // fetch on any other connection's open/close or schema load. The memo's
    // PartialEq gate lets the fetch re-run only when this table's own inputs
    // change.
    let meta_table = table.clone();
    let fetch_meta = use_memo(move || {
        let registry = state.registry.read();
        let schemas = state.schemas.read();
        let meta = find_table_meta(schemas.get(&id), &meta_table);
        match (meta, registry.get(id)) {
            (Some(meta), Some(connection)) => {
                // Row identity is a read concern here — it decides which
                // columns must be fetched whole — so it comes from the
                // resolved access, not from whether editing is allowed.
                let identity = connection.pool.backend_row_identity(meta);
                let extra = match &identity {
                    Some(RowIdentity::Rowid { column }) => Some(column.clone()),
                    _ => None,
                };
                let mut no_preview: Vec<String> = identity
                    .as_ref()
                    .map(|i| i.key_columns().iter().map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                for fk in &meta.foreign_keys {
                    no_preview.extend(fk.columns.iter().cloned());
                }
                (meta.columns.clone(), no_preview, extra)
            }
            _ => (Vec::new(), Vec::new(), None),
        }
    });

    // Whether this object has any rows to list at all, so that a
    // declared-unbrowsable object never reaches the server: the query it would
    // otherwise run is one the engine can only answer with an error. Two cases:
    // a RisingWave sink, which writes outward to Kafka or another database and
    // stores nothing, and a SQLite object the engine can list but not resolve
    // (a virtual table whose module is missing, a view over a dropped table).
    //
    // Resolved through a memo for the reason `fetch_meta` above is: `registry`
    // and `schemas` are whole-map signals, so a raw read would make every
    // other connection's open, close or protection change re-run this grid's
    // page fetch and its `COUNT(*)`. `Option<Option<_>>` because
    // "unknown" and "browsable" are different answers — a reload sets
    // `SchemaLoad::Loading`, which empties the table list, so the outer `None`
    // is the ordinary state during a sidebar Refresh rather than an edge case.
    let gate_table = table.clone();
    let resolved_gate = use_memo(move || {
        let registry = state.registry.read();
        let schemas = state.schemas.read();
        match (
            find_table_meta(schemas.get(&id), &gate_table),
            registry.get(id),
        ) {
            (Some(meta), Some(connection)) => Some(connection.access(meta).unreadable),
            _ => None,
        }
    });
    let mut unbrowsable = use_signal(|| resolved_gate().flatten());
    use_effect(move || {
        // Keeps the last known answer while the schema reloads, instead of
        // falling open and sending the very query the gate exists to stop —
        // the same treatment the SQL editor gives its completions. A table
        // does not stop being a sink because its schema is being re-read.
        //
        // Covers reloads, not the first load: a grid mounted by session
        // restore before its schema has ever arrived has no answer to keep, so
        // one refused query can still go out and be corrected when the schema
        // lands. Self-correcting, because the resources read this signal.
        //
        // Peeked before writing: `Signal::set` is unconditional, and both
        // resources subscribe to this, so setting an unchanged value would
        // re-fetch the page and re-count the rows for nothing — undoing the
        // memo's gate one line above.
        if let Some(reason) = resolved_gate() {
            if *unbrowsable.peek() != reason {
                unbrowsable.set(reason);
            }
        }
    });

    let table_for_resource = table.clone();
    let rows_resource = use_resource(move || {
        let table = table_for_resource.clone();
        // Read reactive deps before any await so the resource re-runs when
        // they change and no borrow spans the await.
        let (columns, no_preview, extra_key_column) = fetch_meta();
        let request = PageRequest {
            schema: table.schema.clone(),
            table: table.name.clone(),
            limit: PAGE_SIZE,
            offset: page() * PAGE_SIZE as u64,
            sort: sort(),
            filter: applied_filter(),
            extra_key_column,
        };
        let _ = refresh_nonce();
        let refused = unbrowsable();
        // Peeked, not read: subscribing to `registry` here would re-fetch on
        // any connection open/close. The pool for a ConnectionId never
        // changes while this grid is mounted — the registry's only writes
        // are `insert` (mints a fresh id), `set_protection` (pool untouched)
        // and `remove` (which unmounts this tab) — so there is no pool
        // change to react to; a reconnect is a new id and a new grid.
        //
        // `browse` hands over the connection's `Reading` with it, at fetch
        // time: it says whether the read runs where the engine refuses
        // writes (a marked connection — see `DbPool::fetch_page`), and a
        // marking changed since the last fetch applies to the next one.
        let pool = state.registry.peek().get(id).map(|c| c.browse());
        async move {
            if let Some(reason) = refused {
                return Err(crate::db::DbError::Unsupported(reason.to_string()));
            }
            let Some((pool, reading)) = pool else {
                return Err(crate::db::DbError::Query("connection closed".into()));
            };
            let no_preview_refs: Vec<&str> = no_preview.iter().map(String::as_str).collect();
            let page = pool
                .fetch_page_bounded(reading, &request, &columns, &no_preview_refs)
                .await?;
            // The offset travels with the rows, so whatever describes them —
            // the footer's range, the paging rule — describes the rows on
            // screen rather than the page asked for since.
            Ok::<FetchedPage, crate::db::DbError>((page, request.extra_key_column, request.offset))
        }
    });

    // Row count, fetched separately from the page so it is NOT re-run on every
    // page flip or sort change: `COUNT(*)` depends only on the table
    // and filter. It re-runs when the filter changes or a write bumps
    // `grid_refresh`; between those, Dioxus keeps the resolved value, so
    // flipping pages reuses the cached count with no extra query.
    let count_table = table.clone();
    let count_resource = use_resource(move || {
        let table = count_table.clone();
        let request = PageRequest {
            schema: table.schema.clone(),
            table: table.name.clone(),
            limit: PAGE_SIZE,
            offset: 0,
            sort: None, // COUNT(*) ignores ordering
            filter: applied_filter(),
            extra_key_column: None,
        };
        let _ = refresh_nonce();
        let refused = unbrowsable();
        // Peeked for the same reason as in `rows_resource`: the pool for a
        // ConnectionId is fixed for the connection's lifetime, and reading
        // `registry` would re-count on unrelated connection opens/closes.
        // With its `Reading`, as for the page.
        let pool = state.registry.peek().get(id).map(|c| c.browse());
        async move {
            // Gated as well as the page: counting rows an object does not have
            // is the same query failing for the same reason.
            if let Some(reason) = refused {
                return Err(crate::db::DbError::Unsupported(reason.to_string()));
            }
            let Some((pool, reading)) = pool else {
                return Err(crate::db::DbError::Query("connection closed".into()));
            };
            pool.count_rows(reading, &request).await
        }
    });

    // Clamp the page if it is past the last — see [`clamped_page`]. Setting
    // the page re-runs this once with a now-valid page, so it settles without
    // looping. (Reactive; verified live, not by unit test.)
    use_effect(move || {
        let count = settled_count(
            *count_resource.state().read() == UseResourceState::Pending,
            count_resource.read().as_ref(),
        );
        if let Some(last_page) = clamped_page(count, page()) {
            page.set(last_page);
        }
    });

    // The introspected metadata every row of this table renders against,
    // built once here and handed to the row components as a single
    // pointer-compared prop instead of a `HashMap`/`Vec` deep-cloned into each
    // of the ~30 windowed rows on every render.
    //
    // A memo because `schemas` and `registry` are whole-map signals: a raw
    // read would rebuild — and re-render every row — on any other connection's
    // schema load. The memo re-runs then too, but its PartialEq gate stops the
    // propagation there (the same gate that keeps another tab's writes from
    // resetting this grid).
    let render_table = table.clone();
    let render_meta = use_memo(move || {
        let schemas = state.schemas.read();
        let (dialect, rules) = state
            .registry
            .read()
            .get(id)
            .map(|c| (c.pool.dialect(), c.pool.key_rules()))
            .unzip();
        let load = schemas.get(&id);
        Shared::new(TableRenderMeta::build(
            find_table_meta(load, &render_table),
            dialect,
            |fk| rules.is_some_and(|rules| fk_jump_target(load, fk, rules).is_some()),
        ))
    });

    // The fetched page reduced to what the grid renders: headers,
    // the rows with the stage applied, and the rows "select all on this page"
    // ticks.
    //
    // This is the grid's expensive derivation — it clones every cell `Value`
    // of the 100-row page. Deriving it in the render body re-ran it on every
    // arrow key, shift-click, copy and checkbox tick, because the render also
    // reads `focused_cell`, `selection`, `copy_status` and `selected`. Behind
    // a memo none of those reach it: a focus move now costs the two `GridRow`
    // diffs it should.
    //
    // It is not fully insulated, and doesn't need to be. `schemas`, `registry`
    // and `stages` are whole-map signals, so this still re-runs on an
    // unrelated connection's schema load or save — as the render body always
    // did. What changed is that the PartialEq gate below stops such a rebuild
    // from reaching the rows, so the cost is one derivation rather than a
    // re-render of the grid.
    let page_table = table.clone();
    let page_view = use_memo(move || {
        let meta = render_meta();
        let current = rows_resource.read();
        let Some(Ok((page, extra_key, _))) = current.as_ref() else {
            return PageView::default();
        };
        let result = &page.result;
        // Same resolution the render uses, so keyboard navigation offers
        // exactly the editors the grid shows — the user's marking
        // included. Resolving the *backend's* answer here instead would let
        // Enter open an editor on a cell the mouse correctly refuses.
        let access = find_table_meta(state.schemas.read().get(&id), &page_table)
            .and_then(|meta| state.table_access(id, meta));
        let identity = access.as_ref().and_then(|a| a.identity.clone());
        let can_mutate = access.as_ref().is_some_and(TableAccess::can_mutate);
        let stage = state.table_stage(id, &page_table);
        // The fetch prepended the row-identity key column (rowid) when one was
        // requested; keep it for locators, hide it from display.
        let hidden = usize::from(extra_key.is_some());
        let headers: Vec<String> = if result.columns.is_empty() {
            meta.schema_columns.clone()
        } else {
            result
                .columns
                .iter()
                .skip(hidden)
                .map(|c| c.name.clone())
                .collect()
        };
        let rows = view_rows(
            result,
            &page.previews,
            hidden,
            identity.as_ref(),
            stage.as_ref(),
            can_mutate,
        );
        PageView {
            headers: Shared::new(headers),
            selectable: Shared::new(selectable_rows(&rows)),
            rows,
        }
    });

    // Keyboard-navigation model of the visible page: read by the grid
    // container's key handler for focus movement and Enter. Built from the
    // same `page_view` the render consumes — one row derivation, two readers —
    // but owning its data so the `'static` key closure can read it off-render.
    let nav_table = table.clone();
    let grid_nav = use_memo(move || {
        let view = page_view.read();
        let meta = render_meta();
        // The phantom rows render only under a loaded page, so they join
        // the focus ring only then too.
        let inserts = match (
            rows_resource.read().as_ref(),
            state.table_stage(id, &nav_table),
        ) {
            (Some(Ok(_)), Some(stage)) => stage.inserts().to_vec(),
            _ => Vec::new(),
        };
        GridNav::build((*view.headers).clone(), &view.rows, &meta.column_kinds)
            .with_inserts(&inserts, &meta.column_kinds)
    });

    GridPage {
        rows: rows_resource,
        count: count_resource,
        unbrowsable,
        render_meta,
        view: page_view,
        nav: grid_nav,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_count_is_unknown_even_while_it_holds_an_old_value() {
        let stale: Result<u64, ()> = Ok(1_000);
        assert_eq!(settled_count(true, Some(&stale)), None);
        assert_eq!(settled_count(false, Some(&stale)), Some(1_000));
        assert_eq!(settled_count(false, Some(&Err::<u64, ()>(()))), None);
        assert_eq!(settled_count::<()>(false, None), None);
        // A stale count is not believed: the old one said 1 000 rows, yet a
        // short third page is the end while the new one runs.
        assert!(!at_end(settled_count(false, Some(&stale)), 2, Some(40)));
        assert!(at_end(settled_count(true, Some(&stale)), 2, Some(40)));
    }

    #[test]
    fn the_last_page_is_where_the_rows_reach_the_count() {
        let full = PAGE_SIZE as u64;
        // Exactly two pages: the first has a next, the second does not.
        assert!(!at_end(Some(2 * full), 0, Some(full)));
        assert!(at_end(Some(2 * full), 1, Some(full)));
        // One row past a page boundary is a page of its own.
        assert!(!at_end(Some(full + 1), 0, Some(full)));
        assert!(at_end(Some(full + 1), 1, Some(1)));
        // A short page is the last, and an empty table has no next page.
        assert!(at_end(Some(40), 0, Some(40)));
        assert!(at_end(Some(0), 0, Some(0)));
    }

    /// A slow count must not hold paging for its whole run, nor a failed
    /// one for good: with the count unknown, a full page may have more
    /// behind it and a short page is the end.
    #[test]
    fn an_unknown_count_lets_a_full_page_go_on_and_stops_a_short_one() {
        let full = PAGE_SIZE as u64;
        assert!(!at_end(None, 0, Some(full)));
        assert!(!at_end(None, 7, Some(full)));
        assert!(at_end(None, 0, Some(full - 1)));
        assert!(at_end(None, 3, Some(1)));
        // Rows an exact multiple of the page: the last full page offers a
        // next, which comes back empty — and that empty page is the end, so
        // PageDown stops there rather than walking on through empty pages.
        assert!(!at_end(None, 1, Some(full)));
        assert!(at_end(None, 2, Some(0)));
        // Once the count lands, the empty page is past the last and is
        // pulled back to it…
        assert_eq!(clamped_page(Some(2 * full), 2), Some(1));
        // …but while it is unknown, paging ahead of it is left alone.
        assert_eq!(clamped_page(None, 2), None);
        assert_eq!(clamped_page(None, 40), None);
    }

    /// While a page is being fetched the rows on screen are the previous
    /// page's, so their being a full page says nothing about the next one.
    #[test]
    fn a_page_still_loading_is_the_end_for_now() {
        assert!(at_end(None, 3, None));
        assert!(at_end(Some(1_000_000), 3, None));
    }

    /// The rule both Next and PageDown call, from the resources' raw data:
    /// rows count only when they were fetched for the page now asked for.
    #[test]
    fn a_next_page_is_offered_only_from_rows_fetched_for_this_page() {
        let full = PAGE_SIZE as u64;
        let unknown: Option<&Result<u64, ()>> = None;
        // Page 3 asked for; page 2's full rows still on screen: no next yet,
        // whatever the count — this is what stops PageDown queueing fetches.
        assert!(!next_page_offered(true, unknown, 3, Some((2 * full, full))));
        assert!(!next_page_offered(
            false,
            Some(&Ok::<u64, ()>(1_000_000)),
            3,
            Some((2 * full, full))
        ));
        // Page 3's own rows arrived: a full page goes on while counting…
        assert!(next_page_offered(true, unknown, 3, Some((3 * full, full))));
        // …and a failed count does not stop it either.
        assert!(next_page_offered(
            false,
            Some(&Err(())),
            3,
            Some((3 * full, full))
        ));
        // A known count still has the last word.
        assert!(!next_page_offered(
            false,
            Some(&Ok::<u64, ()>(4 * full)),
            3,
            Some((3 * full, full))
        ));
        // Nothing fetched yet, or the fetch failed: no next page.
        assert!(!next_page_offered(true, unknown, 0, None));
        let failed: Result<FetchedPage, ()> = Err(());
        assert_eq!(fetched_rows(Some(&failed)), None);
    }

    #[test]
    fn the_row_range_describes_the_rows_on_screen() {
        let full = PAGE_SIZE as u64;
        assert_eq!(row_range(0, full, Some(250)), "rows 1–100 of 250");
        assert_eq!(row_range(2 * full, 50, None), "rows 201–250 of …");
        // The empty page past an exact multiple — with the count failed it
        // stays, so it must not claim a range.
        assert_eq!(row_range(2 * full, 0, None), "no more rows");
        assert_eq!(row_range(0, 0, Some(0)), "no rows");
    }

    #[test]
    fn the_page_is_clamped_only_when_past_a_known_last_page() {
        let full = PAGE_SIZE as u64;
        assert_eq!(clamped_page(Some(250), 2), None);
        assert_eq!(clamped_page(Some(250), 3), Some(2));
        assert_eq!(clamped_page(Some(2 * full), 1), None);
        // An empty table's only page is the first.
        assert_eq!(clamped_page(Some(0), 0), None);
        assert_eq!(clamped_page(Some(0), 4), Some(0));
    }

    #[test]
    fn sort_cycles_per_column() {
        let none = None;
        let asc = next_sort(&none, "a");
        assert_eq!(asc, Some(("a".to_string(), SortDir::Asc)));
        let desc = next_sort(&asc, "a");
        assert_eq!(desc, Some(("a".to_string(), SortDir::Desc)));
        assert_eq!(next_sort(&desc, "a"), None);
    }

    #[test]
    fn sorting_another_column_starts_at_asc() {
        let current = Some(("a".to_string(), SortDir::Desc));
        assert_eq!(
            next_sort(&current, "b"),
            Some(("b".to_string(), SortDir::Asc))
        );
    }
}
