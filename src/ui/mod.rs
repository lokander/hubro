//! UI components. `App` is the root; `main.rs` only configures the window
//! and launches it.

mod shell;
mod state;

pub use state::{tab_title, ActiveView, AppState};

use dioxus::prelude::*;

use shell::Shell;

const FAVICON: Asset = asset!("/assets/favicon.ico");
const MAIN_CSS: Asset = asset!("/assets/main.css");
const TAILWIND_CSS: Asset = asset!("/assets/tailwind.css");

#[component]
pub fn App() -> Element {
    use_context_provider(AppState::new);
    rsx! {
        document::Link { rel: "icon", href: FAVICON }
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        document::Link { rel: "stylesheet", href: TAILWIND_CSS }
        Shell {}
    }
}
