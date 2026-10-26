//! UI components. `App` is the root; `main.rs` only configures the window
//! and launches it.

mod classes;
mod connections;
pub mod editing;
mod editor;
mod filter;
mod grid;
mod history_panel;
mod icons;
mod import;
mod js;
mod notice;
mod saved_panel;
mod schema;
mod schema_edit;
mod selection;
mod shell;
mod sidebar;
pub mod stage;
mod state;
#[cfg(test)]
mod test_dom;
mod text_field;

pub use stage::TableStage;
pub use state::{tab_title, ActiveView, AppState, TableRef};

use dioxus::prelude::*;

use shell::Shell;

const FAVICON: Asset = asset!("/assets/favicon.ico");
const CODEMIRROR_JS: Asset = asset!("/assets/codemirror.js");
const MAIN_CSS: Asset = asset!("/assets/main.css");
const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");

#[component]
pub fn App() -> Element {
    use_context_provider(AppState::new);
    rsx! {
        document::Link { rel: "icon", href: FAVICON }
        document::Script { src: CODEMIRROR_JS }
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        document::Link { rel: "stylesheet", href: TAILWIND_CSS }
        Shell {}
    }
}

/// Asks where to save an export, offering `{stem}.{extension}` under a filter
/// for the format alone. `None` when the dialog is cancelled.
///
/// Only the dialog: whoever awaits it must already be outliving the component
/// that asked (see the grid's `every_dialog_task_outlives_its_component`).
async fn pick_export_path(
    title: &str,
    stem: &str,
    format: crate::db::ExportFormat,
) -> Option<std::path::PathBuf> {
    let ext = format.extension();
    rfd::AsyncFileDialog::new()
        .set_title(title)
        .set_file_name(format!("{stem}.{ext}"))
        .add_filter(format.filter_label(), &[ext])
        .save_file()
        .await
        .map(|file| file.path().to_path_buf())
}
