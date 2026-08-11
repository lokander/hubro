//! UI components. `App` is the root; `main.rs` only configures the window
//! and launches it.

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
