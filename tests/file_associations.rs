//! Guards the file-association packaging (FRE-114) against the code and the
//! build config it claims to agree with.
//!
//! Three files in `packaging/` say which databases hubro opens — a `.desktop`
//! template, a macOS `Info.plist`, a WiX fragment — and none of them is
//! exercised by anything a developer runs. They are read by `dx bundle` on a
//! release runner, and the two that matter most are read only by an operating
//! system this project is not developed on. A wrong value in any of them shows
//! up as "double-clicking does nothing", months later, on someone else's
//! machine.
//!
//! So this file makes the claims checkable offline, in the shape
//! `support_matrix.rs` uses for the README: the extension list lives in the
//! code as [`DATABASE_EXTENSIONS`], and the two declarations that name
//! extensions — macOS and Windows — must match it. (Linux names a MIME type
//! instead, and deliberately claims no extension at all; the test below pins
//! that decision.) `Info.plist` gets more than that, because it is a
//! **hand-maintained copy of a generated file** — dx replaces its generated
//! plist with ours rather than merging — so every value copied out of
//! `Cargo.toml` and `Dioxus.toml` is re-derived here and compared.
//!
//! What this cannot check, and what therefore stays a claim: that macOS
//! honours the document types, that Windows Explorer offers hubro, that dx's
//! generated plist has not grown a key since this copy was taken. Those need
//! the platform. This checks everything that does not.

use hubro::cli::DATABASE_EXTENSIONS;

const DIOXUS_TOML: &str = include_str!("../Dioxus.toml");
const CARGO_TOML: &str = include_str!("../Cargo.toml");
const DESKTOP_TEMPLATE: &str = include_str!("../packaging/linux/hubro.desktop.hbs");
const INFO_PLIST: &str = include_str!("../packaging/macos/Info.plist");
const WIX_FRAGMENT: &str = include_str!("../packaging/windows/file-associations.wxs");

/// The MIME type the whole Linux association hangs off, and its alias — both
/// standard shared-mime-info types whose content magic already recognizes a
/// SQLite file under any of hubro's extensions. Registering the *type* is
/// therefore all Linux needs; see packaging/README.md for the measurement that
/// ruled out adding extension globs.
const SQLITE_MIME: &str = "application/vnd.sqlite3";
const SQLITE_MIME_ALIAS: &str = "application/x-sqlite3";

/// Reads a `plist` `<key>name</key><string>value</string>` pair.
///
/// A five-line scanner instead of a plist parser: the file is checked in, its
/// shape is known, and the alternative is a dependency in the shipped tree for
/// one test.
fn plist_string(key: &str) -> String {
    let marker = format!("<key>{key}</key>");
    let after = INFO_PLIST
        .split_once(&marker)
        .unwrap_or_else(|| panic!("Info.plist has no <key>{key}</key>"))
        .1;
    let open = after
        .find("<string>")
        .unwrap_or_else(|| panic!("{key} has no string value"));
    let close = after
        .find("</string>")
        .unwrap_or_else(|| panic!("{key} has no string value"));
    assert!(
        open < close,
        "{key} is not followed by a string — is it a different value type?"
    );
    after[open + "<string>".len()..close].to_string()
}

/// The value of a top-level `key = "value"` line in a TOML file. Same
/// reasoning as `plist_string`: these files are checked in and simple, and the
/// point is to re-derive a value, not to parse TOML in general.
fn toml_string(source: &str, key: &str) -> String {
    source
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(key)?.trim_start().strip_prefix('='))
        .unwrap_or_else(|| panic!("no `{key} = …` line found"))
        .trim()
        .trim_matches('"')
        .to_string()
}

#[test]
fn every_packaging_file_is_wired_into_the_build() {
    // A packaging file dx is never told about is a file that ships nothing,
    // and looks exactly like one that works.
    for (path, setting) in [
        ("packaging/linux/hubro.desktop.hbs", "desktop_template"),
        ("packaging/linux/postinst", "post_install_script"),
        ("packaging/linux/postrm", "post_remove_script"),
        ("packaging/macos/Info.plist", "info_plist_path"),
        ("packaging/windows/file-associations.wxs", "fragment_paths"),
    ] {
        assert!(
            std::path::Path::new(path).exists(),
            "{path} is referenced but missing"
        );
        assert!(
            DIOXUS_TOML.contains(path),
            "{path} exists but nothing in Dioxus.toml ({setting}) points at it, \
             so `dx bundle` would never read it"
        );
    }
    // The WiX fragment installs nothing unless the feature references its
    // component group — the one wiring mistake that leaves a green build and
    // an MSI with no associations.
    assert!(
        WIX_FRAGMENT.contains(r#"ComponentGroup Id="HubroFileAssociations""#),
        "the fragment must define the component group Dioxus.toml references"
    );
    assert!(
        DIOXUS_TOML.contains(r#"component_group_refs = ["HubroFileAssociations"]"#),
        "the fragment's component group must be referenced, or it installs nothing"
    );
}

#[test]
fn the_desktop_entry_passes_the_file_and_claims_the_type() {
    // `%f` is what makes a double-clicked path reach argv at all — and it must
    // be the single-file code, since hubro opens one database per window and
    // would reject a launch that named several.
    let exec = DESKTOP_TEMPLATE
        .lines()
        .find(|line| line.starts_with("Exec="))
        .expect("the template has no Exec line");
    assert_eq!(exec, "Exec={{exec}} %f");
    // Registering the type is the whole Linux association: the system already
    // recognizes a SQLite file by content under any of hubro's extensions, so
    // the only missing link is an application that claims the type. The alias
    // is listed too, because a desktop that resolves a file to
    // `application/x-sqlite3` looks up that spelling and no other.
    let mime = DESKTOP_TEMPLATE
        .lines()
        .find(|line| line.starts_with("MimeType="))
        .expect("the template declares no MIME types");
    for declared in [SQLITE_MIME, SQLITE_MIME_ALIAS] {
        assert!(
            mime.contains(&format!("{declared};")),
            "{declared} is not declared, so hubro is not offered as a handler \
             for files resolved to it"
        );
    }
    // dx does not merge this template with its default either: whatever is
    // dropped here is dropped from the shipped .desktop file.
    for key in [
        "Categories=",
        "Comment=",
        "Icon=",
        "Name=",
        "Terminal=false",
        "Type=Application",
    ] {
        assert!(
            DESKTOP_TEMPLATE.contains(key),
            "{key} is in dx's default template and would be lost by overriding it"
        );
    }
}

#[test]
fn linux_claims_no_extension_of_its_own() {
    // The measured conclusion recorded in packaging/README.md, kept from being
    // quietly undone: hubro must not install shared-mime-info glob rules. A
    // glob outranks content sniffing, so claiming `*.db` by name retypes every
    // plain file that happens to be called `.db` as a SQLite database — while
    // adding nothing, because the content magic already recognizes real
    // databases under all three of hubro's extensions.
    for entry in std::fs::read_dir("packaging/linux").expect("packaging/linux") {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            !text.contains("<glob"),
            "{} declares MIME globs; packaging/README.md records why hubro \
             deliberately declares none",
            path.display()
        );
    }
    assert!(
        !DIOXUS_TOML.contains("/usr/share/mime/packages"),
        "installing a shared-mime-info package file was measured to do harm; \
         see packaging/README.md"
    );
}

#[test]
fn the_info_plist_still_mirrors_the_values_dx_would_have_generated() {
    // dx *replaces* its generated Info.plist with this file, so every value it
    // would have written is copied by hand — and a copy goes stale in silence.
    // Each of these is re-derived from the file dx reads it from.
    assert_eq!(
        plist_string("CFBundleShortVersionString"),
        toml_string(CARGO_TOML, "version"),
        "the bundle version has drifted from Cargo.toml"
    );
    assert_eq!(
        plist_string("CFBundleVersion"),
        toml_string(CARGO_TOML, "version"),
    );
    assert_eq!(
        plist_string("CFBundleIdentifier"),
        toml_string(DIOXUS_TOML, "identifier"),
        "the bundle identifier has drifted from Dioxus.toml (it keys the app's \
         config directory and keychain items)"
    );
    assert_eq!(
        plist_string("LSMinimumSystemVersion"),
        toml_string(DIOXUS_TOML, "minimum_system_version"),
    );
    assert_eq!(
        plist_string("NSHumanReadableCopyright"),
        toml_string(DIOXUS_TOML, "copyright"),
    );
    // dx derives these two from the crate name: the executable keeps it,
    // the bundle (and so the .icns it looks for) is its PascalCase form. Get
    // the executable wrong and the .app does not launch at all.
    let crate_name = toml_string(CARGO_TOML, "name");
    assert_eq!(plist_string("CFBundleExecutable"), crate_name);
    let product = format!(
        "{}{}",
        crate_name[..1].to_uppercase(),
        crate_name[1..].to_lowercase()
    );
    assert_eq!(plist_string("CFBundleName"), product);
    assert_eq!(plist_string("CFBundleDisplayName"), product);
    assert_eq!(plist_string("CFBundleIconFile"), format!("{product}.icns"));
    assert_eq!(plist_string("CFBundlePackageType"), "APPL");
    assert!(
        INFO_PLIST.contains("<key>NSHighResolutionCapable</key>\n\t<true/>"),
        "dx sets NSHighResolutionCapable; without it the app renders at 1x"
    );
}

#[test]
fn the_document_types_cover_the_extensions_and_only_claim_db_as_an_alternate() {
    let types = INFO_PLIST
        .split_once("<key>CFBundleDocumentTypes</key>")
        .expect("Info.plist declares no document types")
        .1;
    for extension in DATABASE_EXTENSIONS {
        assert!(
            types.contains(&format!("<string>{extension}</string>")),
            ".{extension} is not among the document types, so macOS will not \
             offer hubro for it"
        );
    }
    // The `.db` entry must be the Alternate one. Splitting on the extension
    // finds the dict it belongs to; its handler rank is the last one declared
    // before it.
    let (before_db, _) = types
        .split_once("<string>db</string>")
        .expect("no .db document type");
    let rank = before_db
        .rsplit_once("<key>LSHandlerRank</key>")
        .expect(".db's document type declares no handler rank")
        .1;
    assert!(
        rank.contains("<string>Alternate</string>"),
        ".db must be claimed as an Alternate handler — it is a contested \
         extension and hubro must not present itself as its owner"
    );
}

#[test]
fn the_windows_registry_entries_add_a_handler_rather_than_a_default() {
    for extension in DATABASE_EXTENSIONS {
        assert!(
            WIX_FRAGMENT.contains(&format!(r"Software\Classes\.{extension}\OpenWithProgids")),
            ".{extension} gets no OpenWithProgids entry, so hubro will not be \
             offered for it"
        );
        // Writing the extension key's own default value is what *takes over* a
        // file type. Nothing here may do it — least of all for `.db`.
        assert!(
            !WIX_FRAGMENT.contains(&format!(r#"Key="Software\Classes\.{extension}""#)),
            ".{extension}'s own key is written, which would claim the type as \
             the default handler"
        );
    }
    assert!(
        WIX_FRAGMENT.contains(r#"Root="HKLM""#),
        "dx's MSI installs per-machine, so the classes belong in HKLM"
    );
}
