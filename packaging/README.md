# Packaging: file associations

These files make a double-clicked database open in hubro (FRE-114). They are
read by `dx bundle`, wired up in `Dioxus.toml`, and checked by
`tests/file_associations.rs` — which is the only thing that runs them, since
none of them is exercised by a normal build.

The extensions hubro claims are `.db`, `.sqlite` and `.sqlite3`, declared once
in code as `cli::DATABASE_EXTENSIONS`. **`.db` is contested** — plenty of
unrelated software uses it — so on every platform hubro registers as *a*
handler and never takes the type over.

| File | Platform | Read by |
| --- | --- | --- |
| `linux/hubro.desktop.hbs` | deb, rpm, AppImage | `dx` renders the `.desktop` entry from it |
| `linux/postinst`, `linux/postrm` | deb | dpkg, to refresh the desktop database |
| `macos/Info.plist` | dmg / `.app` | LaunchServices |
| `windows/file-associations.wxs` | msi | WiX, at install time |

## Linux declares a type, not extensions

The Linux entry lists `MimeType=application/vnd.sqlite3;application/x-sqlite3;`
and nothing else. It installs **no** shared-mime-info glob rules, which is a
decision rather than an omission, and one worth not re-litigating:

`application/vnd.sqlite3` is already a standard shared-mime-info type, and it
carries content magic for the `SQLite format 3` header. Measured on a
CachyOS/KDE box against a scratch `XDG_DATA_HOME`, with no rules from hubro at
all:

| file | resolved type |
| --- | --- |
| a real database named `real.db` | `application/vnd.sqlite3` |
| a real database named `real.sqlite` | `application/vnd.sqlite3` |
| a real database named `real.sqlite3` | `application/vnd.sqlite3` |
| a text file named `notes.db` | `text/plain` |

All three extensions already work, by content, and a plain file called `.db` is
correctly left alone. Adding `<glob pattern="*.db">` — the obvious way to
"register an extension" — made it *worse*: a name match outranks content
sniffing, so with that rule installed `notes.db` resolved to
`application/vnd.sqlite3`, and a text file would have been offered to hubro
(which would then have refused it). Glob weights do not help; they only break
ties between competing globs, not between a glob and the magic.

So the whole Linux association is the `MimeType` line, plus `Exec=… %f` to pass
the path. `update-desktop-database` in `postinst` is what makes the line take
effect — dx's deb has no dpkg triggers, so nothing else would rebuild the
index.

Known limit of typing by content: a **zero-length** `.db` file resolves to
`application/x-zerosize`, so a file manager will not offer hubro for it. hubro
itself opens one (SQLite treats it as an empty database), just not from a
double-click.

The AppImage installs nothing outside itself, so there the association depends
on whatever integrates the AppImage's `.desktop` entry; the deb and rpm install
it properly.

## macOS replaces the whole Info.plist

`[bundle.macos] info_plist_path` does not merge — dx copies the file in place
of the plist it would have generated. `macos/Info.plist` therefore repeats
every key dx writes, and only `CFBundleDocumentTypes` is ours.

That makes it a hand-maintained copy of a generated file, which goes stale
silently: bump the crate version and the bundle would keep reporting the old
one. `tests/file_associations.rs` re-derives each mirrored value from
`Cargo.toml` and `Dioxus.toml` and fails on drift. What it cannot know is dx
growing a *new* key — **when upgrading dx, re-read `create_macos_info_plist` in
the dioxus-cli source and mirror anything new.**

`.sqlite`/`.sqlite3` are declared `LSHandlerRank = Default`; `.db` is a
separate entry at `Alternate`, so hubro stays available under "Open With"
without presenting itself as the owner of the extension.

## Windows adds registry values, never a default

The WiX fragment writes a `Hubro.Database` ProgID and adds it to each
extension's `OpenWithProgids` list under `HKLM\Software\Classes` (dx's MSI is
`InstallScope="perMachine"`). It never writes an extension key's own default
value — that is what *takes over* a file type, and Windows 8 and later refuse
an installer-set default anyway. The user picks the default; this makes hubro
something they can pick.

The fragment is inert unless the installed feature references its component
group, which is why `Dioxus.toml` carries both `fragment_paths` and
`component_group_refs`. The NSIS `setup.exe` registers nothing — a deliberate
limit of FRE-114, not an oversight.

## What is unverified

The macOS and Windows declarations have never been run: they were written on
Linux, from the dioxus-cli bundler source, and the offline checks above are the
whole of their verification. The Linux behaviour in the table was measured on
this machine.
