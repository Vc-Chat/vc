//! # xmpp-core
//!
//! XMPP protocol primitives shared by the server and the client.
//!
//! At its core, XMPP (RFC 6120) is **a pair of XML documents that keep
//! growing forever**, one in each direction of a TCP connection:
//!
//! 1. Each side opens a root element `<stream:stream>` (the *stream*).
//! 2. Every direct child of that root is a protocol unit: a *stanza*
//!    (`<message/>`, `<presence/>`, `<iq/>`) or a negotiation element
//!    (`<starttls/>`, `<auth/>`, `<stream:features/>`...).
//! 3. Closing `</stream:stream>` ends the session.
//!
//! This crate implements the pieces that do not depend on the role
//! (client/server):
//!
//! - [`jid`]: XMPP addresses (`user@domain/resource`, RFC 7622);
//! - [`xml`]: a minimal XML tree ([`Element`]) and the incremental stream
//!   reader ([`XmlReader`]), with the security restrictions XMPP requires;
//! - [`ns`]: the namespaces used during negotiation and in stanzas;
//! - builders for protocol messages: stream header, stream and stanza
//!   errors, and the SASL PLAIN mechanism ([`sasl`]).

pub mod jid;
pub mod xml;

pub use jid::{Jid, JidError};
pub use xml::{Element, Error, Event, Node, XmlReader};

use quick_xml::escape::escape;

/// XMPP XML namespaces. In XMPP the meaning of an element is given by the
/// (name, namespace) pair: `<query xmlns='jabber:iq:roster'/>` and
/// `<query xmlns='http://jabber.org/protocol/disco#info'/>` are completely
/// different things.
pub mod ns {
    /// Default namespace of stanzas on a client-to-server connection (RFC 6120 §4.8.3).
    pub const CLIENT: &str = "jabber:client";
    /// Namespace of the `<stream:stream>` root and of `<stream:features>`/`<stream:error>`.
    pub const STREAM: &str = "http://etherx.jabber.org/streams";
    /// Stream error conditions (RFC 6120 §4.9.3).
    pub const STREAM_ERRORS: &str = "urn:ietf:params:xml:ns:xmpp-streams";
    /// Stanza error conditions (RFC 6120 §8.3.3).
    pub const STANZA_ERRORS: &str = "urn:ietf:params:xml:ns:xmpp-stanzas";
    /// STARTTLS negotiation (RFC 6120 §5).
    pub const TLS: &str = "urn:ietf:params:xml:ns:xmpp-tls";
    /// SASL authentication (RFC 6120 §6).
    pub const SASL: &str = "urn:ietf:params:xml:ns:xmpp-sasl";
    /// Resource binding (RFC 6120 §7).
    pub const BIND: &str = "urn:ietf:params:xml:ns:xmpp-bind";
    /// Session establishment — legacy from RFC 3921, a no-op nowadays.
    pub const SESSION: &str = "urn:ietf:params:xml:ns:xmpp-session";
    /// XEP-0199: XMPP-level ping.
    pub const PING: &str = "urn:xmpp:ping";
    /// RFC 6121 §2: roster (contact list).
    pub const ROSTER: &str = "jabber:iq:roster";
    /// XEP-0030: service discovery (which features an entity supports).
    pub const DISCO_INFO: &str = "http://jabber.org/protocol/disco#info";
}

/// Tag that closes the stream (RFC 6120 §4.4). It is plain text because it
/// closes the root element opened by [`stream_header`], not a full [`Element`].
pub const STREAM_CLOSE: &str = "</stream:stream>";

/// Opening of a stream (RFC 6120 §4.7). It stays "open": the root element is
/// only closed at the end of the connection with [`STREAM_CLOSE`].
///
/// - client → server: `to` = requested domain;
/// - server → client: `from` = served domain and `id` = unique,
///   unpredictable stream identifier.
pub fn stream_header(from: Option<&str>, to: Option<&str>, id: Option<&str>) -> String {
    let mut header = String::from("<?xml version='1.0'?><stream:stream");
    for (name, value) in [("from", from), ("to", to), ("id", id)] {
        if let Some(value) = value {
            header.push_str(&format!(" {name}='{}'", escape(value)));
        }
    }
    // version='1.0' signals RFC 6120 support (without it, it would be the
    // pre-standard protocol). The default namespace `jabber:client` applies to
    // every child stanza; the `stream:` prefix points to the root namespace.
    header.push_str(&format!(
        " version='1.0' xml:lang='en' xmlns='{}' xmlns:stream='{}'>",
        ns::CLIENT,
        ns::STREAM
    ));
    header
}

/// Stream error (RFC 6120 §4.9): `<stream:error><condition/></stream:error>`.
/// Stream errors are fatal — after sending one, the sender closes the stream.
pub fn stream_error(condition: &str) -> Element {
    Element::new("error", ns::STREAM).child(Element::new(condition, ns::STREAM_ERRORS))
}

/// Skeleton of a reply to a stanza: same element, same `id`, swapped
/// addresses (`to` ↔ `from`) and the given `type` (`result` or `error`).
pub fn reply(stanza: &Element, kind: &str) -> Element {
    let mut reply = Element::new(&stanza.name, &stanza.ns).attr("type", kind);
    for (attr, swapped) in [("id", "id"), ("from", "to"), ("to", "from")] {
        if let Some(value) = stanza.get_attr(attr) {
            reply.set_attr(swapped, value);
        }
    }
    reply
}

/// Stanza error (RFC 6120 §8.3). Unlike a stream error, it does not tear the
/// connection down: it only tells the sender that *that* stanza failed.
///
/// - `kind`: what the sender should do — `cancel` (give up), `modify`
///   (fix and resend), `wait` (retry later), `auth`, `continue`;
/// - `condition`: the reason, e.g. `service-unavailable`, `jid-malformed`.
pub fn stanza_error(stanza: &Element, kind: &str, condition: &str) -> Element {
    let error = Element::new("error", &stanza.ns)
        .attr("type", kind)
        .child(Element::new(condition, ns::STANZA_ERRORS));
    reply(stanza, "error").child(error)
}

/// SASL PLAIN (RFC 4616): the simplest authentication mechanism — it sends
/// username and password. It is only safe **inside TLS**, which is why the
/// server and client in this workspace refuse to use it without STARTTLS.
///
/// ponytail: PLAIN only. SCRAM-SHA-1/256 (RFC 5802/7677) keeps the password
/// away from the server; add it when some client requires it.
pub mod sasl {
    use base64::prelude::*;

    /// Builds the PLAIN "initial response": `authzid NUL authcid NUL password`,
    /// base64-encoded. The authzid (acting on behalf of another identity) is empty.
    pub fn plain_encode(authcid: &str, password: &str) -> String {
        BASE64_STANDARD.encode(format!("\0{authcid}\0{password}"))
    }

    /// Decodes a PLAIN response into `(authzid, authcid, password)`.
    /// `None` if the base64 is invalid or the message does not have 3 fields.
    pub fn plain_decode(data: &str) -> Option<(String, String, String)> {
        let raw = String::from_utf8(BASE64_STANDARD.decode(data.trim()).ok()?).ok()?;
        // RFC 4616 forbids NUL inside the fields, so there are exactly 3 parts.
        let mut parts = raw.split('\0').map(str::to_owned);
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(authzid), Some(authcid), Some(password), None) => {
                Some((authzid, authcid, password))
            }
            _ => None,
        }
    }

    #[test]
    fn plain_roundtrip() {
        let data = plain_encode("alice", "s3nh@");
        assert_eq!(
            plain_decode(&data),
            Some((String::new(), "alice".into(), "s3nh@".into()))
        );
        assert_eq!(plain_decode("não é base64"), None);
        assert_eq!(plain_decode(&BASE64_STANDARD.encode("sem-nul")), None);
    }
}
