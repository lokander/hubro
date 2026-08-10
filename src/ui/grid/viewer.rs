//! Rich cell viewers (FRE-115): one value rendered as what it *is* — a JSON
//! tree, an image, a hex dump, or a scrollable text pane — instead of as a
//! wall of escaped text or a `<blob 12 KB>` placeholder.
//!
//! Read-only by design. Editing still goes through [`CellEditor`] on the raw
//! value: everything here is a rendering of the bytes, and several of those
//! renderings (a pretty-printed document, a hex dump) are not round-trippable
//! text. "Copy raw" beside a viewer copies the value, never the view.
//!
//! **How a viewer is chosen.** The declared column type decides what it can
//! decide and content decides the rest:
//!
//! - The *type* separates a fixed-size scalar from text ([`classify_column`]).
//!   That is a real difference here because [`Value`] has no date/numeric
//!   variant — a `timestamp` and a `text` column both arrive as
//!   [`Value::Text`], and only the declared type says one is a one-line scalar
//!   and the other wants a scrollable wrapping pane.
//! - *Content* separates the rest, because that is where the answer actually
//!   lives: a JSON document is JSON whether its column is `jsonb`, MySQL
//!   `JSON` or SQL Server `nvarchar`, and a PNG in a `bytea` is a PNG only if
//!   its magic bytes say so.
//!
//! **Every decode is bounded.** [`IMAGE_MAX_BYTES`], [`JSON_MAX_BYTES`],
//! [`JSON_TREE_MAX_NODES`] and [`HEX_DUMP_MAX_BYTES`] are ceilings on what a
//! viewer will turn into DOM; past one of them the viewer *declines in favour
//! of a cheaper view and says so* rather than working on a value big enough to
//! stall the webview. The same refusal covers an incomplete value: a blob that
//! arrived as a prefix (past [`FETCH_CELL_MAX_BYTES`]) is never handed to
//! `<img>`, because half a PNG is not an image.

use super::*;

use base64::Engine as _;

use crate::db::{classify_column, ColumnClass};

/// Largest blob rendered as an image. A data URL costs ~4/3 of the blob in
/// DOM on top of the bytes themselves, so the ceiling is on the decode as
/// much as on the picture; 4 MiB comfortably covers the avatars, thumbnails
/// and scanned pages that live in databases. Past it the viewer shows a hex
/// dump and says why — the 200 MB blob case (FRE-115).
pub(super) const IMAGE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Largest text a viewer will *attempt* to parse as JSON. Past it the text is
/// shown as text: parsing costs a full copy as a `serde_json::Value` plus
/// another as rendered nodes, and a document this size is not read as a tree
/// anyway.
pub(super) const JSON_MAX_BYTES: usize = 1024 * 1024;

/// Largest JSON document rendered as a collapsible tree, counted in nodes
/// (every scalar, object and array). Past it the pretty-printed text goes into
/// the text pane — one string in one `<pre>`, rather than tens of thousands of
/// elements each with their own collapse state.
pub(super) const JSON_TREE_MAX_NODES: usize = 5_000;

/// Bytes shown in a hex dump. A dump identifies content (a header, an
/// encoding, a magic number); it is not a way to read a file, and 16 KiB is
/// already 1024 lines.
pub(super) const HEX_DUMP_MAX_BYTES: usize = 16 * 1024;

/// Tree depth that starts expanded. Deeper nodes render collapsed so a nested
/// document opens readable instead of as a full unfolded dump.
const JSON_OPEN_DEPTH: usize = 2;

/// Bytes per hex dump line — the `hexdump -C` layout, gutter and all.
const HEX_COLUMNS: usize = 16;

/// An image format the webview can render from a data URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageFormat {
    pub(super) fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::Webp => "image/webp",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            ImageFormat::Png => "PNG",
            ImageFormat::Jpeg => "JPEG",
            ImageFormat::Gif => "GIF",
            ImageFormat::Webp => "WebP",
        }
    }
}

/// The image format `bytes` starts with, by magic bytes — never by column
/// name or by a file extension nobody stored.
///
/// Deliberately a prefix test only: it identifies the *container*, and the
/// webview is the one that decodes the picture. A truncated or corrupt file
/// that still carries its header is offered to `<img>` and renders as far as
/// it goes, which is why [`choose_view`] refuses to route an incomplete value
/// here at all.
pub(super) fn sniff_image(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ImageFormat::Png);
    }
    // JFIF/Exif/raw: every JPEG starts SOI + the first marker's 0xFF.
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(ImageFormat::Gif);
    }
    // RIFF container with a WEBP form type; the 4 bytes between are the
    // (unchecked) file size.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(ImageFormat::Webp);
    }
    None
}

/// `hexdump -C` layout: an 8-digit offset, 16 bytes as hex split into two
/// groups of eight, then the printable ASCII with everything else as `.`.
///
/// The caller bounds the input ([`HEX_DUMP_MAX_BYTES`]); this formats whatever
/// it is given.
pub(super) fn hex_dump(bytes: &[u8]) -> String {
    // 78 chars per full line, and the ceiling on `bytes` bounds the rest.
    let mut out = String::with_capacity(bytes.len() / HEX_COLUMNS * 78 + 16);
    for (line, chunk) in bytes.chunks(HEX_COLUMNS).enumerate() {
        let offset = line * HEX_COLUMNS;
        out.push_str(&format!("{offset:08x}  "));
        for column in 0..HEX_COLUMNS {
            match chunk.get(column) {
                Some(byte) => out.push_str(&format!("{byte:02x} ")),
                // A short last line keeps the ASCII gutter aligned.
                None => out.push_str("   "),
            }
            if column == HEX_COLUMNS / 2 - 1 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for byte in chunk {
            out.push(if (0x20..0x7f).contains(byte) {
                *byte as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}

/// Nodes a document renders as a tree: itself plus every descendant. Bounded
/// by serde_json's own 128-level nesting limit, so the recursion is safe on
/// anything that parsed.
pub(super) fn json_node_count(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => 1 + map.values().map(json_node_count).sum::<usize>(),
        serde_json::Value::Array(items) => 1 + items.iter().map(json_node_count).sum::<usize>(),
        _ => 1,
    }
}

/// How one cell is rendered.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ViewBody {
    /// SQL NULL — the marker, styled apart from an empty string.
    Null,
    /// A one-line value: a number, a date, a uuid. Kept out of the text pane
    /// so a form of scalars reads as a form.
    Inline(String),
    /// Text, in a scrollable wrapping pane.
    Text(String),
    /// A JSON document, as a collapsible tree.
    Json(serde_json::Value),
    /// An image, as a data URL the webview decodes.
    Image {
        data_url: String,
        format: ImageFormat,
        bytes: usize,
    },
    /// Binary content as a hex + ASCII dump: `shown` bytes of a value that is
    /// `total` bytes in all — which is bigger than what is in hand whenever
    /// the value arrived as a prefix.
    Hex {
        dump: String,
        shown: usize,
        total: u64,
    },
}

/// A chosen viewer, plus the one sentence explaining a decision the user would
/// otherwise have to guess at — why a picture is a hex dump, why a document is
/// flat text. `None` when the obvious view was the one rendered.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CellView {
    pub(super) body: ViewBody,
    pub(super) note: Option<String>,
}

impl CellView {
    fn plain(body: ViewBody) -> Self {
        CellView { body, note: None }
    }

    fn noted(body: ViewBody, note: String) -> Self {
        CellView {
            body,
            note: Some(note),
        }
    }
}

/// Picks the viewer for one cell (FRE-115).
///
/// `type_name` is the column's declared type as the panel shows it (empty when
/// there is none — the free-form query grid knows only what the driver
/// returned).
///
/// `truncated` is `None` when `value` is the whole cell, and `Some(full_len)`
/// when it is only a prefix of a value that long (a page preview, or a fetch
/// past [`FETCH_CELL_MAX_BYTES`]). One input rather than a flag plus a size,
/// so "complete" and "and here is how much is missing" cannot contradict each
/// other. A prefix is a different thing from a small value: decoding one as an
/// image would render part of a picture as if it were the whole.
pub(super) fn choose_view(value: &Value, type_name: &str, truncated: Option<u64>) -> CellView {
    match value {
        Value::Null => CellView::plain(ViewBody::Null),
        Value::Integer(_) | Value::Real(_) => CellView::plain(ViewBody::Inline(value.display())),
        Value::Blob(bytes) => blob_view(bytes, truncated),
        Value::Text(text) => text_view(text, type_name, truncated),
    }
}

/// An image when the bytes say so and the whole blob is in hand and small
/// enough to hand to the webview; a hex dump — with the reason — otherwise.
fn blob_view(bytes: &[u8], truncated: Option<u64>) -> CellView {
    let Some(format) = sniff_image(bytes) else {
        return hex_view(bytes, truncated, None);
    };
    if truncated.is_some() {
        return hex_view(
            bytes,
            truncated,
            Some(format!(
                "Only part of this value was loaded, so it can't be shown as {} — an image renders from the whole blob.",
                format.label(),
            )),
        );
    }
    if bytes.len() > IMAGE_MAX_BYTES {
        return hex_view(
            bytes,
            truncated,
            Some(format!(
                "{} image is {} — too large to render (limit {}).",
                format.label(),
                human_bytes(bytes.len() as u64),
                human_bytes(IMAGE_MAX_BYTES as u64),
            )),
        );
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    CellView::plain(ViewBody::Image {
        data_url: format!("data:{};base64,{encoded}", format.mime()),
        format,
        bytes: bytes.len(),
    })
}

/// A hex dump of the first [`HEX_DUMP_MAX_BYTES`], carrying `note` when it is
/// standing in for a view that was declined. The reported total is the stored
/// value's length, which for a prefix is more than the bytes in hand.
fn hex_view(bytes: &[u8], truncated: Option<u64>, note: Option<String>) -> CellView {
    let shown = bytes.len().min(HEX_DUMP_MAX_BYTES);
    let body = ViewBody::Hex {
        dump: hex_dump(&bytes[..shown]),
        shown,
        total: truncated.unwrap_or(bytes.len() as u64),
    };
    match note {
        Some(note) => CellView::noted(body, note),
        None => CellView::plain(body),
    }
}

/// A JSON tree when the text parses as a document, a scrollable pane when it
/// is text, one line when the column says the value is a scalar.
fn text_view(text: &str, type_name: &str, truncated: Option<u64>) -> CellView {
    // The one thing the declared type knows that the bytes don't: a
    // `timestamp`, `numeric` or `uuid` arrives as `Value::Text` exactly like a
    // `text` column does, and only its type says it is one short line rather
    // than a document.
    if classify_column(type_name) == ColumnClass::Scalar {
        return CellView::plain(ViewBody::Inline(text.to_string()));
    }
    let pane = || CellView::plain(ViewBody::Text(text.to_string()));
    // Only a JSON object or array is worth a tree; a bare scalar document
    // re-renders as itself. This is also the cheap prefilter that keeps a
    // megabyte of prose away from the parser.
    if !matches!(text.trim_start().as_bytes().first(), Some(b'{' | b'[')) {
        return pane();
    }
    // A prefix of a document is not a document: it either fails to parse, or
    // (worse) parses as a smaller one that silently drops what was cut.
    if truncated.is_some() {
        return pane();
    }
    if text.len() > JSON_MAX_BYTES {
        return CellView::noted(
            ViewBody::Text(text.to_string()),
            format!(
                "Document is {} — too large to format (limit {}); showing it as text.",
                human_bytes(text.len() as u64),
                human_bytes(JSON_MAX_BYTES as u64),
            ),
        );
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return pane();
    };
    if !matches!(
        parsed,
        serde_json::Value::Object(_) | serde_json::Value::Array(_)
    ) {
        return pane();
    }
    let nodes = json_node_count(&parsed);
    if nodes > JSON_TREE_MAX_NODES {
        // Pretty-printing still helps; `pretty_json` re-parses, so format the
        // document already in hand.
        let pretty = serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| text.to_string());
        return CellView::noted(
            ViewBody::Text(pretty),
            format!(
                "Document has {nodes} nodes — too many for a tree (limit {JSON_TREE_MAX_NODES}); showing it pretty-printed.",
            ),
        );
    }
    CellView::plain(ViewBody::Json(parsed))
}

/// One cell rendered by whichever viewer [`choose_view`] picked (FRE-115).
///
/// Read-only: this is the value as it is, not a second way to write it.
#[component]
pub(super) fn CellViewer(
    value: Value,
    /// The column's declared type, or empty where none is known.
    type_name: String,
    /// The stored value's full length when `value` is only a prefix of it;
    /// `None` when the whole cell is in hand. See [`choose_view`].
    truncated: Option<u64>,
) -> Element {
    // A pure function of the props, so it re-runs exactly when they change —
    // which is what a memo would buy, without the extra signal.
    let view = choose_view(&value, &type_name, truncated);
    let pane = "max-h-64 overflow-auto rounded border border-slate-200 dark:border-slate-800 \
                bg-white dark:bg-slate-900/40 p-1.5";
    rsx! {
        if let Some(note) = view.note {
            div { class: "mb-1",
                Banner { kind: BannerKind::Info, message: note }
            }
        }
        match view.body {
            ViewBody::Null => rsx! {
                span { class: "font-mono text-xs italic text-slate-400 dark:text-slate-600", "NULL" }
            },
            ViewBody::Inline(text) => rsx! {
                span { class: "break-all font-mono text-xs text-slate-900 dark:text-slate-200", "{text}" }
            },
            ViewBody::Text(text) => rsx! {
                pre { class: "max-h-64 overflow-auto whitespace-pre-wrap break-words font-mono text-xs text-slate-900 dark:text-slate-200",
                    "{text}"
                }
            },
            ViewBody::Json(parsed) => rsx! {
                div { class: "{pane} font-mono text-xs leading-5 text-slate-900 dark:text-slate-200",
                    JsonNode { value: parsed, depth: 0 }
                }
            },
            ViewBody::Image { data_url, format, bytes } => rsx! {
                figure {
                    img {
                        class: "max-h-64 max-w-full rounded border border-slate-200 dark:border-slate-800 bg-white object-contain",
                        src: "{data_url}",
                        alt: "{format.label()} image, {human_bytes(bytes as u64)}",
                    }
                    figcaption { class: "mt-1 font-mono text-[11px] text-slate-500 dark:text-slate-400",
                        "{format.label()} · {human_bytes(bytes as u64)}"
                    }
                }
            },
            ViewBody::Hex { dump, shown, total } => rsx! {
                p { class: "mb-1 font-mono text-[11px] text-slate-500 dark:text-slate-400",
                    if (shown as u64) < total {
                        "Binary · showing {human_bytes(shown as u64)} of {human_bytes(total)}"
                    } else {
                        "Binary · {human_bytes(total)}"
                    }
                }
                pre { class: "max-h-64 overflow-auto whitespace-pre font-mono text-[11px] leading-4 text-violet-700 dark:text-violet-300",
                    "{dump}"
                }
            },
        }
    }
}

/// One node of the JSON tree: a scalar on its own line, or a collapsible
/// object/array holding its children.
///
/// Recursive, and keyed by member name, so the collapse state belongs to the
/// node the user clicked rather than to a position in the list.
#[component]
fn JsonNode(
    /// The member name (object key or array index) this node hangs off, or
    /// `None` for the document root.
    label: Option<String>,
    value: serde_json::Value,
    depth: usize,
) -> Element {
    let mut open = use_signal(|| depth < JSON_OPEN_DEPTH);
    let key_class = "text-sky-700 dark:text-sky-300";
    let punct_class = "text-slate-400 dark:text-slate-500";

    let children: Option<Vec<(String, serde_json::Value)>> = match &value {
        serde_json::Value::Object(map) => Some(
            map.iter()
                .map(|(key, child)| (key.clone(), child.clone()))
                .collect(),
        ),
        serde_json::Value::Array(items) => Some(
            items
                .iter()
                .enumerate()
                .map(|(index, child)| (index.to_string(), child.clone()))
                .collect(),
        ),
        _ => None,
    };

    // A scalar, or an empty container: nothing to fold, so no toggle.
    let Some(children) = children.filter(|children| !children.is_empty()) else {
        let (text, class) = json_leaf(&value);
        return rsx! {
            div { class: "break-all",
                if let Some(label) = label {
                    span { class: key_class, "{label}" }
                    span { class: punct_class, ": " }
                }
                span { class, "{text}" }
            }
        };
    };

    let object = value.is_object();
    let (opening, closing) = if object { ("{", "}") } else { ("[", "]") };
    let summary = match (object, children.len()) {
        (true, 1) => "1 key".to_string(),
        (true, n) => format!("{n} keys"),
        (false, 1) => "1 item".to_string(),
        (false, n) => format!("{n} items"),
    };
    let expanded = open();
    rsx! {
        div {
            button {
                class: "flex w-full items-baseline gap-1 rounded text-left hover:bg-slate-100 dark:hover:bg-slate-800",
                aria_expanded: "{expanded}",
                onclick: move |_| open.set(!open()),
                span { class: "w-3 shrink-0 {punct_class}", if expanded { "▾" } else { "▸" } }
                if let Some(label) = label {
                    span { class: key_class, "{label}" }
                    span { class: punct_class, ":" }
                }
                span { class: punct_class,
                    if expanded { "{opening}" } else { "{opening}…{closing}" }
                }
                if !expanded {
                    span { class: "text-slate-500 dark:text-slate-400", "{summary}" }
                }
            }
            if expanded {
                div { class: "ml-1.5 border-l border-slate-200 dark:border-slate-700 pl-2",
                    for (key , child) in children {
                        JsonNode { key: "{key}", label: Some(key.clone()), value: child, depth: depth + 1 }
                    }
                }
                div { class: "ml-4 {punct_class}", "{closing}" }
            }
        }
    }
}

/// A JSON scalar as rendered text plus its colour: strings quoted (so an empty
/// string and a null read apart), everything else as JSON spells it.
fn json_leaf(value: &serde_json::Value) -> (String, &'static str) {
    match value {
        serde_json::Value::Null => (
            "null".to_string(),
            "italic text-slate-400 dark:text-slate-500",
        ),
        serde_json::Value::Bool(flag) => (flag.to_string(), "text-violet-700 dark:text-violet-300"),
        serde_json::Value::Number(number) => {
            (number.to_string(), "text-amber-700 dark:text-amber-300")
        }
        serde_json::Value::String(text) => (
            format!("{:?}", text),
            "text-emerald-700 dark:text-emerald-300",
        ),
        // An empty object/array reaches here as a leaf (nothing to fold).
        serde_json::Value::Array(_) => ("[]".to_string(), "text-slate-400 dark:text-slate-500"),
        serde_json::Value::Object(_) => ("{}".to_string(), "text-slate-400 dark:text-slate-500"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest byte string that carries each format's magic.
    fn png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec()
    }

    #[test]
    fn image_formats_are_named_by_magic_bytes_and_nothing_else() {
        assert_eq!(sniff_image(&png()), Some(ImageFormat::Png));
        assert_eq!(
            sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(sniff_image(b"GIF87a\x01\x00"), Some(ImageFormat::Gif));
        assert_eq!(sniff_image(b"GIF89a\x01\x00"), Some(ImageFormat::Gif));
        assert_eq!(
            sniff_image(b"RIFF\x24\x00\x00\x00WEBPVP8 "),
            Some(ImageFormat::Webp)
        );

        // Near misses stay binary rather than being handed to `<img>`: a
        // wrong-cased PNG signature, a JPEG missing its third byte, a GIF of
        // an unknown version, a RIFF that is a WAV, and anything shorter than
        // the magic it would need.
        assert_eq!(sniff_image(b"\x89png\r\n\x1a\n"), None);
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0x00]), None);
        assert_eq!(sniff_image(b"GIF88a\x01\x00"), None);
        assert_eq!(sniff_image(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff_image(b"RIFF\x24\x00\x00"), None);
        assert_eq!(sniff_image(&png()[..4]), None);
        assert_eq!(sniff_image(b""), None);
        assert_eq!(sniff_image(b"just some text"), None);
        // The magic has to *start* the value, not merely appear in it.
        let mut buried = vec![0u8; 8];
        buried.extend_from_slice(&png());
        assert_eq!(sniff_image(&buried), None);

        // Each format keeps its own MIME type — the data URL is what makes
        // the picture render, so a mislabelled one shows nothing.
        assert_eq!(ImageFormat::Png.mime(), "image/png");
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
        assert_eq!(ImageFormat::Gif.mime(), "image/gif");
        assert_eq!(ImageFormat::Webp.mime(), "image/webp");
    }

    #[test]
    fn a_hex_dump_lays_out_offset_bytes_and_ascii() {
        // Ground truth: `printf 'Hello, world!\n\x00\xff' | hexdump -C`.
        let dump = hex_dump(b"Hello, world!\n\x00\xff");
        assert_eq!(
            dump,
            "00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 0a 00 ff  |Hello, world!...|\n"
        );
        // A short final line keeps the ASCII gutter in the same column, so
        // the two halves still read as a table.
        let short = hex_dump(b"Hi");
        assert_eq!(
            short,
            "00000000  48 69                                             |Hi|\n"
        );
        assert_eq!(short.find('|'), dump.find('|'), "gutter stays aligned");
        // Offsets count bytes, not lines.
        let two_lines = hex_dump(&[0x41; 20]);
        assert!(two_lines.lines().count() == 2);
        assert!(two_lines
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("00000010  41"));
        assert_eq!(hex_dump(b""), "");
    }

    #[test]
    fn an_oversized_image_declines_with_a_reason_instead_of_decoding() {
        // The 200 MB blob of the issue, at the boundary that decides it: one
        // byte over the ceiling is a hex dump plus a sentence, and the same
        // bytes at the ceiling are a picture. Neither answer is a hang.
        let mut over = png();
        over.resize(IMAGE_MAX_BYTES + 1, 0);
        let view = choose_view(&Value::Blob(over.clone()), "bytea", None);
        assert!(
            matches!(view.body, ViewBody::Hex { .. }),
            "an oversized image is never decoded"
        );
        let note = view.note.expect("declining says why");
        assert!(note.contains("PNG"), "{note}");
        assert!(note.contains("4.0 MB"), "names the limit: {note}");

        let mut at_limit = over;
        at_limit.truncate(IMAGE_MAX_BYTES);
        let view = choose_view(&Value::Blob(at_limit), "bytea", None);
        assert!(
            matches!(view.body, ViewBody::Image { .. }),
            "exactly at the limit still renders"
        );
        assert!(view.note.is_none());
    }

    #[test]
    fn a_partly_loaded_blob_is_never_rendered_as_an_image() {
        // A value past `FETCH_CELL_MAX_BYTES` arrives as a prefix. It still
        // carries the magic bytes, so only the caller's `truncated` can tell
        // the viewer that showing it would show part of a picture as the whole.
        let view = choose_view(&Value::Blob(png()), "bytea", Some(200_000_000));
        let ViewBody::Hex { shown, total, .. } = view.body else {
            panic!("a prefix dumps rather than renders");
        };
        // …and the dump reports the stored size, not the prefix's.
        assert_eq!(shown, png().len());
        assert_eq!(total, 200_000_000);
        assert!(view.note.is_some_and(|note| note.contains("PNG")));
        // Complete, the same bytes are a picture with the right MIME type.
        let view = choose_view(&Value::Blob(png()), "bytea", None);
        let ViewBody::Image {
            data_url,
            format,
            bytes,
        } = view.body
        else {
            panic!("a complete PNG renders");
        };
        assert_eq!(format, ImageFormat::Png);
        assert_eq!(bytes, png().len());
        assert!(data_url.starts_with("data:image/png;base64,"), "{data_url}");
    }

    #[test]
    fn a_hex_dump_is_bounded_and_reports_what_it_left_out() {
        let big = vec![0x41; HEX_DUMP_MAX_BYTES + 1024];
        let view = choose_view(&Value::Blob(big.clone()), "blob", None);
        let ViewBody::Hex { dump, shown, total } = view.body else {
            panic!("non-image bytes dump");
        };
        assert_eq!(total, big.len() as u64, "the whole size is still reported");
        assert_eq!(shown, HEX_DUMP_MAX_BYTES, "…but only this much is rendered");
        assert_eq!(dump.lines().count(), HEX_DUMP_MAX_BYTES / HEX_COLUMNS);
        // Nothing was declined, so nothing is claimed to have been.
        assert!(view.note.is_none());
    }

    #[test]
    fn json_documents_get_a_tree_and_everything_else_gets_a_pane() {
        // A document, whatever its column is called: `jsonb`, MySQL `JSON`
        // and an nvarchar holding JSON all reach the same view.
        for type_name in ["jsonb", "json", "nvarchar(max)", "text", ""] {
            let view = choose_view(&Value::Text(r#"{"a":[1,2]}"#.into()), type_name, None);
            assert!(
                matches!(view.body, ViewBody::Json(_)),
                "{type_name} holds a document"
            );
        }
        // Not a document: prose, a JSON scalar (which re-renders as itself),
        // and a document cut off mid-way all stay text.
        for text in ["plain text", "42", "\"hi\"", r#"{"cut": "mid-docu"#] {
            let view = choose_view(&Value::Text(text.into()), "text", None);
            assert_eq!(view.body, ViewBody::Text(text.to_string()), "{text}");
        }
    }

    #[test]
    fn the_declared_type_is_what_separates_a_scalar_from_a_text_pane() {
        // `Value` has no date or numeric variant, so these arrive as text
        // exactly as a `text` column does — the type is the only thing that
        // knows one is a line and the other is a document.
        for type_name in ["timestamp", "date", "numeric", "uuid", "integer"] {
            assert_eq!(
                choose_view(&Value::Text("2026-08-10".into()), type_name, None).body,
                ViewBody::Inline("2026-08-10".into()),
                "{type_name}"
            );
        }
        for type_name in ["text", "varchar(200)", "xml", ""] {
            assert_eq!(
                choose_view(&Value::Text("2026-08-10".into()), type_name, None).body,
                ViewBody::Text("2026-08-10".into()),
                "{type_name}"
            );
        }
        // Numbers the driver decoded as numbers never reach the pane either.
        assert_eq!(
            choose_view(&Value::Integer(-42), "bigint", None).body,
            ViewBody::Inline("-42".into())
        );
        assert_eq!(choose_view(&Value::Null, "text", None).body, ViewBody::Null);
    }

    #[test]
    fn a_partly_loaded_document_is_shown_as_text_rather_than_parsed() {
        // A prefix can parse into a *smaller* valid document — `{"a":1}` is
        // the first seven bytes of `{"a":1}...`-shaped input in plenty of
        // real rows — which would drop what was cut without saying so.
        let view = choose_view(&Value::Text(r#"{"a":1}"#.into()), "jsonb", Some(4_000));
        assert_eq!(view.body, ViewBody::Text(r#"{"a":1}"#.into()));
        assert!(view.note.is_none(), "the caller states the truncation once");
    }

    #[test]
    fn oversized_and_overgrown_documents_decline_to_text_with_a_reason() {
        // Past the parse ceiling nothing is parsed at all.
        let huge = format!(r#"{{"a":"{}"}}"#, "x".repeat(JSON_MAX_BYTES));
        let view = choose_view(&Value::Text(huge.clone()), "jsonb", None);
        assert_eq!(view.body, ViewBody::Text(huge));
        let note = view.note.expect("declining says why");
        assert!(note.contains("1.0 MB"), "names the limit: {note}");

        // Under it but too many nodes for a tree: pretty-printed text, and a
        // note that says so rather than a silently different view.
        let wide: Vec<u32> = (0..JSON_TREE_MAX_NODES as u32).collect();
        let text = serde_json::to_string(&wide).unwrap();
        assert!(text.len() < JSON_MAX_BYTES, "parses, but is a big tree");
        let view = choose_view(&Value::Text(text), "jsonb", None);
        let ViewBody::Text(shown) = view.body else {
            panic!("a document this wide is not a tree");
        };
        assert!(shown.starts_with("[\n  0,"), "still pretty-printed");
        assert!(view
            .note
            .is_some_and(|note| note.contains(&JSON_TREE_MAX_NODES.to_string())));

        // One node under the limit is a tree — the boundary is a decision,
        // not a slogan.
        let narrow: Vec<u32> = (0..JSON_TREE_MAX_NODES as u32 - 1).collect();
        let view = choose_view(
            &Value::Text(serde_json::to_string(&narrow).unwrap()),
            "jsonb",
            None,
        );
        assert!(matches!(view.body, ViewBody::Json(_)));
    }

    #[test]
    fn json_nodes_are_counted_as_the_tree_renders_them() {
        let count = |text: &str| json_node_count(&serde_json::from_str(text).unwrap());
        // The document itself is a node, and so is every scalar under it.
        assert_eq!(count("1"), 1);
        assert_eq!(count("[]"), 1);
        assert_eq!(count("[1,2,3]"), 4);
        assert_eq!(count(r#"{"a":1,"b":{"c":[1,2]}}"#), 1 + 1 + 1 + 1 + 2);
    }
}
