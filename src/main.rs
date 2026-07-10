use dataview::ui::App;
use dioxus::desktop::tao::dpi::LogicalSize;
use dioxus::desktop::{Config, WindowBuilder};

fn main() {
    let window = WindowBuilder::new()
        .with_title("dataview")
        .with_inner_size(LogicalSize::new(1200.0, 800.0));
    dioxus::LaunchBuilder::new()
        .with_cfg(Config::new().with_window(window))
        .launch(App);
}
