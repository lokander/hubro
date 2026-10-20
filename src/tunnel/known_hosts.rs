//! Reading OpenSSH known_hosts files for host-key verification (FRE-289).
//!
//! Replaces russh's `known_host_keys_path` for reading; russh still appends
//! trusted keys. russh's reader failed a whole file on one undecodable line for
//! the host or on any byte that was not UTF-8, ignored `@revoked`, split only
//! on spaces, and matched host patterns literally. Each of those turned a host
//! the user had recorded into Unknown, which is a trust prompt. This follows
//! OpenSSH's `hostfile.c` instead, and every rule below was checked against the
//! real OpenSSH 10.5 client with `StrictHostKeyChecking=yes`:
//!
//! - An unparsable line is skipped by itself and records nothing, even when it
//!   names the host. A host recorded *only* by such lines is Unknown.
//! - `@revoked` lines whose patterns match the host collect revoked keys, which
//!   are refused even when the same key is also listed plainly.
//! - `@cert-authority` lines vouch only for certificates, which hubro refuses,
//!   so they record nothing. Lines with an unknown marker are skipped.
//! - Fields are separated by any run of spaces or tabs.
//! - A file that cannot be read contributes nothing; OpenSSH likewise reads an
//!   unreadable known_hosts as recording no keys.
//! - Host patterns are comma-separated globs (`*`, `?`), compared without
//!   regard to case; a matching `!` pattern excludes the host even when another
//!   pattern matches. A hashed entry (`|1|salt|hash`) is HMAC-SHA1 of the host.

use std::path::PathBuf;

use hmac::{Hmac, KeyInit, Mac};
use russh::keys::PublicKey;
use sha1::Sha1;

/// What known_hosts says about one host: its plain keys, and the keys revoked
/// for it.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Recorded {
    pub(super) keys: Vec<PublicKey>,
    pub(super) revoked: Vec<PublicKey>,
}

/// Every entry for `host:port` across `files`. A file that cannot be read at
/// all (missing, permissions) contributes nothing.
pub(super) fn recorded_for(host: &str, port: u16, files: &[PathBuf]) -> Recorded {
    let mut recorded = Recorded::default();
    for path in files {
        if let Ok(contents) = std::fs::read(path) {
            let found = parse(&contents, host, port);
            recorded.keys.extend(found.keys);
            recorded.revoked.extend(found.revoked);
        }
    }
    recorded
}

/// The entries for `host:port` in one file's contents.
pub(super) fn parse(contents: &[u8], host: &str, port: u16) -> Recorded {
    let target = target_name(host, port);
    let mut recorded = Recorded::default();
    for line in contents.split(|&b| b == b'\n') {
        // Lossy, so a stray non-UTF-8 byte only spoils its own line, and only
        // if it falls in a field that matters.
        let line = String::from_utf8_lossy(line);
        let mut fields = line.split_ascii_whitespace();
        let Some(first) = fields.next() else { continue };
        if first.starts_with('#') {
            continue;
        }
        let (revoked, hosts) = match first {
            "@revoked" => (true, fields.next()),
            "@cert-authority" => continue,
            marker if marker.starts_with('@') => continue,
            hosts => (false, Some(hosts)),
        };
        let (Some(hosts), Some(key_type), Some(key)) = (hosts, fields.next(), fields.next()) else {
            continue;
        };
        if !hosts_match(hosts, &target) {
            continue;
        }
        // The type field must agree with the key it names, as in OpenSSH.
        let Ok(key) = PublicKey::from_openssh(&format!("{key_type} {key}")) else {
            continue;
        };
        if revoked {
            recorded.revoked.push(key);
        } else {
            recorded.keys.push(key);
        }
    }
    recorded
}

/// The name a known_hosts entry is matched against: the host, or `[host]:port`
/// off port 22. Lowercase, since OpenSSH lowercases the host it looks up.
fn target_name(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

fn hosts_match(hosts: &str, target: &str) -> bool {
    if let Some(hashed) = hosts.strip_prefix("|1|") {
        hashed_match(hashed, target)
    } else {
        pattern_list_match(hosts, target)
    }
}

/// `salt|hash`, both base64: the entry is for `target` if the hash is
/// HMAC-SHA1 of `target` keyed with the salt.
fn hashed_match(salt_and_hash: &str, target: &str) -> bool {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;
    let Some((salt, hash)) = salt_and_hash.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (engine.decode(salt), engine.decode(hash)) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(target.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

/// OpenSSH's `match_hostname`: any matching `!pattern` excludes the target,
/// otherwise any matching pattern includes it.
fn pattern_list_match(list: &str, target: &str) -> bool {
    let mut matched = false;
    for pattern in list.split(',') {
        let pattern = pattern.to_ascii_lowercase();
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob(negated.as_bytes(), target.as_bytes()) {
                return false;
            }
        } else if glob(pattern.as_bytes(), target.as_bytes()) {
            matched = true;
        }
    }
    matched
}

/// `*` matches any run of bytes and `?` exactly one. Iterative, backtracking
/// only to the most recent `*`: O(pattern × text) with no recursion, so a
/// hostile line in a synced known_hosts cannot hang or overflow the stack.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    // The last `*` seen, and where in `text` it currently starts matching.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == b'?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                // Let the last `*` swallow one more byte and retry after it.
                Some((star_p, star_t)) => {
                    star = Some((star_p, star_t + 1));
                    p = star_p + 1;
                    t = star_t + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_follows_openssh_wildcards() {
        for (pattern, text, expected) in [
            ("*", "anything", true),
            ("*", "", true),
            ("db.example.co?", "db.example.com", true),
            ("db.example.co?", "db.example.co", false),
            ("*.example.com", "example.com", false),
            ("*.example.com", "a.b.example.com", true),
            ("[db*]:2222", "[db.internal]:2222", true),
            ("[db*]:2222", "[db.internal]:22", false),
            ("d*b*c", "dxxbyyc", true),
            ("d*b*c", "dxxbyy", false),
        ] {
            assert_eq!(
                glob(pattern.as_bytes(), text.as_bytes()),
                expected,
                "{pattern} vs {text}"
            );
        }
    }

    #[test]
    fn a_hostile_pattern_neither_hangs_nor_overflows() {
        // Exponential for a backtracking matcher, and one stack frame per `*`
        // for a recursive one.
        let many = format!("{}b", "*a".repeat(2_000));
        assert!(!glob(many.as_bytes(), "a".repeat(4_000).as_bytes()));
        let stars = "*".repeat(100_000);
        assert!(glob(stars.as_bytes(), b"db.example.com"));
    }

    #[test]
    fn the_target_name_is_lowercase_and_bracketed_off_port_22() {
        assert_eq!(target_name("DB.Internal", 22), "db.internal");
        assert_eq!(target_name("DB.Internal", 2222), "[db.internal]:2222");
    }

    #[test]
    fn a_negated_pattern_excludes_even_when_listed_first() {
        assert!(!pattern_list_match(
            "!db.example.com,*.example.com",
            "db.example.com"
        ));
        assert!(pattern_list_match(
            "!db.example.com,*.example.com",
            "web.example.com"
        ));
    }

    #[test]
    fn a_hashed_entry_matches_only_its_own_host() {
        // From `ssh-keygen -H` over a `db.example.com` line.
        let hashed = "tbLEo3oZNezhlFk5483qNgLrsh8=|l9n0k6ONOAQcZH3VHlomE0dSN2E=";
        assert!(hashed_match(hashed, "db.example.com"));
        assert!(!hashed_match(hashed, "db.example.org"));
        assert!(!hashed_match("not|base64!", "db.example.com"));
    }
}
