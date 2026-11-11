//! Bridge between the GTK UI and the XMPP connection.
//!
//! [`run`] lives on the tokio runtime: it connects with `xmpp-client`, turns
//! incoming stanzas into [`Event`]s for the UI and outgoing [`Command`]s from
//! the UI into stanzas. The UI never touches sockets, and the network side
//! never touches widgets (GTK widgets must stay on the main thread).

use std::path::PathBuf;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use xmpp_client::{Client, load_ca, send, tls_config};
use xmpp_core::{Element, Event as StreamEvent, Jid, STREAM_CLOSE, ns};

/// What the login screen collects.
pub struct Credentials {
    pub jid: Jid,
    pub password: String,
    /// `host:port`; `None` connects to `<domain>:5222`.
    pub server: Option<String>,
    /// Extra trusted certificate (PEM), e.g. the development self-signed one.
    pub ca: Option<PathBuf>,
}

/// UI → network.
pub enum Command {
    /// Send a chat message to a bare JID.
    Send { to: String, body: String },
}

/// Network → UI.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Logged in and bound; carries our full JID.
    Connected(Jid),
    /// A chat message from `from` (bare JID).
    Message { from: String, body: String },
    /// The server bounced a message we sent to `to` (bare JID), e.g. because
    /// the contact is offline (`service-unavailable`).
    Undelivered { to: String, condition: String },
    /// The connection failed or ended; the text explains why.
    Closed(String),
}

/// Runs one XMPP session until the server closes it or the UI drops its
/// command sender (logout/window closed).
pub async fn run(
    credentials: Credentials,
    mut commands: UnboundedReceiver<Command>,
    events: UnboundedSender<Event>,
) {
    let client = match connect(&credentials).await {
        Ok(client) => client,
        Err(e) => {
            let _ = events.send(Event::Closed(format!("Falha ao conectar: {e}")));
            return;
        }
    };
    let Client {
        jid,
        mut reader,
        mut writer,
    } = client;
    // Initial presence (RFC 6121 §4.2): without it the server does not
    // deliver messages addressed to our bare JID.
    if let Err(e) = send(&mut writer, &Element::new("presence", ns::CLIENT)).await {
        let _ = events.send(Event::Closed(format!("Conexão perdida: {e}")));
        return;
    }
    let _ = events.send(Event::Connected(jid));

    // Reading gets its own task: `XmlReader::next` is not cancel-safe, so it
    // cannot share a `select!` with the command channel.
    let incoming = events.clone();
    let reading = tokio::spawn(async move {
        let reason = loop {
            match reader.next().await {
                Ok(StreamEvent::Element(stanza)) => {
                    if let Some(event) = event_for(&stanza) {
                        let _ = incoming.send(event);
                    }
                }
                Ok(_) => break "O servidor encerrou a conexão.".to_owned(),
                Err(e) => break format!("Conexão perdida: {e}"),
            }
        };
        let _ = incoming.send(Event::Closed(reason));
    });

    while let Some(Command::Send { to, body }) = commands.recv().await {
        let message = Element::new("message", ns::CLIENT)
            .attr("to", &to)
            .attr("type", "chat")
            .child(Element::new("body", ns::CLIENT).text(&body));
        if send(&mut writer, &message).await.is_err() {
            break;
        }
    }
    // The UI dropped its sender: end the stream politely (RFC 6120 §4.4).
    let _ = writer.write_all(STREAM_CLOSE.as_bytes()).await;
    let _ = writer.flush().await;
    reading.abort();
}

async fn connect(credentials: &Credentials) -> std::io::Result<Client> {
    let roots = match &credentials.ca {
        Some(path) => load_ca(path)?,
        None => Vec::new(),
    };
    let server = credentials.server.as_deref();
    Client::connect(
        &credentials.jid,
        &credentials.password,
        server,
        tls_config(roots)?,
    )
    .await
}

/// Maps an incoming stanza to what the UI cares about. Conversations are
/// keyed by bare JID, so every connection of a contact lands in the same DM.
/// ponytail: presence is ignored — the server does not broadcast it to
/// contacts yet (no roster), so there is nothing to show.
fn event_for(stanza: &Element) -> Option<Event> {
    if !stanza.is("message", ns::CLIENT) {
        return None;
    }
    let peer = Jid::parse(stanza.get_attr("from")?)
        .ok()?
        .bare()
        .to_string();
    if stanza.get_attr("type") == Some("error") {
        // <error type='cancel'><service-unavailable/></error>: the child names the reason.
        let condition = stanza
            .get_child("error", ns::CLIENT)
            .and_then(|error| error.elements().next())
            .map_or_else(|| "erro".to_owned(), |c| c.name.clone());
        return Some(Event::Undelivered {
            to: peer,
            condition,
        });
    }
    // Messages without <body/> (typing notifications, receipts...) are not shown.
    let body = stanza.get_child("body", ns::CLIENT)?.text_content();
    Some(Event::Message { from: peer, body })
}

#[test]
fn maps_stanzas_to_ui_events() {
    let body = |text| Element::new("body", ns::CLIENT).text(text);
    let message = Element::new("message", ns::CLIENT)
        .attr("from", "Bob@localhost/phone")
        .child(body("oi"));
    assert_eq!(
        event_for(&message),
        Some(Event::Message {
            from: "bob@localhost".into(),
            body: "oi".into()
        })
    );

    let bounce = xmpp_core::stanza_error(
        &Element::new("message", ns::CLIENT)
            .attr("to", "carol@localhost")
            .attr("from", "alice@localhost/pc"),
        "cancel",
        "service-unavailable",
    );
    assert_eq!(
        event_for(&bounce),
        Some(Event::Undelivered {
            to: "carol@localhost".into(),
            condition: "service-unavailable".into()
        })
    );

    let typing = Element::new("message", ns::CLIENT).attr("from", "bob@localhost");
    assert_eq!(event_for(&typing), None);
    let presence = Element::new("presence", ns::CLIENT).attr("from", "bob@localhost");
    assert_eq!(event_for(&presence), None);
}
