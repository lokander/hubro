use dioxus::desktop::tao::dpi::{LogicalPosition, LogicalSize};
use dioxus::desktop::{Config, WindowBuilder};
use hubro::config::{default_settings_path, load_settings};
use hubro::ui::App;

fn main() {
    // Restore the last window geometry (FRE-30), falling back to the historical
    // 1200x800 default when nothing is saved. `sanitized` clamps a corrupt
    // size/position so a bad settings file can't produce an unusable window.
    let geometry = default_settings_path()
        .map(|path| load_settings(&path).window.unwrap_or_default())
        .unwrap_or_default()
        .sanitized();
    let mut window = WindowBuilder::new()
        .with_title("Hubro")
        .with_inner_size(LogicalSize::new(geometry.width, geometry.height));
    if let (Some(x), Some(y)) = (geometry.x, geometry.y) {
        window = window.with_position(LogicalPosition::new(x, y));
    }
    if geometry.maximized {
        window = window.with_maximized(true);
    }
    dioxus::LaunchBuilder::new()
        .with_cfg(Config::new().with_window(window))
        .launch(App);
}
