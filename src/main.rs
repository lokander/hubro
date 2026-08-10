use dioxus::desktop::tao::dpi::{LogicalPosition, LogicalSize};
use dioxus::desktop::tao::event::Event;
use dioxus::desktop::{Config, WindowBuilder};
use hubro::cli::{self, CliError, Invocation, Startup};
use hubro::config::{default_settings_path, load_settings};
use hubro::ui::App;

fn main() {
    // Arguments are answered before anything else happens (FRE-114): `--help`
    // and `--version` print and exit, and a bad command line reports on stderr
    // — none of the three should cost the user a window.
    let target = match cli::parse(std::env::args_os().skip(1)) {
        Ok(Invocation::Help) => {
            print!("{}", cli::HELP);
            return;
        }
        Ok(Invocation::Version) => {
            println!("hubro {}", cli::VERSION);
            return;
        }
        Ok(Invocation::Run(target)) => target,
        Err(err) => fail(&err),
    };
    // A file we can already tell isn't a database is reported here rather than
    // in the app: from a terminal that is where the user is looking, and a
    // window whose entire content is "not a SQLite database" helps nobody. A
    // *server* URL never fails here — reachability is not knowable without the
    // network, so those always launch and report in the UI (see
    // `OpenTarget::preflight`).
    if let Some(target) = &target {
        if let Err(err) = target.preflight() {
            fail(&err);
        }
    }

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
        .with_cfg(
            Config::new()
                .with_window(window)
                .with_custom_event_handler(|event, _| {
                    // macOS delivers a double-clicked file as an `open` Apple
                    // Event, not in argv, so the file association depends on
                    // this arm; tao raises `Opened` on no other platform, where
                    // the same file arrives above as `target`. Queued rather
                    // than handled here: the event loop runs outside any Dioxus
                    // scope, and at launch it beats the first render.
                    if let Event::Opened { urls } = event {
                        for url in urls {
                            cli::deliver_opened_url(url);
                        }
                    }
                }),
        )
        // Read once by the UI, which opens it after the session restore so the
        // named database is the tab in front (see `ui::shell::Shell`).
        .with_context(Startup(target))
        .launch(App);
}

/// Reports a command-line problem and exits without opening a window.
///
/// `CliError`'s `Display` redacts any password in what it echoes, which is why
/// nothing here formats the raw argument itself.
fn fail(err: &CliError) -> ! {
    eprintln!("hubro: {err}");
    if err.is_usage() {
        eprintln!("Try `hubro --help` for usage.");
    }
    std::process::exit(err.exit_code())
}
