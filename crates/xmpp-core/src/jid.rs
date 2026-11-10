//! JIDs (Jabber IDs): XMPP addresses, defined in RFC 7622.
//!
//! Format: `[localpart@]domainpart[/resourcepart]`
//!
//! | example                   | name     | identifies                             |
//! |---------------------------|----------|----------------------------------------|
//! | `alice@example.com`       | bare JID | the account                            |
//! | `alice@example.com/phone` | full JID | one specific connection of the account |
//! | `example.com`             | domain   | the server itself                      |
//!
//! An account may have several simultaneous connections (phone, desktop...),
//! each with its own *resource*. The server uses the full JID to route to one
//! connection and the bare JID for "any/all connections of the account".

use std::fmt;
use std::str::FromStr;

/// An already validated and normalized XMPP address.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Jid {
    pub local: Option<String>,
    pub domain: String,
    pub resource: Option<String>,
}

/// Invalid JID; the text says which part failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JidError(&'static str);

impl fmt::Display for JidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JID inválido: {}", self.0)
    }
}

impl std::error::Error for JidError {}

impl Jid {
    /// Parses following the order from RFC 7622 §3.1:
    ///
    /// 1. the resource is everything after the **first** `/` (it may contain `/` and `@`);
    /// 2. in what remains, the localpart is whatever comes before the `@`;
    /// 3. the rest is the domain.
    ///
    /// Normalizes localpart and domain to lowercase: `Alice@Example.COM` and
    /// `alice@example.com` are the same account. The resource is case-sensitive.
    ///
    /// ponytail: approximates the PRECIS profiles (RFC 8264/8265) and IDNA with
    /// `to_lowercase` + a list of forbidden characters; switch to the
    /// `precis-profiles` crate if full Unicode normalization is needed.
    pub fn parse(s: &str) -> Result<Jid, JidError> {
        let (rest, resource) = match s.split_once('/') {
            Some((rest, resource)) => (rest, Some(resource)),
            None => (s, None),
        };
        let (local, domain) = match rest.split_once('@') {
            Some((local, domain)) => (Some(local), domain),
            None => (None, rest),
        };
        Ok(Jid {
            local: local.map(check_local).transpose()?,
            domain: check_domain(domain)?,
            resource: resource.map(check_resource).transpose()?,
        })
    }

    /// The bare JID (without resource): the account address.
    pub fn bare(&self) -> Jid {
        Jid {
            resource: None,
            ..self.clone()
        }
    }
}

/// Each JID part has 1 to 1023 bytes (RFC 7622 §3.2-3.4).
fn check_len(part: &str, what: &'static str) -> Result<(), JidError> {
    match part.len() {
        1..=1023 => Ok(()),
        _ => Err(JidError(what)),
    }
}

/// Localpart (the "user"): no whitespace, control characters, or the
/// characters reserved by RFC 7622 §3.3.1 — `" & ' / : < > @`.
fn check_local(local: &str) -> Result<String, JidError> {
    check_len(local, "localpart vazio ou longo demais")?;
    if local
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || "\"&'/:<>@".contains(c))
    {
        return Err(JidError("localpart contém caractere proibido"));
    }
    Ok(local.to_lowercase())
}

/// Domain: DNS name or IP literal. A trailing dot (`example.com.`) is
/// removed (RFC 7622 §3.2). `:` only appears in bracketed IPv6 literals.
fn check_domain(domain: &str) -> Result<String, JidError> {
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    check_len(domain, "domínio vazio ou longo demais")?;
    let ipv6 = domain.starts_with('[') && domain.ends_with(']');
    if domain.chars().any(|c| {
        c.is_whitespace() || c.is_control() || "@/\\<>\"'&".contains(c) || (c == ':' && !ipv6)
    }) {
        return Err(JidError("domínio contém caractere proibido"));
    }
    Ok(domain.to_lowercase())
}

/// Resource: free text (OpaqueString profile), only without control characters.
fn check_resource(resource: &str) -> Result<String, JidError> {
    check_len(resource, "recurso vazio ou longo demais")?;
    if resource.chars().any(char::is_control) {
        return Err(JidError("recurso contém caractere de controle"));
    }
    Ok(resource.to_owned())
}

impl FromStr for Jid {
    type Err = JidError;
    fn from_str(s: &str) -> Result<Jid, JidError> {
        Jid::parse(s)
    }
}

impl fmt::Display for Jid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(local) = &self.local {
            write!(f, "{local}@")?;
        }
        f.write_str(&self.domain)?;
        if let Some(resource) = &self.resource {
            write!(f, "/{resource}")?;
        }
        Ok(())
    }
}

#[test]
fn parse_normalize_and_reject() {
    let jid: Jid = "Alice@Exemplo.COM./celular/2@x".parse().unwrap();
    assert_eq!(jid.local.as_deref(), Some("alice"));
    assert_eq!(jid.domain, "exemplo.com");
    assert_eq!(jid.resource.as_deref(), Some("celular/2@x"));
    assert_eq!(jid.to_string(), "alice@exemplo.com/celular/2@x");
    assert_eq!(jid.bare().to_string(), "alice@exemplo.com");
    assert_eq!(Jid::parse("[::1]").unwrap().domain, "[::1]");

    for bad in [
        "", "@x", "a@", "x/", "a b@x", "a@b@c", "a@x:5222", "a\u{0}@x",
    ] {
        assert!(Jid::parse(bad).is_err(), "{bad:?} deveria ser inválido");
    }
}
