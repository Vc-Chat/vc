//! # xmpp-server
//!
//! Client-to-server (c2s) XMPP server. Every connection goes through three
//! consecutive XML streams before becoming a session (RFC 6120):
//!
//! ```text
//! client                                           server
//!   ── <stream:stream to='domain'> ───────────────────▶
//!   ◀────────── <stream:stream id=…> <stream:features><starttls><required/>
//!   ── <starttls/> ───────────────────────────────────▶
//!   ◀───────────────────────────────────────── <proceed/>
//!   ═══════════════════ TLS handshake ════════════════════
//!   ── <stream:stream> (stream 2, now encrypted) ─────▶
//!   ◀────────── <stream:features><mechanisms><mechanism>PLAIN
//!   ── <auth mechanism='PLAIN'>base64</auth> ─────────▶
//!   ◀───────────────────────────────────────── <success/>
//!   ── <stream:stream> (stream 3, authenticated) ─────▶
//!   ◀────────── <stream:features><bind/>
//!   ── <iq type='set'><bind><resource>phone ──────────▶
//!   ◀────────── <iq type='result'><bind><jid>alice@domain/phone
//!   ◀════════ session: <message/> <presence/> <iq/> both ways ═════▶
//! ```
//!
//! After binding, each session has two tasks:
//!
//! - **reader** (the connection task): reads stanzas from the socket and calls
//!   [`Server::route`], which decides where each one goes;
//! - **writer**: drains a queue (`mpsc`) into the socket. Any session that
//!   wants to deliver something to this one just pushes it onto its queue.
//!
//! The `user → resource → queue` map in [`Server`] is the routing table.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use std::{fs, io};

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::{TlsAcceptor, rustls, server::TlsStream};
use xmpp_core::{
    Element, Event, Jid, STREAM_CLOSE, XmlReader, ns, reply, sasl, stanza_error, stream_error,
    stream_header,
};

/// Deadline between `connect` and the end of binding. Without it, connections
/// that never authenticate would hold memory and sockets forever.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(60);
/// Password attempts per connection (RFC 6120 §6.4.5 asks for at least 2).
const MAX_AUTH_ATTEMPTS: u32 = 3;
/// Stanzas in a session's outgoing queue. If it fills up, the recipient is not
/// reading its socket: new deliveries fail instead of growing memory forever.
const QUEUE_SIZE: usize = 256;

type Tls = TlsStream<TcpStream>;
/// user → resource → connection.
type Sessions = HashMap<String, HashMap<String, Resource>>;

/// A connection bound to a full JID, as seen by the routing table.
struct Resource {
    /// Outgoing queue: whatever is pushed here, the writer task sends to the socket.
    tx: mpsc::Sender<Element>,
    /// Last "available" presence sent by the connection. `None` = connected
    /// but unavailable (no initial presence yet, or it sent `unavailable`):
    /// it does not receive messages sent to the bare JID.
    presence: Option<Element>,
}

pub struct Server {
    domain: String,
    /// Accounts file: one `user:argon2-hash` line per account.
    users_file: PathBuf,
    tls: TlsAcceptor,
    // ponytail: one global Mutex; switch to per-user shards if contention
    // shows up in a profile. It is never held across an `.await`.
    sessions: Mutex<Sessions>,
}

impl Server {
    pub fn new(domain: &str, users_file: PathBuf, tls: Arc<rustls::ServerConfig>) -> Arc<Server> {
        Arc::new(Server {
            domain: domain.to_lowercase(),
            users_file,
            tls: TlsAcceptor::from(tls),
            sessions: Mutex::default(),
        })
    }

    /// Accepts connections forever, one tokio task per client.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((tcp, peer)) => {
                    let server = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = server.connection(tcp).await {
                            eprintln!("{peer}: {e}");
                        }
                    });
                }
                // E.g. the open-files limit. Wait a little instead of spinning
                // the CPU or taking the whole server down.
                Err(e) => {
                    eprintln!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// The whole life of a connection: negotiation under a deadline, then the session.
    async fn connection(self: Arc<Self>, tcp: TcpStream) -> io::Result<()> {
        let negotiation = async {
            let (mut rd, mut wr, user) = self.negotiate(tcp).await?;
            let (session, rx) = self.bind(&mut rd, &mut wr, &user).await?;
            Ok::<_, io::Error>((rd, wr, session, rx))
        };
        let (mut rd, mut wr, session, mut rx) =
            tokio::time::timeout(NEGOTIATION_TIMEOUT, negotiation)
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "negociação expirou"))??;

        // Writer task: drains the queue into the socket. It ends once every
        // `tx` has been dropped — the session's and the routing table's.
        let mut writer = tokio::spawn(async move {
            while let Some(stanza) = rx.recv().await {
                if write(&mut wr, &stanza.to_string()).await.is_err() {
                    return;
                }
            }
            // The session is over: close our side of the stream (RFC 6120 §4.4).
            let _ = write(&mut wr, STREAM_CLOSE).await;
            let _ = wr.shutdown().await;
        });

        eprintln!("sessão iniciada: {}", session.jid);
        loop {
            let stanza = match rd.next().await {
                Ok(Event::Element(stanza)) => stanza,
                // `</stream:stream>` or TCP closed: normal end.
                Ok(_) | Err(xmpp_core::Error::Io(_)) => break,
                Err(e) => {
                    session.send(stream_error(e.condition())).await;
                    break;
                }
            };
            if let Err(condition) = self.route(&session, stanza).await {
                session.send(stream_error(condition)).await;
                break;
            }
        }
        eprintln!("sessão encerrada: {}", session.jid);

        // Leave the routing table (see `Drop for Session`) and give the writer
        // task some time to drain the queue and close the stream.
        drop(session);
        if tokio::time::timeout(Duration::from_secs(5), &mut writer)
            .await
            .is_err()
        {
            writer.abort();
        }
        Ok(())
    }

    /// Streams 1 and 2: STARTTLS and authentication. Ends with stream 3 open
    /// and the bind features advertised; returns the authenticated user.
    async fn negotiate(
        &self,
        tcp: TcpStream,
    ) -> io::Result<(XmlReader<ReadHalf<Tls>>, WriteHalf<Tls>, String)> {
        // ── Stream 1, plaintext TCP: the only feature is STARTTLS, marked as
        // required. No password ever travels unencrypted.
        let (rd, mut wr) = tcp.into_split();
        let mut rd = XmlReader::new(rd);
        let starttls = Element::new("starttls", ns::TLS).child(Element::new("required", ns::TLS));
        self.open_stream(&mut rd, &mut wr, vec![starttls]).await?;
        if !next_element(&mut rd, &mut wr)
            .await?
            .is("starttls", ns::TLS)
        {
            // RFC 6120 §5.3.1: TLS is mandatory and the client tried something else.
            return Err(fail(&mut wr, "policy-violation").await);
        }
        write(&mut wr, &Element::new("proceed", ns::TLS).to_string()).await?;
        // Join the halves back into the TcpStream and run the TLS handshake on it.
        let tcp = rd.into_inner()?.reunite(wr).map_err(io::Error::other)?;
        let tls = self.tls.accept(tcp).await?;

        // ── Stream 2, inside TLS: a brand-new XML document (§5.4.3.3), now
        // offering the SASL mechanisms.
        let (rd, mut wr) = tokio::io::split(tls);
        let mut rd = XmlReader::new(rd);
        let mechanisms = Element::new("mechanisms", ns::SASL)
            .child(Element::new("mechanism", ns::SASL).text("PLAIN"));
        self.open_stream(&mut rd, &mut wr, vec![mechanisms]).await?;
        let user = self.authenticate(&mut rd, &mut wr).await?;

        // ── Stream 3: after `<success/>` both sides restart the stream
        // (§6.4.6). We offer bind and, for compatibility with old clients,
        // `session` marked as optional.
        let mut rd = rd.restart();
        let bind = Element::new("bind", ns::BIND);
        let session =
            Element::new("session", ns::SESSION).child(Element::new("optional", ns::SESSION));
        self.open_stream(&mut rd, &mut wr, vec![bind, session])
            .await?;
        Ok((rd, wr, user))
    }

    /// Receives the client header, answers with ours and advertises the
    /// features of this negotiation stage (RFC 6120 §4.2-4.3).
    async fn open_stream<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
        &self,
        rd: &mut XmlReader<R>,
        wr: &mut W,
        features: Vec<Element>,
    ) -> io::Result<()> {
        let first = rd.next().await;
        // We send our header before validating the client's: if something is
        // wrong, the stream error must go inside an open stream (§4.9.1.2).
        // The `id` identifies this stream (§4.7.3).
        let header = stream_header(Some(&self.domain), None, Some(&random_id()));
        write(wr, &header).await?;
        let header = match first {
            Ok(Event::Header(header)) => header,
            Ok(_) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Err(xmpp_core::Error::Io(e)) => return Err(e),
            Err(e) => return Err(fail(wr, e.condition()).await),
        };
        let to_other_domain = header
            .get_attr("to")
            .is_some_and(|to| !to.trim_end_matches('.').eq_ignore_ascii_case(&self.domain));
        let condition = if header.get_attr("xmlns") != Some(ns::CLIENT) {
            Some("invalid-namespace") // not a client-to-server stream (§4.8.2)
        } else if to_other_domain {
            Some("host-unknown") // we do not serve that domain (§4.9.3.6)
        } else if header.get_attr("version") != Some("1.0") {
            Some("unsupported-version") // we only speak RFC 6120 (§4.7.5)
        } else {
            None
        };
        if let Some(condition) = condition {
            return Err(fail(wr, condition).await);
        }
        let features = features
            .into_iter()
            .fold(Element::new("features", ns::STREAM), Element::child);
        write(wr, &features.to_string()).await
    }

    /// SASL PLAIN (RFC 6120 §6 + RFC 4616). Returns the username.
    async fn authenticate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
        &self,
        rd: &mut XmlReader<R>,
        wr: &mut W,
    ) -> io::Result<String> {
        let failure = |condition| {
            Element::new("failure", ns::SASL)
                .child(Element::new(condition, ns::SASL))
                .to_string()
        };
        for _ in 0..MAX_AUTH_ATTEMPTS {
            let auth = next_element(rd, wr).await?;
            if !auth.is("auth", ns::SASL) {
                // Stanzas before authentication are not allowed (§4.9.3.12).
                return Err(fail(wr, "not-authorized").await);
            }
            if auth.get_attr("mechanism") != Some("PLAIN") {
                write(wr, &failure("invalid-mechanism")).await?;
                continue;
            }
            let mut data = auth.text_content();
            if data.is_empty() {
                // The client sent no "initial response": send an empty
                // challenge and wait for the credentials in <response/> (§6.4.2-6.4.3).
                write(wr, &Element::new("challenge", ns::SASL).to_string()).await?;
                let response = next_element(rd, wr).await?;
                if !response.is("response", ns::SASL) {
                    write(wr, &failure("aborted")).await?;
                    continue;
                }
                data = response.text_content();
            }
            // "=" is the encoding of a zero-length response (§6.4.2).
            let credentials = (data != "=").then(|| sasl::plain_decode(&data)).flatten();
            let Some((authzid, authcid, password)) = credentials else {
                write(wr, &failure("malformed-request")).await?;
                continue;
            };
            // The authcid must be a valid localpart; it becomes the account JID.
            let account = Jid::parse(&format!("{authcid}@{}", self.domain))
                .ok()
                .filter(|jid| jid.resource.is_none());
            // authzid ("act as") may only be empty or the account itself.
            let authorized = account.as_ref().is_some_and(|account| {
                authzid.is_empty() || Jid::parse(&authzid).is_ok_and(|jid| jid == *account)
            });
            if let Some(user) = account.and_then(|account| account.local)
                && authorized
                && self.check_password(&user, &password).await
            {
                write(wr, &Element::new("success", ns::SASL).to_string()).await?;
                return Ok(user);
            }
            write(wr, &failure("not-authorized")).await?;
        }
        Err(fail(wr, "policy-violation").await)
    }

    /// Argon2 is slow on purpose (tens of ms): it runs on a blocking thread so
    /// it does not stall the runtime's other connections.
    async fn check_password(&self, user: &str, password: &str) -> bool {
        let (file, user, password) = (
            self.users_file.clone(),
            user.to_owned(),
            password.to_owned(),
        );
        tokio::task::spawn_blocking(move || verify_password(&file, &user, &password))
            .await
            .unwrap_or(false)
    }

    /// Resource binding (RFC 6120 §7): the connection gets a full JID and
    /// enters the routing table.
    async fn bind<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
        self: &Arc<Self>,
        rd: &mut XmlReader<R>,
        wr: &mut W,
        user: &str,
    ) -> io::Result<(Session, mpsc::Receiver<Element>)> {
        loop {
            let iq = next_element(rd, wr).await?;
            let bind = iq
                .get_child("bind", ns::BIND)
                .filter(|_| iq.is("iq", ns::CLIENT) && iq.get_attr("type") == Some("set"));
            let Some(bind) = bind else {
                // Without a full JID there is no way to route stanzas yet (§7.1, §4.9.3.12).
                return Err(fail(wr, "not-authorized").await);
            };
            // The client may suggest the resource; otherwise the server generates one.
            let resource = bind
                .get_child("resource", ns::BIND)
                .map(Element::text_content)
                .filter(|resource| !resource.is_empty())
                .unwrap_or_else(random_id);
            let Ok(jid) = Jid::parse(&format!("{user}@{}/{resource}", self.domain)) else {
                write(wr, &stanza_error(&iq, "modify", "bad-request").to_string()).await?;
                continue;
            };
            let (tx, rx) = mpsc::channel(QUEUE_SIZE);
            if !self.add_resource(&jid, tx.clone()) {
                // Resource already in use: the RFC allows refusing the new one (§7.7.2.2).
                write(wr, &stanza_error(&iq, "cancel", "conflict").to_string()).await?;
                continue;
            }
            // From here on `Session` guarantees removal from the table, even if
            // the write below fails or the negotiation deadline expires.
            let session = Session {
                server: self.clone(),
                jid,
                tx,
            };
            let bound = Element::new("jid", ns::BIND).text(&session.jid.to_string());
            let result = reply(&iq, "result").child(Element::new("bind", ns::BIND).child(bound));
            write(wr, &result.to_string()).await?;
            return Ok((session, rx));
        }
    }

    /// Routes a stanza coming from `session` (RFC 6120 §10, RFC 6121 §8).
    /// `Err(condition)` is a protocol violation: it ends the stream.
    async fn route(&self, session: &Session, mut stanza: Element) -> Result<(), &'static str> {
        if stanza.ns != ns::CLIENT || !matches!(stanza.name.as_str(), "message" | "presence" | "iq")
        {
            return Err("unsupported-stanza-type");
        }
        // §8.1.2.1: the server ALWAYS stamps `from` with the session's full JID,
        // overwriting whatever came in. That is what prevents sender spoofing.
        stanza.set_attr("from", &session.jid.to_string());
        let to = match stanza.get_attr("to").map(Jid::parse) {
            None => None,
            Some(Ok(to)) => Some(to),
            Some(Err(_)) => {
                session.bounce(&stanza, "modify", "jid-malformed").await;
                return Ok(());
            }
        };
        match stanza.name.as_str() {
            "iq" => self.route_iq(session, stanza, to).await,
            "message" => self.route_message(session, stanza, to).await,
            _ => self.route_presence(session, stanza, to).await,
        }
        Ok(())
    }

    /// `<iq/>`: request/response. Every request (`get`/`set`) must receive
    /// exactly one response (`result`/`error`), which is why every path
    /// without a destination returns an error (RFC 6120 §8.2.3).
    async fn route_iq(&self, session: &Session, iq: Element, to: Option<Jid>) {
        let kind = iq.get_attr("type").unwrap_or_default();
        let request = matches!(kind, "get" | "set");
        if iq.get_attr("id").is_none() || !(request || matches!(kind, "result" | "error")) {
            return session.bounce(&iq, "modify", "bad-request").await;
        }
        match to {
            // ponytail: no federation (s2s, server-to-server); our domain
            // only. Add it when we need to talk to other servers.
            Some(to) if to.domain != self.domain => {
                if request {
                    session
                        .bounce(&iq, "cancel", "remote-server-not-found")
                        .await;
                }
            }
            // To a specific connection: forward it (e.g. client-to-client).
            Some(to) if to.local.is_some() && to.resource.is_some() => {
                if !self.deliver_to(&to, &iq) && request {
                    session.bounce(&iq, "cancel", "service-unavailable").await;
                }
            }
            // To another account's bare JID: the server would answer on its
            // behalf (vCard, PEP...), but we do not implement those services.
            Some(to) if to.local.is_some() && to.local != session.jid.local => {
                if request {
                    session.bounce(&iq, "cancel", "service-unavailable").await;
                }
            }
            // No `to`, to the account itself or to the domain: the server answers.
            to => {
                if request {
                    let to_server = to.is_some_and(|to| to.local.is_none());
                    session.send(handle_iq(&iq, to_server)).await;
                }
            }
        }
    }

    /// `<message/>`: "fire and forget" delivery (RFC 6121 §8.5).
    async fn route_message(&self, session: &Session, message: Element, to: Option<Jid>) {
        // No `to` = message to the account itself (RFC 6120 §10.3.1).
        let to = to.unwrap_or_else(|| session.jid.bare());
        if to.domain != self.domain {
            return session
                .bounce(&message, "cancel", "remote-server-not-found")
                .await;
        }
        let Some(user) = to.local.as_deref() else {
            // Message to the server itself: there is no service that consumes it.
            return session
                .bounce(&message, "cancel", "service-unavailable")
                .await;
        };
        // Full JID of an active connection: direct delivery (§8.5.3.1).
        if to.resource.is_some() && self.deliver_to(&to, &message) {
            return;
        }
        // Bare JID, or a full JID that is no longer connected (§8.5.2, §8.5.3.2).
        match message.get_attr("type").unwrap_or("normal") {
            "groupchat" => {
                session
                    .bounce(&message, "cancel", "service-unavailable")
                    .await
            }
            // Errors never generate another error; with no recipient, they vanish.
            "error" => {}
            // headline is "best effort": if nobody is online, drop it.
            "headline" => {
                self.deliver_available(user, &message, true);
            }
            // normal/chat: to every available connection with priority >= 0.
            // Nobody online (or nonexistent account — the same answer, so as
            // not to reveal which accounts exist): error.
            // ponytail: no offline storage (XEP-0160) or history (XEP-0313);
            // they come together with a database.
            _ => {
                if self.deliver_available(user, &message, true) == 0 {
                    session
                        .bounce(&message, "cancel", "service-unavailable")
                        .await;
                }
            }
        }
    }

    /// `<presence/>`: availability and subscriptions (RFC 6121 §3-4).
    async fn route_presence(&self, session: &Session, mut presence: Element, to: Option<Jid>) {
        let kind = presence.get_attr("type").map(str::to_owned);
        match (kind.as_deref(), to) {
            // No `to`: the connection announces it is available or unavailable.
            (None | Some("unavailable"), None) => self.broadcast_presence(session, presence),
            // Probes are a server-to-server matter (§4.3); other types without a destination make no sense.
            (Some("probe"), _) | (_, None) => {}
            (_, Some(to)) if to.domain != self.domain => {
                session
                    .bounce(&presence, "cancel", "remote-server-not-found")
                    .await;
            }
            (kind, Some(to)) => {
                let Some(user) = to.local.as_deref() else {
                    return;
                };
                if matches!(
                    kind,
                    Some("subscribe" | "subscribed" | "unsubscribe" | "unsubscribed")
                ) {
                    // A subscription is between accounts, not connections: it
                    // goes out with the bare JID as sender (§3.1.2).
                    // ponytail: we only forward. Without a persistent roster
                    // there is no subscription state (none/to/from/both) and no
                    // delivery of requests to offline users (RFC 6121 §2-3).
                    presence.set_attr("from", &session.jid.bare().to_string());
                    self.deliver_available(user, &presence, false);
                } else if to.resource.is_some() {
                    // Presence directed to a specific connection (§4.6).
                    self.deliver_to(&to, &presence);
                } else {
                    self.deliver_available(user, &presence, false);
                }
            }
        }
    }

    /// Stores the connection's presence and forwards it to every available
    /// connection of the same account, including itself, which thus gets the
    /// confirmation (RFC 6121 §4.2.2). On *initial* presence, the new
    /// connection also gets the current presence of the others — the local
    /// equivalent of a "probe" (§4.3).
    ///
    /// ponytail: without a roster, contacts do not receive this broadcast.
    fn broadcast_presence(&self, session: &Session, presence: Element) {
        let available = presence.get_attr("type").is_none();
        let (user, resource) = session.key();
        let mut sessions = self.sessions();
        let Some(resources) = sessions.get_mut(user) else {
            return;
        };
        let Some(me) = resources.get_mut(resource) else {
            return;
        };
        let initial = available && me.presence.is_none();
        me.presence = available.then(|| presence.clone());
        let my_tx = me.tx.clone();
        for (name, other) in resources.iter() {
            let Some(other_presence) = &other.presence else {
                continue;
            };
            let _ = other.tx.try_send(presence.clone());
            if initial && name != resource {
                let _ = my_tx.try_send(other_presence.clone());
            }
        }
    }

    fn sessions(&self) -> MutexGuard<'_, Sessions> {
        // A panic while holding the lock must not take down every other connection.
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers the full JID in the table. `false` if the resource is already in use.
    fn add_resource(&self, jid: &Jid, tx: mpsc::Sender<Element>) -> bool {
        let (Some(user), Some(resource)) = (&jid.local, &jid.resource) else {
            return false;
        };
        let mut sessions = self.sessions();
        let resources = sessions.entry(user.clone()).or_default();
        if resources.contains_key(resource) {
            return false;
        }
        resources.insert(resource.clone(), Resource { tx, presence: None });
        true
    }

    /// Delivers to a specific connection. `false` if it does not exist or its
    /// queue is full (`try_send` never blocks the sender).
    fn deliver_to(&self, to: &Jid, stanza: &Element) -> bool {
        let sessions = self.sessions();
        let resource = to
            .local
            .as_ref()
            .and_then(|user| sessions.get(user))
            .and_then(|resources| resources.get(to.resource.as_deref()?));
        resource.is_some_and(|r| r.tx.try_send(stanza.clone()).is_ok())
    }

    /// Delivers to every *available* connection of an account; with
    /// `skip_negative`, skips those with negative priority, which by definition
    /// do not want messages addressed to the bare JID (RFC 6121 §4.7.2.3).
    /// Returns how many received it.
    fn deliver_available(&self, user: &str, stanza: &Element, skip_negative: bool) -> usize {
        let sessions = self.sessions();
        let Some(resources) = sessions.get(user) else {
            return 0;
        };
        resources
            .values()
            .filter(|r| {
                r.presence
                    .as_ref()
                    .is_some_and(|p| !skip_negative || priority(p) >= 0)
            })
            .filter(|r| r.tx.try_send(stanza.clone()).is_ok())
            .count()
    }
}

/// A bound session. When dropped (normal end, error, timeout or panic), it
/// leaves the routing table and notifies the account's other connections.
struct Session {
    server: Arc<Server>,
    jid: Jid,
    tx: mpsc::Sender<Element>,
}

impl Session {
    fn key(&self) -> (&str, &str) {
        let user = self.jid.local.as_deref().unwrap_or_default();
        (user, self.jid.resource.as_deref().unwrap_or_default())
    }

    /// Pushes onto our own outgoing queue. Waiting here is desirable: if the
    /// client does not read the responses, we stop reading its requests (backpressure).
    async fn send(&self, stanza: Element) {
        let _ = self.tx.send(stanza).await;
    }

    /// Returns a stanza error to the sender — never in response to another
    /// error (RFC 6120 §8.3.1), which would create loops.
    async fn bounce(&self, stanza: &Element, kind: &str, condition: &str) {
        if stanza.get_attr("type") != Some("error") {
            self.send(stanza_error(stanza, kind, condition)).await;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let (user, resource) = self.key();
        let mut sessions = self.server.sessions();
        let Some(resources) = sessions.get_mut(user) else {
            return;
        };
        let was_available = resources
            .remove(resource)
            .is_some_and(|r| r.presence.is_some());
        if was_available {
            // RFC 6121 §4.5.2: the account's other connections see this one leave.
            let unavailable = Element::new("presence", ns::CLIENT)
                .attr("from", &self.jid.to_string())
                .attr("type", "unavailable");
            for other in resources.values().filter(|r| r.presence.is_some()) {
                let _ = other.tx.try_send(unavailable.clone());
            }
        }
        if resources.is_empty() {
            sessions.remove(user);
        }
    }
}

/// Services the server itself answers.
fn handle_iq(iq: &Element, to_server: bool) -> Element {
    let kind = iq.get_attr("type").unwrap_or_default();
    // get/set carry exactly one child, which says what is being asked (§8.2.3).
    let payload = iq
        .elements()
        .next()
        .map(|p| (kind, p.name.as_str(), p.ns.as_str()));
    match payload {
        // XEP-0199: ping (keepalive and latency measurement).
        Some(("get", "ping", ns::PING)) => reply(iq, "result"),
        // Legacy RFC 3921: "establish session" does nothing nowadays.
        Some(("set", "session", ns::SESSION)) => reply(iq, "result"),
        // RFC 6121 §2.2: the account's own roster. No persistence: empty.
        Some(("get", "query", ns::ROSTER)) if !to_server => {
            reply(iq, "result").child(Element::new("query", ns::ROSTER))
        }
        // XEP-0030: what this server is and supports.
        Some(("get", "query", ns::DISCO_INFO)) if to_server => {
            let feature = |var| Element::new("feature", ns::DISCO_INFO).attr("var", var);
            let identity = Element::new("identity", ns::DISCO_INFO)
                .attr("category", "server")
                .attr("type", "im");
            let query = Element::new("query", ns::DISCO_INFO)
                .child(identity)
                .child(feature(ns::DISCO_INFO))
                .child(feature(ns::PING));
            reply(iq, "result").child(query)
        }
        // Anything else: "we do not offer that" (§8.4).
        _ => stanza_error(iq, "cancel", "service-unavailable"),
    }
}

/// Priority of a presence (-128..127, default 0; RFC 6121 §4.7.2.3).
fn priority(presence: &Element) -> i8 {
    presence
        .get_child("priority", ns::CLIENT)
        .and_then(|p| p.text_content().trim().parse().ok())
        .unwrap_or(0)
}

/// Reads the next element during negotiation. XML errors become a stream
/// error sent to the client; EOF becomes `UnexpectedEof`.
async fn next_element<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    rd: &mut XmlReader<R>,
    wr: &mut W,
) -> io::Result<Element> {
    match rd.next().await {
        Ok(Event::Element(el)) => Ok(el),
        Ok(_) => Err(io::ErrorKind::UnexpectedEof.into()),
        Err(xmpp_core::Error::Io(e)) => Err(e),
        Err(e) => Err(fail(wr, e.condition()).await),
    }
}

/// Sends a stream error and closes the stream (RFC 6120 §4.9). Returns the
/// `io::Error` that the negotiation propagates with `?`.
async fn fail<W: AsyncWrite + Unpin>(wr: &mut W, condition: &'static str) -> io::Error {
    let _ = write(wr, &format!("{}{STREAM_CLOSE}", stream_error(condition))).await;
    let _ = wr.shutdown().await;
    io::Error::other(format!("erro de stream enviado ao cliente: {condition}"))
}

/// Writes and flushes: TLS may hold bytes in its buffer until the flush.
async fn write<W: AsyncWrite + Unpin>(wr: &mut W, data: &str) -> io::Result<()> {
    wr.write_all(data.as_bytes()).await?;
    wr.flush().await
}

/// 128 random bits in hex: stream ids and generated resources. They must be
/// unpredictable (RFC 6120 §4.7.3), so they come from the OS RNG.
fn random_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("RNG do sistema operacional indisponível");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Argon2id hash in PHC format (`$argon2id$v=19$m=...$salt$hash`), with a
/// random salt. This is what goes into the users file.
pub fn hash_password(password: &str) -> String {
    Argon2::default()
        .hash_password(password.as_bytes())
        .expect("falha ao gerar hash Argon2")
        .to_string()
}

/// Checks the password against the users file (`user:hash` per line). The
/// file is re-read on every login: `xmpp-server adduser` takes effect without a restart.
fn verify_password(file: &Path, user: &str, password: &str) -> bool {
    let users = fs::read_to_string(file).unwrap_or_else(|e| {
        eprintln!("não consegui ler {}: {e}", file.display());
        String::new()
    });
    let hash = users
        .lines()
        .find_map(|line| line.split_once(':').filter(|(name, _)| *name == user))
        .map(|(_, hash)| hash.to_owned());
    let exists = hash.is_some();
    // A nonexistent account still pays the cost of one Argon2 run (against the
    // hash of a random password): response time does not reveal which accounts exist.
    let hash = hash.unwrap_or_else(|| dummy_hash().to_owned());
    let valid = PasswordHash::new(&hash).is_ok_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    });
    valid && exists
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password(&random_id()))
}
