//! End-to-end test: a real server on a local port, two real clients, real
//! TLS (certificate generated on the fly) and real Argon2.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_rustls::rustls::{self, pki_types::PrivateKeyDer};
use xmpp_client::{Client, tls_config};
use xmpp_core::{Element, Jid, STREAM_CLOSE, ns};
use xmpp_server::{Server, hash_password};

/// Next stanza, failing instead of hanging the test forever.
async fn recv(client: &mut Client) -> Element {
    tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .expect("timeout esperando stanza")
        .unwrap()
        .expect("stream fechado")
}

fn jid(s: &str) -> Jid {
    s.parse().unwrap()
}

#[tokio::test]
async fn login_routing_and_errors() {
    // Self-signed certificate for "localhost": the server presents it, the client trusts it.
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    let server_tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .unwrap();
    let client_tls = tls_config([cert.cert.der().clone()]).unwrap();

    let users = std::env::temp_dir().join(format!("xmpp-e2e-{}.txt", std::process::id()));
    std::fs::write(
        &users,
        format!("alice:{}\nbob:{}\n", hash_password("a"), hash_password("b")),
    )
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(Server::new("localhost", users.clone(), Arc::new(server_tls)).serve(listener));
    let connect = async |who: &str, password: &str| {
        Client::connect(&jid(who), password, Some(&addr), client_tls.clone()).await
    };

    // Wrong password and nonexistent account: rejected during SASL.
    assert!(connect("alice@localhost", "errada").await.is_err());
    assert!(connect("carol@localhost", "x").await.is_err());

    // A resource requested by the client is honored; without one, the server generates it.
    let mut alice = connect("Alice@localhost/laptop", "a").await.unwrap();
    let mut bob = connect("bob@localhost", "b").await.unwrap();
    assert_eq!(alice.jid.to_string(), "alice@localhost/laptop");
    assert!(bob.jid.resource.is_some());
    // The same resource twice: bind conflict.
    assert!(connect("alice@localhost/laptop", "a").await.is_err());

    // Initial presence is echoed back to the connection itself (RFC 6121 §4.2.2).
    bob.send(&Element::new("presence", ns::CLIENT))
        .await
        .unwrap();
    assert!(recv(&mut bob).await.is("presence", ns::CLIENT));

    // A message to the bare JID arrives with `from` stamped by the server,
    // even if alice tries to spoof the sender.
    let body = Element::new("body", ns::CLIENT).text("oi <bob> & cia");
    let message = Element::new("message", ns::CLIENT)
        .attr("to", "bob@localhost")
        .attr("from", "mallory@localhost")
        .attr("type", "chat")
        .child(body);
    alice.send(&message).await.unwrap();
    let got = recv(&mut bob).await;
    assert_eq!(got.get_attr("from"), Some("alice@localhost/laptop"));
    assert_eq!(
        got.get_child("body", ns::CLIENT).unwrap().text_content(),
        "oi <bob> & cia"
    );

    // Nonexistent/offline recipient: a service-unavailable error comes back.
    let to_carol = Element::new("message", ns::CLIENT)
        .attr("to", "carol@localhost")
        .attr("id", "m2")
        .child(Element::new("body", ns::CLIENT).text("?"));
    alice.send(&to_carol).await.unwrap();
    let error = recv(&mut alice).await;
    assert_eq!(
        (error.get_attr("type"), error.get_attr("id")),
        (Some("error"), Some("m2"))
    );
    assert!(
        error
            .get_child("error", ns::CLIENT)
            .unwrap()
            .get_child("service-unavailable", ns::STANZA_ERRORS)
            .is_some()
    );

    // iq: a ping to the server gets a result.
    let ping = Element::new("iq", ns::CLIENT)
        .attr("type", "get")
        .attr("id", "p1")
        .attr("to", "localhost")
        .child(Element::new("ping", ns::PING));
    alice.send(&ping).await.unwrap();
    let pong = recv(&mut alice).await;
    assert_eq!(
        (pong.get_attr("type"), pong.get_attr("id")),
        (Some("result"), Some("p1"))
    );

    // Bob closes his stream; the server answers by closing its own (RFC 6120 §4.4).
    bob.writer.write_all(STREAM_CLOSE.as_bytes()).await.unwrap();
    bob.writer.flush().await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), bob.recv())
        .await
        .unwrap();
    assert!(closed.unwrap().is_none());

    // No federation: another domain gets remote-server-not-found.
    let federated = Element::new("message", ns::CLIENT)
        .attr("to", "x@outro.example")
        .attr("id", "m3");
    alice.send(&federated).await.unwrap();
    let error = recv(&mut alice).await;
    assert_eq!(error.get_attr("id"), Some("m3"));
    assert!(error.to_string().contains("remote-server-not-found"));

    let _ = std::fs::remove_file(users);
}
