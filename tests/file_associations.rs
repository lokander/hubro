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
const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
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

/// `Info.plist` with its XML comments removed, so prose mentioning a tag
/// cannot be mistaken for one. The file's header is a long comment that names
/// keys and quotes dx's source, which is exactly the material a scanner would
/// otherwise read as markup.
fn plist_without_comments() -> String {
    let mut source = String::with_capacity(INFO_PLIST.len());
    let mut rest = INFO_PLIST;
    while let Some(start) = rest.find("<!--") {
        source.push_str(&rest[..start]);
        rest = match rest[start..].find("-->") {
            Some(end) => &rest[start + end + "-->".len()..],
            None => "",
        };
    }
    source.push_str(rest);
    source
}

/// Reads a `plist` `<key>name</key><string>value</string>` pair.
///
/// A short scanner instead of a plist parser: the file is checked in, its
/// shape is known, and the alternative is a dependency in the shipped tree for
/// one test. (A parser is not merely unnecessary here but unavailable —
/// `--package-types` in the header comment makes the file malformed XML, which
/// strict parsers reject and macOS accepts.)
///
/// Two things this deliberately does *not* do, both of which let a stale
/// version pass as current: read comments (a `<string>` inside the header
/// prose would answer for the real key), and search forward for a `<string>`
/// anywhere later in the file (a key given a different value type would answer
/// with some *other* key's value). The value has to be the element
/// immediately after the key.
fn plist_string(key: &str) -> String {
    let source = plist_without_comments();
    let marker = format!("<key>{key}</key>");
    let after = source
        .split_once(&marker)
        .unwrap_or_else(|| panic!("Info.plist has no <key>{key}</key>"))
        .1
        .trim_start();
    let value = after.strip_prefix("<string>").unwrap_or_else(|| {
        panic!("{key} is not immediately followed by a <string> — is it a different value type?")
    });
    let close = value
        .find("</string>")
        .unwrap_or_else(|| panic!("{key}'s <string> is never closed"));
    value[..close].to_string()
}

/// The keys of the plist's outermost `<dict>`, in document order — the ones
/// dx would have generated, as opposed to those nested inside
/// `CFBundleDocumentTypes`.
///
/// Depth-tracked rather than "every key up to the first nested one". That
/// shortcut looked equivalent and was not: it stopped reading at the
/// document-types array, so a key appended *after* the array was invisible to
/// the very guard that exists to notice a key appearing or going missing.
/// Comments are stripped first, so prose mentioning a tag cannot be mistaken
/// for one.
fn top_level_plist_keys() -> Vec<String> {
    let source = plist_without_comments();

    let mut keys = Vec::new();
    let mut depth = 0usize;
    let mut rest = source.as_str();
    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('>') else { break };
        match &rest[..close] {
            "dict" | "array" => depth += 1,
            "/dict" | "/array" => depth = depth.saturating_sub(1),
            // Depth 1 is directly inside the root <dict>.
            "key" if depth == 1 => {
                if let Some(end) = rest[close + 1..].find("</key>") {
                    keys.push(rest[close + 1..close + 1 + end].to_string());
                }
            }
            _ => {}
        }
        rest = &rest[close + 1..];
    }
    keys
}

/// The top-level jobs of `release.yml` as `(id, body)`, the body being every
/// line up to the next job. A scanner rather than a YAML parser, for the same
/// reason as the plist one: one property of one checked-in file does not repay
/// a dependency.
fn release_jobs() -> Vec<(String, String)> {
    let after_jobs = RELEASE_WORKFLOW
        .split_once("\njobs:\n")
        .expect("release.yml declares no jobs")
        .1;
    let mut jobs: Vec<(String, String)> = Vec::new();
    for line in after_jobs.lines() {
        let starts_a_job = line.starts_with("  ")
            && !line.starts_with("   ")
            && !line.trim_start().starts_with('#')
            && line.trim_end().ends_with(':');
        if starts_a_job {
            jobs.push((line.trim().trim_end_matches(':').to_string(), String::new()));
        } else if let Some((_, body)) = jobs.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    jobs
}

/// The job ids a job's `needs:` names, in either the inline (`needs: [a, b]`,
/// `needs: a`) or block (`needs:` then `- a`) form — a reformat between the
/// two is a reformat, not a change of meaning, and should not read as one.
fn declared_needs(body: &str) -> Vec<String> {
    let mut lines = body.lines().skip_while(|line| !line.trim().starts_with("needs:"));
    let Some(first) = lines.next() else {
        return Vec::new();
    };
    let mut value = first.trim().trim_start_matches("needs:").trim().to_string();
    for line in lines {
        match line.trim().strip_prefix('-') {
            Some(item) => value.push_str(&format!(" {item}")),
            None => break,
        }
    }
    value
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
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
    //
    // The assignment is matched in full, not just the path: a path that
    // appears only in a comment, or one assigned to a misspelled key, would
    // satisfy a bare substring search while shipping nothing — and the failure
    // message names the key, so it had better be the key that was checked.
    for (path, assignment) in [
        (
            "packaging/linux/hubro.desktop.hbs",
            r#"desktop_template = "packaging/linux/hubro.desktop.hbs""#,
        ),
        (
            "packaging/linux/postinst",
            r#"post_install_script = "packaging/linux/postinst""#,
        ),
        (
            "packaging/linux/postrm",
            r#"post_remove_script = "packaging/linux/postrm""#,
        ),
        (
            "packaging/macos/Info.plist",
            r#"info_plist_path = "packaging/macos/Info.plist""#,
        ),
        (
            "packaging/windows/file-associations.wxs",
            r#"fragment_paths = ["packaging/windows/file-associations.wxs"]"#,
        ),
    ] {
        assert!(
            std::path::Path::new(path).exists(),
            "{path} is referenced but missing"
        );
        assert!(
            DIOXUS_TOML.contains(assignment),
            "Dioxus.toml has no `{assignment}`, so `dx bundle` would never read \
             {path}"
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
fn the_info_plist_declares_exactly_the_keys_dx_would_have_written() {
    // Re-deriving *values* leaves a whole class of drift invisible: a key dx
    // writes and this mirror lacks is not a wrong value anywhere, it is an
    // absence, and nothing that checks values can see one. So the key set is
    // asserted outright, in the order the file declares it.
    //
    // Taken from `create_macos_info_plist` in dioxus-cli/src/bundler/macos.rs
    // (dx 0.7.9, byte-identical in 0.7.10) — the generator behind `dx bundle
    // --package-types macos`. dioxus-cli's other plist generators (widget
    // extensions, frameworks, iOS) emit different keys, including
    // CFBundleSupportedPlatforms; none of them runs for this bundle.
    let expected = [
        "CFBundleDevelopmentRegion",
        "CFBundleDisplayName",
        "CFBundleExecutable",
        "CFBundleIconFile",
        "CFBundleIdentifier",
        "CFBundleInfoDictionaryVersion",
        "CFBundleName",
        "CFBundlePackageType",
        "CFBundleShortVersionString",
        "CFBundleVersion",
        "LSMinimumSystemVersion",
        "LSApplicationCategoryType",
        "NSHumanReadableCopyright",
        "NSHighResolutionCapable",
        // The only key that is ours rather than dx's — the whole reason this
        // file exists. Its nested keys are checked by the test below.
        "CFBundleDocumentTypes",
    ];
    let keys = top_level_plist_keys();
    let top_level: Vec<&str> = keys.iter().map(String::as_str).collect();
    assert_eq!(
        top_level, expected,
        "the mirror no longer matches the key set dx writes — add the missing \
         key, or drop the extra one"
    );

    // dx emits these two only when given config this project does not set, so
    // their absence is correct *because* of what Dioxus.toml omits. If that
    // ever changes, the mirror has to gain the key, and this says so.
    for (key, setting) in [
        ("ITSAppUsesNonExemptEncryption", "provider_short_name"),
        ("NSAppTransportSecurity", "exception_domain"),
    ] {
        assert!(
            !DIOXUS_TOML.contains(setting),
            "Dioxus.toml now sets {setting}, so dx would write {key} and the \
             mirror must gain it"
        );
    }
}

#[test]
fn the_info_plist_records_the_dx_version_it_was_mirrored_from() {
    // The hole a value check cannot cover: a *dx upgrade* adding a key. The
    // mirror cannot detect that on its own, so it is pinned to the version it
    // was taken from, and that version is pinned to the one the release
    // workflow actually bundles with. Upgrading dx therefore fails here until
    // someone re-reads `create_macos_info_plist` and moves the note — which
    // turns "undetectable" into "detected at exactly the moment it matters".
    let pinned = RELEASE_WORKFLOW
        .lines()
        .find_map(|line| line.trim().strip_prefix("DX_VERSION:"))
        .expect("release.yml pins no DX_VERSION")
        .trim()
        .trim_matches('"')
        .to_string();
    assert!(
        INFO_PLIST.contains(&format!("MIRRORED FROM dx {pinned},")),
        "packaging/macos/Info.plist says which dx version it mirrors, and \
         release.yml bundles with {pinned} — they disagree, so the mirror was \
         not re-derived when dx moved"
    );
}

#[test]
fn nothing_bundles_or_publishes_before_the_version_check() {
    // The version a release *claims* is the tag; the version its artifacts
    // *carry* comes from Cargo.toml (Linux, Windows) and Info.plist (macOS,
    // which dx copies verbatim). release.yml's `verify` job is what stops
    // those three disagreeing — v0.6.0's tag names a commit whose Cargo.toml
    // still said 0.5.0, and shipped saying so.
    //
    // The job is only a guard while every job that produces or publishes an
    // artifact waits for it. Deleting three `needs:` lines silently restores
    // the old behaviour, and the test above would not notice — so this reads
    // the wire rather than the spot: every bundle job needs `verify`, and
    // `publish` needs every bundle job, both derived from the file so a
    // fourth platform has to be wired in rather than merely added.
    let jobs = release_jobs();
    assert!(
        jobs.iter().any(|(id, _)| id == "verify"),
        "release.yml has no `verify` job (FRE-166)"
    );

    let bundlers: Vec<&(String, String)> = jobs
        .iter()
        .filter(|(_, body)| body.contains("name: Bundle "))
        .collect();
    assert!(
        !bundlers.is_empty(),
        "no job in release.yml is named `Bundle …`, so this test now checks \
         nothing — were the bundle jobs renamed?"
    );

    for (id, body) in &bundlers {
        assert!(
            declared_needs(body).iter().any(|need| need == "verify"),
            "release.yml's `{id}` job does not need `verify`, so a tag whose \
             version disagrees with Cargo.toml or Info.plist would bundle and \
             publish under it anyway (FRE-166)"
        );
    }

    let (_, publish) = jobs
        .iter()
        .find(|(id, _)| id == "publish")
        .expect("release.yml has no `publish` job");
    let published = declared_needs(publish);
    for (id, _) in &bundlers {
        assert!(
            published.contains(id),
            "release.yml's `publish` job does not need `{id}`, so it can \
             publish a release without that platform's artifacts"
        );
    }
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
