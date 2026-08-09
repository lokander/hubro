//! Shared URL handling for the server backends (Postgres and SQL Server).
//!
//! Both backends carry their connection settings in a URL and need the same
//! five operations on it: splice a session password in, canonicalize into the
//! saved-connection locator, extract the host/port target for an SSH tunnel,
//! rewrite to a forwarded local port, and assemble a URL from the
//! connection-form fields. The implementations differed only in engine *data*
//! — canonical scheme, accepted alias, default port, and the name of the TLS
//! query param — so those four (plus the indefinite article in the
//! wrong-scheme error, "a postgres://" vs "an mssql://") live in a
//! [`UrlScheme`] descriptor and the logic is written once here.
//!
//! Deliberately *not* here, because it is engine logic rather than engine
//! data: SQL Server's parsing of the `encrypt`/`trustServerCertificate`
//! query params into a tiberius `Config` (`sqlserver::parse_mssql_url`), and
//! each backend's `friendly_connect_error` categorization. This module treats
//! query params as opaque — [`UrlScheme::build`] only writes the TLS param,
//! and [`UrlScheme::normalize`] passes params through untouched.
//!
//! The backends re-export thin wrappers with their historical names
//! (`normalize_pg_url`, `build_mssql_url`, …) so call sites are unaffected.

use super::error::DbError;

/// The per-engine data the shared URL operations are parameterized over.
pub struct UrlScheme {
    /// Canonical scheme written into normalized locators.
    pub canonical: &'static str,
    /// Accepted alias scheme, rewritten to the canonical form.
    pub alias: &'static str,
    /// Indefinite article for the canonical scheme in error text
    /// ("a postgres:// URL", "an mssql:// URL").
    pub article: &'static str,
    /// Port filled in when a URL or the connection form leaves it out.
    pub default_port: u16,
    /// Query param carrying the TLS preference in form-built URLs.
    pub tls_param: &'static str,
}

/// Postgres: `postgres://` (alias `postgresql://`), port 5432, `sslmode`.
pub const POSTGRES: UrlScheme = UrlScheme {
    canonical: "postgres",
    alias: "postgresql",
    article: "a",
    default_port: 5432,
    tls_param: "sslmode",
};

/// SQL Server: `mssql://` (alias `sqlserver://`), port 1433, `encrypt`.
pub const MSSQL: UrlScheme = UrlScheme {
    canonical: "mssql",
    alias: "sqlserver",
    article: "an",
    default_port: 1433,
    tls_param: "encrypt",
};

/// Splices a password into a server URL (percent-encoding handled by the url
/// crate). Saved config stores URLs without passwords; this rebuilds the full
/// URL at connect time. Scheme-independent: nothing here depends on the
/// engine.
pub fn with_password(url: &str, password: &str) -> Result<String, DbError> {
    let mut parsed =
        url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    // set_password encodes most special characters but passes '%' through,
    // which would be mis-decoded on parse; encode it up front.
    let password = password.replace('%', "%25");
    parsed
        .set_password(Some(&password))
        .map_err(|_| DbError::Connect("this URL cannot carry a password".into()))?;
    Ok(parsed.into())
}

/// Rewrites a URL to connect through a forwarded local port; everything else
/// (user, database, query params) is kept. The saved URL stays the logical
/// one — this form is only ever used for the actual connect.
/// Scheme-independent, like [`with_password`].
pub fn via_local_port(url: &str, port: u16) -> Result<String, DbError> {
    let mut parsed =
        url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
    parsed
        .set_host(Some("127.0.0.1"))
        .map_err(|e| DbError::Connect(format!("rewriting URL host: {e}")))?;
    parsed
        .set_port(Some(port))
        .map_err(|_| DbError::Connect("rewriting URL port failed".into()))?;
    Ok(parsed.into())
}

impl UrlScheme {
    /// Canonicalizes a URL into the stable form used as a saved-connection
    /// locator and keyring account key, so the same server written different
    /// ways maps to one entry and one stored secret. Validates the scheme,
    /// then:
    ///
    /// - strips any password (never persisted),
    /// - rewrites the alias scheme to the canonical one,
    /// - lowercases the host (DNS is case-insensitive; IP literals are
    ///   unaffected),
    /// - fills the default port when omitted, so `host` and `host:<default>`
    ///   coincide.
    ///
    /// Query params (e.g. the TLS param) and the database path are left as-is.
    pub fn normalize(&self, url: &str) -> Result<String, DbError> {
        let mut parsed = url::Url::parse(url.trim())
            .map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
        if parsed.scheme() != self.canonical && parsed.scheme() != self.alias {
            return Err(DbError::Connect(format!(
                "expected {} {}:// URL, got {}://",
                self.article,
                self.canonical,
                parsed.scheme()
            )));
        }
        if parsed.scheme() == self.alias {
            // Both are non-special schemes, so this never fails; ignore
            // defensively.
            let _ = parsed.set_scheme(self.canonical);
        }
        let _ = parsed.set_password(None);
        if let Some(host) = parsed.host_str() {
            let lowered = host.to_ascii_lowercase();
            if lowered != host {
                parsed
                    .set_host(Some(&lowered))
                    .map_err(|e| DbError::Connect(format!("invalid host: {e}")))?;
            }
        }
        match parsed.port() {
            // 0 is not a usable port; reject it here so a pasted URL is held
            // to the same rule as the connection form (FRE-42).
            Some(0) => return Err(DbError::Connect("port must be between 1 and 65535".into())),
            // Both engines use non-special schemes, so the url crate always
            // serializes an explicit port — the bare and `:<default>` forms
            // now serialize equal.
            None => {
                let _ = parsed.set_port(Some(self.default_port));
            }
            Some(_) => {}
        }
        Ok(parsed.into())
    }

    /// The host and port a URL points at (falling back to the engine default
    /// port) — with an SSH tunnel this is the address the SSH server must
    /// reach.
    pub fn target(&self, url: &str) -> Result<(String, u16), DbError> {
        let parsed =
            url::Url::parse(url).map_err(|e| DbError::Connect(format!("invalid URL: {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| DbError::Connect("URL has no host".into()))?
            // IPv6 hosts come back bracketed; the forward target wants the
            // bare address.
            .trim_matches(['[', ']'])
            .to_string();
        Ok((host, parsed.port().unwrap_or(self.default_port)))
    }

    /// Builds a password-free URL from the individual connection-form fields.
    pub fn build(
        &self,
        host: &str,
        port: &str,
        database: &str,
        user: &str,
        tls: &str,
    ) -> Result<String, DbError> {
        let port = if port.trim().is_empty() {
            self.default_port.to_string()
        } else {
            port.trim().to_string()
        };
        if host.trim().is_empty() {
            return Err(DbError::Connect("host must not be empty".into()));
        }
        let mut parsed = url::Url::parse(&format!("{}://localhost", self.canonical))
            .expect("static base URL parses");
        parsed
            .set_host(Some(host.trim()))
            .map_err(|e| DbError::Connect(format!("invalid host: {e}")))?;
        let port_num: u16 = port
            .parse()
            .map_err(|_| DbError::Connect(format!("invalid port: {port}")))?;
        if port_num == 0 {
            return Err(DbError::Connect("port must be between 1 and 65535".into()));
        }
        parsed
            .set_port(Some(port_num))
            .map_err(|_| DbError::Connect("invalid port".into()))?;
        parsed
            .set_username(user.trim())
            .map_err(|_| DbError::Connect("invalid user".into()))?;
        // Only set a path for a non-empty database, so an empty db field
        // converges with a pasted URL that has no path (both → no trailing
        // `/`).
        let database = database.trim();
        if !database.is_empty() {
            parsed.set_path(&format!("/{database}"));
        }
        if !tls.is_empty() {
            parsed.set_query(Some(&format!("{}={tls}", self.tls_param)));
        }
        // Route through the normalizer so a form host typed as `MyHost` and a
        // pasted `myhost` URL land on the same canonical locator.
        self.normalize(parsed.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_password_splices_and_encodes() {
        assert_eq!(
            with_password("postgres://user@db.example.com:5432/app", "p@ss w%rd").unwrap(),
            "postgres://user:p%40ss%20w%25rd@db.example.com:5432/app"
        );
        assert_eq!(
            with_password("mssql://sa@db.example.com:1433/app", "p@ss w%rd").unwrap(),
            "mssql://sa:p%40ss%20w%25rd@db.example.com:1433/app"
        );
    }

    #[test]
    fn normalize_strips_password_and_checks_scheme() {
        assert_eq!(
            POSTGRES
                .normalize(" postgres://u:secret@h:5432/db?sslmode=require ")
                .unwrap(),
            "postgres://u@h:5432/db?sslmode=require"
        );
        assert_eq!(
            MSSQL
                .normalize(" mssql://u:secret@h:1433/db?encrypt=off ")
                .unwrap(),
            "mssql://u@h:1433/db?encrypt=off"
        );
        assert!(POSTGRES.normalize("mysql://u@h/db").is_err());
        assert!(MSSQL.normalize("postgres://u@h/db").is_err());
        assert!(POSTGRES.normalize("not a url").is_err());
        assert!(MSSQL.normalize("not a url").is_err());
    }

    #[test]
    fn normalize_error_names_the_expected_scheme() {
        // The article is engine data ("a postgres", "an mssql") — pin the
        // exact wording each backend historically produced.
        assert_eq!(
            POSTGRES
                .normalize("mysql://u@h/db")
                .unwrap_err()
                .to_string(),
            "connection failed: expected a postgres:// URL, got mysql://"
        );
        assert_eq!(
            MSSQL
                .normalize("postgres://u@h/db")
                .unwrap_err()
                .to_string(),
            "connection failed: expected an mssql:// URL, got postgres://"
        );
    }

    #[test]
    fn normalize_canonicalizes_scheme_host_and_port() {
        // Alias → canonical scheme, default port filled, host lowercased.
        assert_eq!(
            POSTGRES
                .normalize("postgresql://user@Db.Example.COM/app")
                .unwrap(),
            "postgres://user@db.example.com:5432/app"
        );
        assert_eq!(
            MSSQL
                .normalize("sqlserver://user@Db.Example.COM/app")
                .unwrap(),
            "mssql://user@db.example.com:1433/app"
        );
        // Already canonical: idempotent.
        for (scheme, canonical) in [
            (&POSTGRES, "postgres://user@db.example.com:5432/app"),
            (&MSSQL, "mssql://user@db.example.com:1433/app"),
        ] {
            assert_eq!(scheme.normalize(canonical).unwrap(), canonical);
        }
    }

    #[test]
    fn normalize_rejects_a_zero_port() {
        // The pasted-URL path is held to the same rule as the form fields
        // (FRE-42).
        assert!(POSTGRES.normalize("postgres://user@host:0/db").is_err());
        assert!(POSTGRES.normalize("postgres://user@host:5432/db").is_ok());
        assert!(MSSQL.normalize("mssql://user@host:0/db").is_err());
        assert!(MSSQL.normalize("mssql://user@host:1433/db").is_ok());
    }

    #[test]
    fn equivalent_urls_normalize_to_the_same_locator() {
        // The same server written five ways must collapse to one locator, so
        // a saved list dedups and the keyring key matches.
        let pg_forms = [
            "postgres://user@host:5432/db",
            "postgresql://user@host:5432/db",
            "postgres://user@host/db",
            "postgresql://user@HOST/db",
            "postgres://user:pw@host/db",
        ];
        let mssql_forms = [
            "mssql://user@host:1433/db",
            "sqlserver://user@host:1433/db",
            "mssql://user@host/db",
            "sqlserver://user@HOST/db",
            "mssql://user:pw@host/db",
        ];
        for (scheme, forms) in [(&POSTGRES, pg_forms), (&MSSQL, mssql_forms)] {
            let canonical = scheme.normalize(forms[0]).unwrap();
            for form in forms {
                assert_eq!(scheme.normalize(form).unwrap(), canonical, "{form}");
            }
        }
    }

    #[test]
    fn build_assembles_fields_and_defaults_port() {
        assert_eq!(
            POSTGRES
                .build("db.example.com", "", "app", "user", "prefer")
                .unwrap(),
            "postgres://user@db.example.com:5432/app?sslmode=prefer"
        );
        assert_eq!(
            POSTGRES.build(" h ", "6543", "d", "u", "require").unwrap(),
            "postgres://u@h:6543/d?sslmode=require"
        );
        assert_eq!(
            MSSQL
                .build("db.example.com", "", "app", "sa", "on")
                .unwrap(),
            "mssql://sa@db.example.com:1433/app?encrypt=on"
        );
        assert_eq!(
            MSSQL.build(" h ", "14330", "d", "u", "off").unwrap(),
            "mssql://u@h:14330/d?encrypt=off"
        );
        assert!(POSTGRES
            .build("h", "not-a-port", "d", "u", "prefer")
            .is_err());
        assert!(MSSQL.build("h", "not-a-port", "d", "u", "on").is_err());
    }

    #[test]
    fn build_rejects_a_zero_port_and_empty_host() {
        for scheme in [&POSTGRES, &MSSQL] {
            // 0 parses as a valid u16 but is not a usable port (FRE-42).
            assert!(scheme.build("host", "0", "db", "user", "").is_err());
            assert!(scheme.build("host", "70000", "db", "user", "").is_err());
            let err = scheme.build("  ", "5432", "db", "u", "").unwrap_err();
            assert!(err.to_string().contains("host"));
        }
    }

    #[test]
    fn form_and_paste_converge_for_an_empty_database() {
        // An empty database field must produce the same locator as a pasted
        // URL with no path — no phantom trailing-slash entry.
        let from_form = POSTGRES.build("host", "", "", "user", "").unwrap();
        assert_eq!(from_form, "postgres://user@host:5432");
        assert_eq!(
            from_form,
            POSTGRES.normalize("postgres://user@host").unwrap()
        );

        let from_form = MSSQL.build("host", "", "", "user", "").unwrap();
        assert_eq!(from_form, "mssql://user@host:1433");
        assert_eq!(from_form, MSSQL.normalize("mssql://user@host").unwrap());
    }

    #[test]
    fn target_extracts_host_and_defaults_port() {
        assert_eq!(
            POSTGRES.target("postgres://u@db.internal/app").unwrap(),
            ("db.internal".to_string(), 5432)
        );
        assert_eq!(
            POSTGRES
                .target("postgres://u@db.internal:6543/app")
                .unwrap(),
            ("db.internal".to_string(), 6543)
        );
        // IPv6 hosts come back bracketed; the forward target wants them bare.
        assert_eq!(
            POSTGRES.target("postgres://u@[::1]:6543/app").unwrap(),
            ("::1".to_string(), 6543)
        );
        assert_eq!(
            MSSQL.target("mssql://u@db.internal/app").unwrap(),
            ("db.internal".to_string(), 1433)
        );
        assert_eq!(
            MSSQL.target("mssql://u@[::1]:14330/app").unwrap(),
            ("::1".to_string(), 14330)
        );
        assert!(POSTGRES.target("not a url").is_err());
        assert!(MSSQL.target("not a url").is_err());
    }

    #[test]
    fn via_local_port_rewrites_only_host_and_port() {
        assert_eq!(
            via_local_port("postgres://u@db.internal:5432/app?sslmode=disable", 40123).unwrap(),
            "postgres://u@127.0.0.1:40123/app?sslmode=disable"
        );
        assert_eq!(
            via_local_port("mssql://u@db.internal:1433/app?encrypt=off", 40123).unwrap(),
            "mssql://u@127.0.0.1:40123/app?encrypt=off"
        );
    }
}
