# Noxy

A TLS man-in-the-middle proxy with a pluggable HTTP middleware pipeline. Built on [tower](https://crates.io/crates/tower), noxy gives you full access to decoded HTTP requests and responses flowing through the proxy using standard tower `Service` and `Layer` abstractions -- including all existing [tower-http](https://crates.io/crates/tower-http) middleware out of the box.

## Features

- **Tower middleware pipeline** -- plug in any tower `Layer` or `Service` to inspect and modify HTTP traffic. Works with tower-http layers (compression, tracing, CORS, etc.) and your own custom services.
- Per-host certificate generation on the fly, signed by a user-provided CA
- HTTP/1.1 and HTTP/2 support (auto-negotiated via ALPN)
- Streaming bodies -- middleware can process data as it arrives without buffering
- Upstream TLS verification using Mozilla root certificates
- Async I/O with Tokio and Hyper

## Library Usage

```rust
use noxy::Proxy;

// A custom tower layer that adds a header to every response
let proxy = Proxy::builder()
    .ca_pem_files("ca-cert.pem", "ca-key.pem")?
    .http_layer(my_tower_layer)
    .build();

proxy.listen("127.0.0.1:8080").await?;
```

Any tower `Layer<HttpService>` works. The innermost service forwards requests to the upstream server; your layers wrap around it in an onion model and can inspect or modify requests before forwarding and responses after.

## Quick Start

### 1. Generate a CA certificate

```bash
cargo run --features cli -- --generate
```

Or with OpenSSL:

```bash
openssl req -x509 -newkey rsa:2048 -keyout ca-key.pem -out ca-cert.pem -days 365 -nodes -subj "/CN=Noxy CA"
```

### 2. Run the proxy

```bash
cargo run --features cli
```

### 3. Make a request through the proxy

```bash
curl --proxy http://127.0.0.1:8080 --cacert ca-cert.pem https://example.com
```

### Trusting the CA system-wide

Instead of passing `--cacert` every time, you can install `ca-cert.pem` into your OS or browser trust store. This lets any application use the proxy transparently.

**Important:** Only do this in development/testing environments. Remove the CA when you're done.

## How It Works

Normal HTTPS creates an encrypted tunnel between client and server -- nobody in the middle can read the traffic. Noxy breaks that tunnel into **two separate TLS sessions** and sits in between, with your middleware pipeline processing decoded HTTP traffic.

### The flow

```
Client (curl)            Noxy                       Server (example.com)
     |                     |                               |
     |--- CONNECT host --->|                               |
     |<-- 200 OK ---------|                               |
     |                     |--- TLS handshake ------------>|
     |                     |   (real cert verified)        |
     |                     |                               |
     |   Noxy generates a  |                               |
     |   fake cert for     |                               |
     |   "example.com"     |                               |
     |   signed by our CA  |                               |
     |                     |                               |
     |<- TLS handshake --->|                               |
     |   (fake cert)       |                               |
     |                     |                               |
     |== TLS session 1 ====|====== TLS session 2 =========|
     |                     |                               |
     | "GET / HTTP/1.1"    |  tower middleware pipeline     |
     |--- encrypted ------>|  [Layer] -> [Layer] -> upstream
     |                     |                               |
     |                     |           response + layers   |
     |<--- re-encrypted ---|  upstream -> [Layer] -> [Layer]
```

### Step by step

1. **HTTP CONNECT** -- the client sends an unencrypted `CONNECT example.com:443` request to the proxy. The proxy learns the target hostname from this plaintext request.

2. **Upstream TLS** -- Noxy opens a real TLS connection to `example.com`, verifying the server's authentic certificate against Mozilla's root CAs.

3. **Fake certificate generation** -- Noxy generates a TLS certificate for `example.com` signed by the user-provided CA, created on the fly per host.

4. **Client TLS** -- Noxy performs a TLS handshake with the client using the fake certificate. The client accepts it because it trusts the CA.

5. **HTTP relay with middleware** -- with both TLS sessions established, Hyper handles the HTTP connection on both sides. Each request from the client passes through your tower middleware pipeline before being forwarded upstream, and each response passes back through the pipeline before being sent to the client.

## License

MIT
