# vc

An implementation of the **XMPP** protocol (RFC 6120/6121) in Rust: server,
client and the protocol primitives, plus the infrastructure to run them.

| crate | what it is |
|---|---|
| [`xmpp-core`](crates/xmpp-core) | JIDs (RFC 7622), XML tree, incremental stream reader with XMPP's security restrictions, stream/stanza errors, SASL PLAIN |
| [`xmpp-server`](crates/xmpp-server) | c2s server: mandatory STARTTLS → SASL PLAIN (Argon2id password hashes) → bind → `message`/`presence`/`iq` routing |
| [`xmpp-client`](crates/xmpp-client) | client library + CLI for manual testing; works with any XMPP server |
| [`vc-gui`](crates/vc-gui) | Discord-like desktop client in GTK 4, built on `xmpp-client` |

Every file starts with a comment explaining the part of the protocol it
implements, with references to the RFC sections.

## Running locally

```sh
scripts/dev-cert.sh                          # self-signed certificate in ./certs
echo 'secret-a' | cargo run -p xmpp-server -- adduser alice
echo 'secret-b' | cargo run -p xmpp-server -- adduser bob
cargo run -p xmpp-server                     # listens on 0.0.0.0:5222
```

In two other terminals:

```sh
XMPP_CA=certs/cert.pem XMPP_PASSWORD=secret-b cargo run -p xmpp-client -- bob@localhost 127.0.0.1:5222
XMPP_CA=certs/cert.pem XMPP_PASSWORD=secret-a cargo run -p xmpp-client -- alice@localhost 127.0.0.1:5222
```

In alice's terminal, type `bob@localhost hi bob`. Lines starting with `<` go
raw to the server, for example
`<iq type='get' id='1' to='localhost'><ping xmlns='urn:xmpp:ping'/></iq>`.

Regular XMPP clients (Gajim, Conversations, Dino...) can connect too, as long
as they trust the certificate.

## Desktop client (vc-gui)

A GTK 4 desktop client, written entirely in Rust, with a Discord-inspired
layout: server rail, direct messages, chat and a light/dark theme (☀/☾ button
in the user panel). Accent color `#F5789A`; background `#2B2D31` (dark) and
`#FFFFFF` (light).

```sh
brew install gtk4                 # macOS   (Debian/Ubuntu: apt install libgtk-4-dev)
cargo run -p vc-gui
```

On the login screen, use for example `alice@localhost`, server
`127.0.0.1:5222` and certificate `certs/cert.pem`. To open a conversation,
type the contact's JID in the sidebar search field and press Enter.

## Running with Docker

```sh
scripts/dev-cert.sh
echo 'secret-a' | docker compose run --rm -T xmpp adduser alice
docker compose up --build -d
```

Accounts live in the `xmpp-data` volume; the certificate is mounted from
`./certs`.

## Configuration

`xmpp-server` environment variables:

| variable | default | |
|---|---|---|
| `XMPP_DOMAIN` | `localhost` | served domain (what comes after the `@`) |
| `XMPP_LISTEN` | `0.0.0.0:5222` | c2s address |
| `XMPP_TLS_CERT` / `XMPP_TLS_KEY` | `certs/cert.pem` / `certs/key.pem` | the domain's PEM certificate and key |
| `XMPP_USERS` | `users.txt` | accounts, one per line: `user:argon2-hash` |

## Production

1. **Certificate** from a CA for the XMPP domain (e.g. Let's Encrypt for
   `chat.example.com`); point `XMPP_TLS_CERT`/`XMPP_TLS_KEY` to it.
2. **DNS**: clients discover the server through the SRV record (RFC 6120
   §3.2). If the server runs on the domain's own host, an `A`/`AAAA` record
   is enough.
   ```
   _xmpp-client._tcp.example.com. 3600 IN SRV 5 0 5222 chat.example.com.
   ```
3. **Firewall**: open TCP 5222.

## Tests

```sh
cargo test     # unit tests + end-to-end (real server, 2 clients, TLS)
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, the tests and the image build.

## Protocol coverage

Implemented: XML stream with restricted XML and a 256 KiB per-stanza limit,
mandatory STARTTLS, SASL PLAIN, resource binding, `from` stamping, routing
to full/bare JIDs with priority, stanza and stream errors, presence among
the same account's connections, ping (XEP-0199), disco#info (XEP-0030),
empty roster.

Not yet (marked with `ponytail:` in the code, where it would go):

- persistent roster and subscription states (RFC 6121 §2-3) — presence does
  not reach contacts yet;
- offline messages (XEP-0160) and history (XEP-0313) — they need a database;
- federation with other servers (s2s);
- SCRAM-SHA-256, direct TLS on port 5223 (XEP-0368), DNS SRV in the client;
- full PRECIS normalization of JIDs.
