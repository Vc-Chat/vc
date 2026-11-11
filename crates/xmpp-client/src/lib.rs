//! # xmpp-client
//!
//! Minimal XMPP client: connects, negotiates STARTTLS → SASL PLAIN → bind and
//! returns a [`Client`] ready to exchange stanzas. It mirrors `xmpp-server`,
//! but only follows RFC 6120, so it works with any XMPP server (Prosody,
//! ejabberd, ...).

use std::io;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{
    self,
    pki_types::{CertificateDer, ServerName, pem::PemObject},
};
use xmpp_core::{Element, Event, Jid, XmlReader, ns, sasl, stream_header};

pub type Stream = TlsStream<TcpStream>;

/// Authenticated and bound session.
///
/// Reading and writing live in separate fields: to read and write at the same
/// time, destructure (`let Client { reader, writer, .. } = client;`) and use
/// each half in its own task — `XmlReader::next` cannot go inside a `select!`.
pub struct Client {
    /// Full JID assigned by the server at bind time (e.g. `alice@localhost/a1b2`).
    pub jid: Jid,
    pub reader: XmlReader<ReadHalf<Stream>>,
    pub writer: WriteHalf<Stream>,
}

/// Client TLS configuration: Mozilla's certificate authorities
/// (`webpki-roots`) plus `extra_roots`, e.g. the development server's
/// self-signed certificate.
pub fn tls_config(
    extra_roots: impl IntoIterator<Item = CertificateDer<'static>>,
) -> io::Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    for cert in extra_roots {
        roots.add(cert).map_err(io::Error::other)?;
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Reads every certificate in a PEM file, e.g. the development server's
/// self-signed certificate, to be passed to [`tls_config`] as an extra root.
pub fn load_ca(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_file_iter(path)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))
}

impl Client {
    /// Connects as `jid` (the resource, if any, is requested at bind time).
    ///
    /// `addr` is the server's `host:port`; `None` uses `<domain>:5222`.
    /// ponytail: no DNS SRV resolution (`_xmpp-client._tcp.<domain>`,
    /// RFC 6120 §3.2); pass `addr` when the server lives on another host.
    pub async fn connect(
        jid: &Jid,
        password: &str,
        addr: Option<&str>,
        tls: Arc<rustls::ClientConfig>,
    ) -> io::Result<Client> {
        let Some(user) = jid.local.as_deref() else {
            return Err(io::Error::other(
                "o JID precisa de usuário (usuario@dominio)",
            ));
        };
        let domain = jid.domain.as_str();
        let addr = addr.map_or_else(|| format!("{domain}:5222"), str::to_owned);

        // ── Stream 1, plain TCP: only used to ask for STARTTLS (RFC 6120 §5).
        let (rd, mut wr) = TcpStream::connect(&addr).await?.into_split();
        let mut rd = XmlReader::new(rd);
        let features = open_stream(&mut rd, &mut wr, domain).await?;
        if features.get_child("starttls", ns::TLS).is_none() {
            // We never send a password unencrypted, even if the server accepts it.
            return Err(io::Error::other("o servidor não oferece STARTTLS"));
        }
        send(&mut wr, &Element::new("starttls", ns::TLS)).await?;
        if !next_element(&mut rd).await?.is("proceed", ns::TLS) {
            return Err(io::Error::other("o servidor recusou o STARTTLS"));
        }
        // TLS handshake on the same socket. The certificate must be valid for
        // the JID's domain — that is what proves we are talking to the right server.
        let tcp = rd.into_inner()?.reunite(wr).map_err(io::Error::other)?;
        let name = ServerName::try_from(domain.to_owned()).map_err(io::Error::other)?;
        let tls = TlsConnector::from(tls).connect(name, tcp).await?;

        // ── Stream 2, encrypted: SASL PLAIN authentication (§6, RFC 4616).
        let (rd, mut wr) = tokio::io::split(tls);
        let mut rd = XmlReader::new(rd);
        let features = open_stream(&mut rd, &mut wr, domain).await?;
        let plain = features
            .get_child("mechanisms", ns::SASL)
            .is_some_and(|m| m.elements().any(|m| m.text_content() == "PLAIN"));
        if !plain {
            return Err(io::Error::other("o servidor não oferece SASL PLAIN"));
        }
        let auth = Element::new("auth", ns::SASL)
            .attr("mechanism", "PLAIN")
            .text(&sasl::plain_encode(user, password));
        send(&mut wr, &auth).await?;
        let outcome = next_element(&mut rd).await?;
        if !outcome.is("success", ns::SASL) {
            // <failure><not-authorized/></failure>: the child tells the reason.
            let reason = outcome.elements().next().map_or("?", |e| e.name.as_str());
            let msg = format!("autenticação recusada: {reason}");
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
        }

        // ── Stream 3: post-SASL restart (§6.4.6) and resource binding (§7).
        let mut rd = rd.restart();
        let features = open_stream(&mut rd, &mut wr, domain).await?;
        if features.get_child("bind", ns::BIND).is_none() {
            return Err(io::Error::other("o servidor não oferece bind"));
        }
        let mut bind = Element::new("bind", ns::BIND);
        if let Some(resource) = &jid.resource {
            bind = bind.child(Element::new("resource", ns::BIND).text(resource));
        }
        let iq = Element::new("iq", ns::CLIENT)
            .attr("type", "set")
            .attr("id", "bind")
            .child(bind);
        send(&mut wr, &iq).await?;
        let result = next_element(&mut rd).await?;
        let bound = result
            .get_child("bind", ns::BIND)
            .and_then(|bind| bind.get_child("jid", ns::BIND))
            .filter(|_| result.get_attr("type") == Some("result"))
            .map(Element::text_content)
            .ok_or_else(|| io::Error::other(format!("bind recusado: {result}")))?;
        let jid = Jid::parse(&bound).map_err(io::Error::other)?;
        Ok(Client {
            jid,
            reader: rd,
            writer: wr,
        })
    }

    pub async fn send(&mut self, stanza: &Element) -> io::Result<()> {
        send(&mut self.writer, stanza).await
    }

    /// Next element from the server; `None` when the server closes the stream.
    pub async fn recv(&mut self) -> io::Result<Option<Element>> {
        match self.reader.next().await? {
            Event::Element(el) => Ok(Some(el)),
            _ => Ok(None),
        }
    }
}

/// Writes an element and flushes (TLS may hold bytes in its buffer).
pub async fn send<W: AsyncWrite + Unpin>(wr: &mut W, el: &Element) -> io::Result<()> {
    wr.write_all(el.to_string().as_bytes()).await?;
    wr.flush().await
}

/// Opens a stream: sends our header, reads the server's and returns the
/// `<stream:features>`, which tells the next negotiation step.
async fn open_stream<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    rd: &mut XmlReader<R>,
    wr: &mut W,
    domain: &str,
) -> io::Result<Element> {
    wr.write_all(stream_header(None, Some(domain), None).as_bytes())
        .await?;
    wr.flush().await?;
    if !matches!(rd.next().await?, Event::Header(_)) {
        return Err(io::Error::other("o servidor não abriu o stream"));
    }
    let features = next_element(rd).await?;
    if !features.is("features", ns::STREAM) {
        // Typically a <stream:error> (e.g. host-unknown).
        return Err(io::Error::other(format!(
            "esperava <stream:features>, veio {features}"
        )));
    }
    Ok(features)
}

async fn next_element<R: AsyncRead + Unpin>(rd: &mut XmlReader<R>) -> io::Result<Element> {
    match rd.next().await? {
        Event::Element(el) => Ok(el),
        _ => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "o servidor fechou o stream",
        )),
    }
}
