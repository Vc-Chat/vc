# vc

Implementação do protocolo **XMPP** (RFC 6120/6121) em Rust: servidor,
cliente e as primitivas do protocolo, mais a infra para rodar.

| crate | o que é |
|---|---|
| [`xmpp-core`](crates/xmpp-core) | JIDs (RFC 7622), árvore XML, leitor incremental do stream com as restrições de segurança do XMPP, erros de stream/stanza, SASL PLAIN |
| [`xmpp-server`](crates/xmpp-server) | servidor c2s: STARTTLS obrigatório → SASL PLAIN (senhas em Argon2id) → bind → roteamento de `message`/`presence`/`iq` |
| [`xmpp-client`](crates/xmpp-client) | biblioteca de cliente + CLI para testar à mão; funciona com qualquer servidor XMPP |

Cada arquivo começa com um comentário explicando a parte do protocolo que
implementa, com referência às seções da RFC.

## Rodando localmente

```sh
scripts/dev-cert.sh                          # certificado autoassinado em ./certs
echo 'senha-a' | cargo run -p xmpp-server -- adduser alice
echo 'senha-b' | cargo run -p xmpp-server -- adduser bob
cargo run -p xmpp-server                     # escuta em 0.0.0.0:5222
```

Em outros dois terminais:

```sh
XMPP_CA=certs/cert.pem XMPP_PASSWORD=senha-b cargo run -p xmpp-client -- bob@localhost 127.0.0.1:5222
XMPP_CA=certs/cert.pem XMPP_PASSWORD=senha-a cargo run -p xmpp-client -- alice@localhost 127.0.0.1:5222
```

No terminal de alice, digite `bob@localhost oi bob`. Linhas começando com `<`
vão cruas ao servidor, por exemplo
`<iq type='get' id='1' to='localhost'><ping xmlns='urn:xmpp:ping'/></iq>`.

Clientes XMPP comuns (Gajim, Conversations, Dino...) também conectam, desde
que confiem no certificado.

## Rodando com Docker

```sh
scripts/dev-cert.sh
echo 'senha-a' | docker compose run --rm -T xmpp adduser alice
docker compose up --build -d
```

As contas ficam no volume `xmpp-data`; o certificado é montado de `./certs`.

## Configuração

Variáveis de ambiente do `xmpp-server`:

| variável | padrão | |
|---|---|---|
| `XMPP_DOMAIN` | `localhost` | domínio servido (o que vem depois do `@`) |
| `XMPP_LISTEN` | `0.0.0.0:5222` | endereço c2s |
| `XMPP_TLS_CERT` / `XMPP_TLS_KEY` | `certs/cert.pem` / `certs/key.pem` | certificado e chave PEM do domínio |
| `XMPP_USERS` | `users.txt` | contas, uma por linha: `usuario:hash-argon2` |

## Produção

1. **Certificado** de uma CA para o domínio XMPP (ex.: Let's Encrypt para
   `chat.exemplo.com`); aponte `XMPP_TLS_CERT`/`XMPP_TLS_KEY` para ele.
2. **DNS**: os clientes descobrem o servidor pelo registro SRV (RFC 6120
   §3.2). Se o servidor roda no próprio host do domínio, um `A`/`AAAA` basta.
   ```
   _xmpp-client._tcp.exemplo.com. 3600 IN SRV 5 0 5222 chat.exemplo.com.
   ```
3. **Firewall**: liberar TCP 5222.

## Testes

```sh
cargo test     # unitários + ponta a ponta (servidor real, 2 clientes, TLS)
```

O CI (`.github/workflows/ci.yml`) roda fmt, clippy, testes e o build da imagem.

## Cobertura do protocolo

Implementado: stream XML com XML restrito e limite de 256 KiB por stanza,
STARTTLS obrigatório, SASL PLAIN, bind de recurso, carimbo de `from`,
roteamento para full/bare JID com prioridade, erros de stanza e de stream,
presença entre as conexões da mesma conta, ping (XEP-0199), disco#info
(XEP-0030), roster vazio.

Ainda não (marcado com `ponytail:` no código, onde entraria):

- roster persistente e estados de inscrição (RFC 6121 §2-3) — hoje a presença
  não chega aos contatos;
- mensagens offline (XEP-0160) e histórico (XEP-0313) — precisam de banco;
- federação com outros servidores (s2s);
- SCRAM-SHA-256, TLS direto na porta 5223 (XEP-0368), DNS SRV no cliente;
- normalização PRECIS completa de JIDs.
