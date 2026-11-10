# Two-stage xmpp-server image: builds with the full Rust toolchain and copies
# only the binary into a Debian slim image (the toolchain is left behind).

# ── Stage 1: build ────────────────────────────────────────────────────────────
FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY . .
# ponytail: no dependency cache between builds (it would need BuildKit or
# stub crates); every build recompiles everything (~1 min).
RUN cargo build --release --locked -p xmpp-server

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
# Same Debian release as the build stage, so glibc is compatible.
FROM debian:trixie-slim
COPY --from=build /src/target/release/xmpp-server /usr/local/bin/xmpp-server
# Unprivileged user; /var/lib/xmpp holds the accounts file (volume).
RUN useradd --system --home-dir /var/lib/xmpp --create-home xmpp
USER xmpp
WORKDIR /var/lib/xmpp
ENV XMPP_LISTEN=0.0.0.0:5222 \
    XMPP_TLS_CERT=/etc/xmpp/cert.pem \
    XMPP_TLS_KEY=/etc/xmpp/key.pem \
    XMPP_USERS=/var/lib/xmpp/users.txt
# 5222: XMPP's standard client-to-server port, registered with IANA.
EXPOSE 5222
ENTRYPOINT ["xmpp-server"]
