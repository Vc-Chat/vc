//! Server binary. Configured through environment variables, which is
//! convenient in containers:
//!
//! | variable        | default          | what it is                                 |
//! |-----------------|------------------|--------------------------------------------|
//! | `XMPP_DOMAIN`   | `localhost`      | served domain (what comes after the `@`)   |
//! | `XMPP_LISTEN`   | `0.0.0.0:5222`   | c2s address (5222 is the standard port)    |
//! | `XMPP_TLS_CERT` | `certs/cert.pem` | the domain's PEM certificate chain         |
//! | `XMPP_TLS_KEY`  | `certs/key.pem`  | PEM private key                            |
//! | `XMPP_USERS`    | `users.txt`      | accounts file (`user:argon2-hash`)         |
//!
//! ```text
//! xmpp-server                 # starts the server
//! xmpp-server adduser alice   # creates the account; the password is read from stdin
//! ```

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::{self, pki_types::CertificateDer, pki_types::PrivateKeyDer};
use xmpp_core::Jid;
use xmpp_server::{Server, hash_password};

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

#[tokio::main]
async fn main() -> ExitCode {
    let domain = env("XMPP_DOMAIN", "localhost");
    let users = env("XMPP_USERS", "users.txt");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => serve(&domain, &users).await,
        ["adduser", user] => adduser(&domain, &users, user),
        _ => Err(io::Error::other("uso: xmpp-server [adduser <usuario>]")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("erro: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(domain: &str, users: &str) -> io::Result<()> {
    let tls = load_tls(
        &env("XMPP_TLS_CERT", "certs/cert.pem"),
        &env("XMPP_TLS_KEY", "certs/key.pem"),
    )?;
    let listen = env("XMPP_LISTEN", "0.0.0.0:5222");
    let listener = TcpListener::bind(&listen).await?;
    eprintln!("xmpp-server: domínio {domain}, escutando em {listen}");
    Server::new(domain, users.into(), tls).serve(listener).await;
    Ok(())
}

/// Loads the PEM certificate and key. The certificate must be valid for the
/// XMPP domain: that is what clients validate TLS against.
fn load_tls(cert: &str, key: &str) -> io::Result<Arc<rustls::ServerConfig>> {
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
        .map_err(|e| io::Error::other(format!("{cert}: {e}")))?;
    let key =
        PrivateKeyDer::from_pem_file(key).map_err(|e| io::Error::other(format!("{key}: {e}")))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;
    Ok(Arc::new(config))
}

/// Appends `user:hash` to the accounts file.
fn adduser(domain: &str, users: &str, user: &str) -> io::Result<()> {
    // The name must be a valid JID localpart; it comes out normalized.
    let user = Jid::parse(&format!("{user}@{domain}"))
        .ok()
        .filter(|jid| jid.resource.is_none())
        .and_then(|jid| jid.local)
        .ok_or_else(|| io::Error::other("nome de usuário inválido"))?;
    let existing = fs::read_to_string(users).unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.split_once(':').is_some_and(|(name, _)| name == user))
    {
        return Err(io::Error::other(format!("{user} já existe em {users}")));
    }
    // The password comes from stdin (not from an argument, which would end up
    // in the shell history and be visible in `ps`). Note: the terminal echoes it.
    eprint!("senha de {user}@{domain}: ");
    let mut password = String::new();
    io::stdin().lock().read_line(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        return Err(io::Error::other("senha vazia"));
    }
    // 0600: hashes are not passwords, but there is no reason for other system
    // users to read the file. (Unix only — the server runs on Linux/macOS.)
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(users)?;
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    writeln!(file, "{separator}{user}:{}", hash_password(password))?;
    eprintln!("conta {user}@{domain} criada");
    Ok(())
}
