//! XMPP XML: the [`Element`] tree and the incremental [`XmlReader`].
//!
//! An XMPP connection is a single XML document that never "ends" while the
//! connection is alive. A whole-document parser would wait forever; here we
//! must hand over each level-1 child of the root as soon as it closes:
//!
//! ```text
//! <stream:stream ...>          → Event::Header   (depth 0, stays open)
//!   <message to='bob@x'>       ┐
//!     <body>oi</body>          │ Event::Element  (complete subtree, depth 1)
//!   </message>                 ┘
//! </stream:stream>             → Event::End
//! ```
//!
//! `quick-xml` does the tokenizing (tags, attributes, entities, namespaces);
//! this module assembles the subtrees and enforces the XMPP rules:
//!
//! - **Restricted XML** (RFC 6120 §11.1): comments, processing instructions,
//!   DTDs and non-predefined entities are forbidden. Without a DTD there is no
//!   "billion laughs" and no external entity (XXE).
//! - **Per-stanza size limit**: without it, a client could send an endless
//!   element and exhaust the server's memory.
//! - **Output is always well-formed**: names, characters and prefixes are
//!   validated on input. The server forwards stanzas from one user to another;
//!   if it let through something that breaks the recipient's parser, one user
//!   could kill another user's connection with a single message.

use std::fmt;
use std::io;

use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::escape::{escape, resolve_predefined_entity};
use quick_xml::events::{BytesStart, Event as XmlEvent};
use quick_xml::name::ResolveResult;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader, Take};

use crate::ns;

/// Maximum size of a stanza, in bytes. RFC 6120 §13.12 tells the server to
/// enforce a limit; 256 KiB is plenty for chat messages.
pub const MAX_STANZA_SIZE: u64 = 256 * 1024;

/// Child node of an element: another element or text (already unescaped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
}

/// XML element with its namespace already resolved.
///
/// We store the (local name, namespace) pair instead of the prefixed name: for
/// XMPP `<stream:features>` and `<features xmlns='http://etherx...'>` are the
/// same element, and it is the namespace that gives each element its meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub ns: String,
    /// Attributes in arrival order. They do not include `xmlns` declarations
    /// (the namespace is already in [`Element::ns`]).
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn new(name: &str, ns: &str) -> Self {
        Element {
            name: name.to_owned(),
            ns: ns.to_owned(),
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Builder: sets an attribute.
    pub fn attr(mut self, key: &str, value: &str) -> Self {
        self.set_attr(key, value);
        self
    }

    /// Builder: appends a child.
    pub fn child(mut self, child: Element) -> Self {
        self.children.push(Node::Element(child));
        self
    }

    /// Builder: appends text.
    pub fn text(mut self, text: &str) -> Self {
        self.children.push(Node::Text(text.to_owned()));
        self
    }

    /// Is this the element `name` in namespace `ns`?
    pub fn is(&self, name: &str, ns: &str) -> bool {
        self.name == name && self.ns == ns
    }

    pub fn get_attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Replaces the attribute if it exists; otherwise appends it.
    pub fn set_attr(&mut self, key: &str, value: &str) {
        match self.attrs.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value.to_owned(),
            None => self.attrs.push((key.to_owned(), value.to_owned())),
        }
    }

    /// Children that are elements (skips text).
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|node| match node {
            Node::Element(el) => Some(el),
            Node::Text(_) => None,
        })
    }

    pub fn get_child(&self, name: &str, ns: &str) -> Option<&Element> {
        self.elements().find(|el| el.is(name, ns))
    }

    /// Concatenation of the direct text children.
    pub fn text_content(&self) -> String {
        self.children
            .iter()
            .filter_map(|node| match node {
                Node::Text(text) => Some(text.as_str()),
                Node::Element(_) => None,
            })
            .collect()
    }

    /// Serializes the element. `default_ns` is the default namespace already in
    /// effect where the element will be written: `xmlns` is only declared when it changes.
    fn write(&self, out: &mut String, default_ns: &str) {
        // Elements in the stream namespace use the `stream:` prefix declared
        // in the header (`<stream:features>`, `<stream:error>`). A prefix does
        // not change the default namespace, so children inherit `default_ns`.
        let (tag, children_ns) = if self.ns == ns::STREAM {
            (format!("stream:{}", self.name), default_ns)
        } else {
            (self.name.clone(), self.ns.as_str())
        };
        out.push('<');
        out.push_str(&tag);
        if self.ns != ns::STREAM && self.ns != default_ns {
            out.push_str(&format!(" xmlns='{}'", escape(self.ns.as_str())));
        }
        for (key, value) in &self.attrs {
            out.push_str(&format!(" {key}='{}'", escape(value.as_str())));
        }
        if self.children.is_empty() {
            out.push_str("/>");
            return;
        }
        out.push('>');
        for child in &self.children {
            match child {
                Node::Element(el) => el.write(out, children_ns),
                Node::Text(text) => out.push_str(&escape(text.as_str())),
            }
        }
        out.push_str(&format!("</{tag}>"));
    }
}

/// Serializes as a child of a `jabber:client` stream: that is how every stanza
/// goes to the socket, so `jabber:client` is not redeclared.
impl fmt::Display for Element {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        self.write(&mut out, ns::CLIENT);
        f.write_str(&out)
    }
}

/// What the [`XmlReader`] yields.
#[derive(Debug)]
pub enum Event {
    /// The other side opened the stream. Attributes of `<stream:stream>` (to,
    /// from, version...) and, as an exception, `xmlns` with the declared
    /// default namespace, which the server must validate (RFC 6120 §4.8.2).
    Header(Element),
    /// A complete level-1 child: a stanza or a negotiation element.
    Element(Element),
    /// `</stream:stream>` or connection closed.
    End,
}

/// Read failures. Except for [`Error::Io`], each one maps to a stream error
/// condition that must be sent before closing.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Malformed XML, or not namespace-well-formed.
    NotWellFormed(String),
    /// XML construct forbidden by XMPP (RFC 6120 §11.1).
    Restricted(&'static str),
    /// Stanza larger than [`MAX_STANZA_SIZE`].
    TooBig,
    /// Valid XML, but not in the XMPP stream format.
    BadFormat(&'static str),
    /// Root element is not in the streams namespace.
    InvalidNamespace,
}

impl Error {
    /// Name of the matching stream error condition (RFC 6120 §4.9.3).
    pub fn condition(&self) -> &'static str {
        match self {
            Error::Io(_) => "internal-server-error",
            Error::NotWellFormed(_) => "not-well-formed",
            Error::Restricted(_) => "restricted-xml",
            Error::TooBig => "policy-violation",
            Error::BadFormat(_) => "bad-format",
            Error::InvalidNamespace => "invalid-namespace",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "erro de E/S: {e}"),
            Error::NotWellFormed(msg) => write!(f, "XML malformado: {msg}"),
            Error::Restricted(what) => write!(f, "XML proibido no XMPP: {what}"),
            Error::TooBig => write!(f, "stanza maior que {MAX_STANZA_SIZE} bytes"),
            Error::BadFormat(msg) => write!(f, "formato de stream inválido: {msg}"),
            Error::InvalidNamespace => f.write_str("namespace do stream inválido"),
        }
    }
}

impl std::error::Error for Error {}

/// Allows using `?` in functions that return `io::Result`.
impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        match e {
            Error::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other),
        }
    }
}

/// Incremental reader of an XMPP stream over any `AsyncRead`.
///
/// Layers: `R` (socket/TLS) → [`Take`] (per-stanza byte budget) →
/// [`BufReader`] → [`NsReader`] (namespace-aware tokenizer).
///
/// `next()` is not *cancel-safe*: do not use it inside `tokio::select!`.
/// To read and write at the same time, split reading and writing into tasks.
pub struct XmlReader<R> {
    reader: NsReader<BufReader<Take<R>>>,
    buf: Vec<u8>,
    /// Have we already received this stream's `<stream:stream>`?
    in_stream: bool,
}

impl<R: AsyncRead + Unpin> XmlReader<R> {
    pub fn new(inner: R) -> Self {
        Self::from_buffered(BufReader::new(inner.take(MAX_STANZA_SIZE)))
    }

    fn from_buffered(inner: BufReader<Take<R>>) -> Self {
        XmlReader {
            reader: NsReader::from_reader(inner),
            buf: Vec::new(),
            in_stream: false,
        }
    }

    /// Stream restart (RFC 6120 §4.3.3): after SASL the other side sends a
    /// new `<stream:stream>`, which starts a new XML document. We create a
    /// fresh tokenizer, but keep the bytes that are already buffered.
    pub fn restart(self) -> Self {
        Self::from_buffered(self.reader.into_inner())
    }

    /// Returns the transport for the TLS handshake (STARTTLS). Fails if there
    /// are bytes already read but not processed: the other side could not have
    /// sent anything between `<starttls/>`/`<proceed/>` and the handshake
    /// (RFC 6120 §5.4.3).
    pub fn into_inner(self) -> Result<R, Error> {
        let buffered = self.reader.into_inner();
        if !buffered.buffer().is_empty() {
            return Err(Error::BadFormat("dados recebidos antes do handshake TLS"));
        }
        Ok(buffered.into_inner().into_inner())
    }

    fn budget(&mut self) -> &mut Take<R> {
        self.reader.get_mut().get_mut()
    }

    /// Reads until the next stream event is complete.
    pub async fn next(&mut self) -> Result<Event, Error> {
        // Every stanza starts with a full budget. When the `Take` reaches zero,
        // the tokenizer sees "end of file" and we turn that into TooBig.
        // (The BufReader reads ahead, so the real margin is the limit + up to 8 KiB.)
        self.budget().set_limit(MAX_STANZA_SIZE);
        // Stack of the elements currently open inside the current stanza.
        let mut stack: Vec<Element> = Vec::new();
        loop {
            self.buf.clear();
            let (resolved, event) = match self
                .reader
                .read_resolved_event_into_async(&mut self.buf)
                .await
            {
                Ok(pair) => pair,
                Err(e) => return Err(self.tokenizer_error(e)),
            };
            match event {
                // `<?xml version='1.0'?>` before the header is allowed.
                XmlEvent::Decl(_) if !self.in_stream => {}
                XmlEvent::Start(start) if !self.in_stream => {
                    let header = element(resolved, &start, true)?;
                    if header.ns != ns::STREAM {
                        return Err(Error::InvalidNamespace);
                    }
                    if header.name != "stream" {
                        return Err(Error::BadFormat("o elemento raiz deve ser <stream:stream>"));
                    }
                    self.in_stream = true;
                    return Ok(Event::Header(header));
                }
                XmlEvent::Start(start) => stack.push(element(resolved, &start, false)?),
                // Text before the header falls into `push_text` with an empty
                // stack: whitespace (common after `<?xml?>`) passes, anything else is an error.
                XmlEvent::Empty(_) if !self.in_stream => {
                    return Err(Error::BadFormat("esperava <stream:stream>"));
                }
                // Empty element (`<presence/>`): opens and closes in the same event.
                XmlEvent::Empty(start) => {
                    let el = element(resolved, &start, false)?;
                    if let Some(done) = attach(&mut stack, el) {
                        return Ok(Event::Element(done));
                    }
                }
                // The tokenizer already guarantees the closing tag matches the
                // opening one. Empty stack = the `</stream:stream>` itself was closed.
                XmlEvent::End(_) => match stack.pop() {
                    None => return Ok(Event::End),
                    Some(el) => {
                        if let Some(done) = attach(&mut stack, el) {
                            return Ok(Event::Element(done));
                        }
                    }
                },
                XmlEvent::Text(text) => {
                    if push_text(&mut stack, &text.xml10_content())? {
                        // Whitespace between stanzas is a keepalive (RFC 6120 §4.6.1):
                        // dropped without spending the next stanza's budget.
                        self.budget().set_limit(MAX_STANZA_SIZE);
                    }
                }
                XmlEvent::CData(data) => {
                    push_text(&mut stack, &data.xml10_content())?;
                }
                // Entities: only character references (`&#233;`) and the 5
                // predefined ones (`&lt; &gt; &amp; &apos; &quot;`). Without a DTD there are no others.
                XmlEvent::GeneralRef(entity) => {
                    let text = match entity.resolve_char_ref() {
                        Ok(Some(c)) => c.to_string(),
                        Ok(None) => resolve_predefined_entity(&entity)
                            .ok_or(Error::Restricted("entidade não predefinida"))?
                            .to_owned(),
                        Err(e) => return Err(Error::NotWellFormed(e.to_string())),
                    };
                    push_text(&mut stack, &text)?;
                }
                XmlEvent::Comment(_) => return Err(Error::Restricted("comentário")),
                XmlEvent::PI(_) | XmlEvent::Decl(_) => {
                    return Err(Error::Restricted("instrução de processamento"));
                }
                XmlEvent::DocType(_) => return Err(Error::Restricted("DTD")),
                XmlEvent::Eof => {
                    return match self.budget().limit() {
                        0 => Err(Error::TooBig),
                        _ => Ok(Event::End),
                    };
                }
            }
        }
    }

    fn tokenizer_error(&mut self, e: quick_xml::Error) -> Error {
        if self.budget().limit() == 0 {
            // The budget ran out in the middle of a tag: the "EOF" is artificial.
            return Error::TooBig;
        }
        match e {
            quick_xml::Error::Io(e) => Error::Io(io::Error::new(e.kind(), e.to_string())),
            other => Error::NotWellFormed(other.to_string()),
        }
    }
}

/// Converts a tokenizer start tag into an [`Element`] (without children).
fn element(resolved: ResolveResult, start: &BytesStart, is_header: bool) -> Result<Element, Error> {
    let ns = match resolved {
        ResolveResult::Bound(ns) => ns.as_ref().to_owned(),
        ResolveResult::Unbound => String::new(),
        ResolveResult::Unknown(prefix) => {
            return Err(Error::NotWellFormed(format!(
                "prefixo não declarado: {prefix}"
            )));
        }
    };
    let local_name = start.local_name();
    if !valid_name(local_name.as_ref()) {
        return Err(Error::NotWellFormed("nome de elemento inválido".into()));
    }
    let mut el = Element::new(local_name.as_ref(), &ns);
    for attr in start.attributes() {
        // `attributes()` already rejects duplicate attributes and invalid syntax.
        let attr = attr.map_err(|e| Error::NotWellFormed(e.to_string()))?;
        let key = attr.key.0;
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|e| Error::NotWellFormed(e.to_string()))?;
        if !valid_chars(&value) {
            return Err(Error::NotWellFormed(
                "caractere proibido em atributo".into(),
            ));
        }
        // Namespace declarations were already resolved by the NsReader. Only
        // the header's default namespace is kept, for validation.
        if key == "xmlns" || key.starts_with("xmlns:") {
            if key == "xmlns" && is_header {
                el.attrs.push((key.to_owned(), value.into_owned()));
            }
            continue;
        }
        let local = match key.split_once(':') {
            None => key,
            // `xml:` is the only predeclared prefix (e.g. xml:lang).
            Some(("xml", local)) => local,
            // Other prefixed attributes are dropped: when reserialized without
            // the prefix declaration, the recipient's XML would break.
            // XMPP uses no prefixed attributes other than xml:lang.
            Some(_) => continue,
        };
        if !valid_name(local) {
            return Err(Error::NotWellFormed("nome de atributo inválido".into()));
        }
        el.attrs.push((key.to_owned(), value.into_owned()));
    }
    Ok(el)
}

/// Hangs `el` on its parent element. Without a parent, `el` is a complete
/// level-1 child and is returned to become an `Event::Element`.
fn attach(stack: &mut [Element], el: Element) -> Option<Element> {
    match stack.last_mut() {
        Some(parent) => {
            parent.children.push(Node::Element(el));
            None
        }
        None => Some(el),
    }
}

/// Appends text to the open element (merging with the previous text, since
/// `a &amp; b` arrives as three events). Returns `true` if it was whitespace
/// between stanzas (allowed and ignored); real text outside a stanza is an error.
fn push_text(stack: &mut [Element], text: &str) -> Result<bool, Error> {
    if !valid_chars(text) {
        return Err(Error::NotWellFormed("caractere proibido em texto".into()));
    }
    let Some(parent) = stack.last_mut() else {
        return match text
            .bytes()
            .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            true => Ok(true),
            false => Err(Error::BadFormat("texto fora de uma stanza")),
        };
    };
    match parent.children.last_mut() {
        Some(Node::Text(previous)) => previous.push_str(text),
        _ => parent.children.push(Node::Text(text.to_owned())),
    }
    Ok(false)
}

/// Characters allowed in XML 1.0 (`Char`). Control characters such as U+0001
/// are illegal even when escaped (`&#1;`) and would break the recipient's parser.
fn valid_chars(text: &str) -> bool {
    text.chars().all(|c| {
        matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
    })
}

/// Conservative approximation of XML `Name`: a letter or `_` first; then
/// letters, digits, `-`, `.` or `_`. Blocks every ASCII delimiter.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads every event of an in-memory stream.
    async fn events(input: &[u8]) -> Result<Vec<Event>, Error> {
        let mut reader = XmlReader::new(input);
        let mut out = Vec::new();
        loop {
            match reader.next().await? {
                Event::End => return Ok(out),
                ev => out.push(ev),
            }
        }
    }

    #[tokio::test]
    async fn parses_stream_and_roundtrips_stanzas() {
        let input = b"<?xml version='1.0'?>\n<stream:stream to='x' version='1.0' \
            xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams'> \
            <message to='bob@x' xml:lang='pt' p:lixo='1' xmlns:p='urn:p'><body>a &amp; b&#233;<![CDATA[<c>]]></body>\
            <x:ext xmlns:x='urn:ext' k='&lt;v&gt;'/></message>\n<presence/></stream:stream>";
        let evs = events(input).await.unwrap();
        let [
            Event::Header(header),
            Event::Element(msg),
            Event::Element(presence),
        ] = &evs[..]
        else {
            panic!("eventos inesperados: {evs:?}");
        };
        assert_eq!(header.get_attr("xmlns"), Some(ns::CLIENT));
        assert_eq!(header.get_attr("to"), Some("x"));
        assert!(msg.is("message", ns::CLIENT));
        assert_eq!(
            msg.get_child("body", ns::CLIENT).unwrap().text_content(),
            "a & bé<c>"
        );
        assert_eq!(
            msg.get_child("ext", "urn:ext").unwrap().get_attr("k"),
            Some("<v>")
        );
        assert!(presence.is("presence", ns::CLIENT));
        // an attribute with an unknown prefix is dropped; xml:lang stays
        assert_eq!(msg.get_attr("xml:lang"), Some("pt"));
        assert_eq!(msg.get_attr("p:lixo"), None);

        // serializing and reading back yields the same tree
        let reparsed = format!(
            "<stream:stream xmlns='jabber:client' xmlns:stream='{}'>{msg}</stream:stream>",
            ns::STREAM
        );
        let evs = events(reparsed.as_bytes()).await.unwrap();
        assert!(matches!(&evs[1], Event::Element(el) if el == msg));
    }

    #[tokio::test]
    async fn rejects_restricted_and_oversized_xml() {
        let open =
            "<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams'>";
        let cases = [
            (format!("{open}<!-- oi -->"), "restricted-xml"),
            (
                format!("{open}<message>&custom;</message>"),
                "restricted-xml",
            ),
            (format!("{open}<message>&#1;</message>"), "not-well-formed"),
            (format!("{open}<a></b>"), "not-well-formed"),
            (format!("{open}texto solto"), "bad-format"),
            ("<html/>".to_owned(), "bad-format"),
            ("oi <stream:stream>".to_owned(), "bad-format"),
            ("<stream xmlns='urn:x'>".to_owned(), "invalid-namespace"),
            (
                format!("{open}<message>{}</message>", "a".repeat(300 * 1024)),
                "policy-violation",
            ),
        ];
        for (input, condition) in cases {
            let err = events(input.as_bytes()).await.unwrap_err();
            assert_eq!(err.condition(), condition, "entrada: {:.80}", input);
        }
    }
}
