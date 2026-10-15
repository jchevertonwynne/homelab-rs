//! The identity Cloudflare Access puts on a request — homelab-go's `access`.
//!
//! It deliberately decides nothing. There is no middleware here and nothing
//! returns a 403, because the interesting half of an authentication check is
//! what happens when it fails, and that belongs in the app where a reader can
//! see it next to the route it protects.
//!
//! What is shared is the part that must not drift between apps: the header
//! name, what counts as the same address, and — as [`Allowlist`] — the set of
//! addresses a hostname's Access policy admits, which every app behind Access
//! has to be told and none of them can see for itself.

use std::collections::BTreeSet;

use axum::http::HeaderMap;

/// Set by Cloudflare Access once it has authenticated the caller. Its absence
/// in production means Access itself stopped working, which is why an app
/// should treat that as an error rather than an anonymous visitor.
pub const EMAIL_HEADER: &str = "Cf-Access-Authenticated-User-Email";

/// Collapses case and surrounding whitespace, so "Alice@x.com" and
/// " alice@x.com " are one identity. Access sends the address however the
/// identity provider spelled it, so comparing byte-for-byte would turn a
/// capital letter into a lockout.
pub fn normalize_email(s: &str) -> String {
    s.trim().to_lowercase()
}

/// The normalised authenticated address, or None when the header is absent
/// and `dev_user` is empty — the deny path, which is the caller's to take.
///
/// `dev_user` is a local-development fallback, consulted only when the header
/// is missing and never in preference to it: an app behind Access must not be
/// able to have its caller overridden by its own flags.
pub fn email(headers: &HeaderMap, dev_user: &str) -> Option<String> {
    // The first value, as Go's Header.Get gives it.
    let from_header = headers
        .get(EMAIL_HEADER)
        .map(|v| normalize_email(&String::from_utf8_lossy(v.as_bytes())))
        .unwrap_or_default();
    if !from_header.is_empty() {
        return Some(from_header);
    }
    let dev = normalize_email(dev_user);
    (!dev.is_empty()).then_some(dev)
}

/// The set of addresses a hostname's Access policy admits, handed to an app
/// at startup.
///
/// Checking it is not redundant with Access: Access consults its policy when
/// it issues a session and not again, so someone taken off the policy keeps a
/// working header until that session expires. An app that checks
/// [`Allowlist::admits`] on every request refuses them as soon as its own copy
/// of the list stops naming them.
///
/// The default admits everyone — an unconfigured list — which is why
/// production must pass [`Allowlist::check`].
#[derive(Clone, Debug, Default)]
pub struct Allowlist {
    /// Sorted and deduplicated, for an app that offers the list to a person.
    emails: BTreeSet<String>,
    dev_user: String,
}

impl Allowlist {
    /// Parses the raw comma-separated flag value. Addresses are normalised as
    /// the one on a request is, and blank entries dropped, so a trailing comma
    /// cannot become an address nobody matches.
    ///
    /// `dev_user` is admitted whether or not it is listed: it only ever stands
    /// in for a request with no header, somewhere with no Access and so no
    /// policy to be on. In production it is empty.
    pub fn new(raw: &str, dev_user: &str) -> Self {
        Allowlist {
            emails: raw
                .split(',')
                .map(normalize_email)
                .filter(|e| !e.is_empty())
                .collect(),
            dev_user: normalize_email(dev_user),
        }
    }

    /// Whether an authenticated (already normalised) address may use the app.
    /// An empty list admits everyone: a copy of deployment configuration that
    /// never arrived must not lock out everyone the policy does admit.
    pub fn admits(&self, email: &str) -> bool {
        if self.emails.is_empty() {
            return true;
        }
        if !self.dev_user.is_empty() && email == self.dev_user {
            return true;
        }
        self.emails.contains(email)
    }

    /// The admitted addresses, sorted. The dev user is not among them: it is
    /// an identity for a machine with no Access in front of it, not a person.
    pub fn emails(&self) -> impl Iterator<Item = &str> {
        self.emails.iter().map(String::as_str)
    }

    /// Whether any address was given.
    pub fn configured(&self) -> bool {
        !self.emails.is_empty()
    }

    /// The normalised local-development identity, to pass to [`email`].
    pub fn dev_user(&self) -> &str {
        &self.dev_user
    }

    /// Refuses the one configuration in which an app admits whoever Access
    /// lets through: no list and no dev user. In production that means the
    /// ConfigMap never reached the flag. Treat it like a database that will
    /// not open: log it and exit.
    pub fn check(&self) -> Result<(), &'static str> {
        if !self.configured() && self.dev_user.is_empty() {
            return Err(
                "no allowlist and no dev user: refusing to admit everyone Access lets through",
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(email: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(EMAIL_HEADER, email.parse().unwrap());
        h
    }

    #[test]
    fn header_wins_over_dev_user() {
        assert_eq!(
            email(&headers(" Alice@X.com "), "dev@x.com").as_deref(),
            Some("alice@x.com")
        );
        assert_eq!(
            email(&HeaderMap::new(), "Dev@X.com").as_deref(),
            Some("dev@x.com")
        );
        assert_eq!(email(&HeaderMap::new(), ""), None);
        assert_eq!(email(&headers("  "), ""), None);
    }

    #[test]
    fn parses_and_admits_as_homelab_go_does() {
        let a = Allowlist::new(" B@x.com,a@x.com,,b@x.com,\n", "");
        assert_eq!(a.emails().collect::<Vec<_>>(), ["a@x.com", "b@x.com"]);
        assert!(a.admits("a@x.com"));
        assert!(!a.admits("c@x.com"));
        assert!(a.check().is_ok());
    }

    #[test]
    fn empty_admits_everyone_but_fails_check_without_a_dev_user() {
        assert!(Allowlist::new("", "").admits("anyone@x.com"));
        assert!(Allowlist::new("", "").check().is_err());
        assert!(Allowlist::new("", "dev@x.com").check().is_ok());
    }

    #[test]
    fn dev_user_is_admitted_but_not_listed() {
        let a = Allowlist::new("a@x.com", "Dev@x.com");
        assert!(a.admits("dev@x.com"));
        assert_eq!(a.emails().count(), 1);
    }
}
