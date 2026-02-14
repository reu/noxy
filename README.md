# Noxy

A minimal man-in-the-middle TLS proxy built with Rust and Rustls. Noxy operates as an explicit HTTP CONNECT proxy that decrypts, inspects, and logs HTTPS traffic flowing through it.

## Features

- Explicit HTTP CONNECT proxy on `127.0.0.1:8080`
- Per-host certificate generation on the fly, signed by a user-provided CA
- Upstream TLS verification using Mozilla root certificates
- Bidirectional plaintext logging of decrypted request/response data
- Async I/O with Tokio

## Quick Start

### 1. Generate a CA certificate

```bash
openssl req -x509 -newkey rsa:2048 -keyout ca-key.pem -out ca-cert.pem -days 365 -nodes -subj "/CN=Noxy CA"
```

### 2. Build and run

```bash
cargo build
cargo run
```

### 3. Make a request through the proxy

```bash
curl --proxy http://127.0.0.1:8080 --cacert ca-cert.pem https://example.com
```

The proxy will print the decrypted HTTP request and response to stdout.

### Trusting the CA system-wide

Instead of passing `--cacert` every time, you can install `ca-cert.pem` into your OS or browser trust store. This lets any application use the proxy transparently.

**Important:** Only do this in development/testing environments. Remove the CA when you're done.

## How It Works

Normal HTTPS creates an encrypted tunnel between client and server -- nobody in the middle can read the traffic. Noxy breaks that tunnel into **two separate TLS sessions** and sits in between.

### The flow

```
Client (curl)            Noxy                  Server (example.com)
     |                     |                          |
     |--- CONNECT host --->|                          |
     |<-- 200 OK ---------|                          |
     |                     |--- TCP + TLS handshake ->|
     |                     |   (real cert verified)   |
     |                     |                          |
     |   Noxy generates a  |                          |
     |   fake cert for     |                          |
     |   "example.com"     |                          |
     |   signed by our CA  |                          |
     |                     |                          |
     |<- TLS handshake --->|                          |
     |   (fake cert)       |                          |
     |                     |                          |
     |== TLS session 1 ====|==== TLS session 2 ======|
     |                     |                          |
     | "GET / HTTP/1.1"    |                          |
     |--- encrypted ------>| decrypts, logs,          |
     |                     |--- re-encrypts --------->|
     |                     |                          |
     |                     |          response bytes  |
     |                     |<-- encrypted ------------|
     |<--- re-encrypts ---|  decrypts, logs           |
```

### Step by step

1. **HTTP CONNECT** -- the client sends an unencrypted `CONNECT example.com:443` request to the proxy. This is how browsers and tools like curl ask a proxy to set up a tunnel to an HTTPS destination. The proxy learns the target hostname from this plaintext request.

2. **Upstream TLS** -- Noxy opens a real TLS connection to `example.com`, verifying the server's authentic certificate against Mozilla's root CAs. This is the "right half" of the tunnel. From the real server's perspective, Noxy is just a normal client.

3. **Fake certificate generation** -- Noxy uses `rcgen` to generate a new TLS certificate with the Subject Alternative Name set to `example.com`, signed by the user-provided CA key. This certificate is created on the fly, per host.

4. **Client TLS** -- Noxy performs a TLS handshake with the client using the fake certificate. The client accepts it because it trusts the CA that signed it (via `--cacert` or the system trust store). This is the "left half" of the tunnel.

5. **Relay loop** -- with both TLS sessions established, Noxy reads decrypted plaintext from one side, logs it, and writes it to the other. It uses `tokio::select!` to handle both directions concurrently:
   - Client sends `GET / HTTP/1.1 ...` -> Noxy decrypts, logs, re-encrypts, forwards to server
   - Server sends `HTTP/1.1 200 OK ...` -> Noxy decrypts, logs, re-encrypts, forwards to client

### Why the CA trust matters

This is the critical piece. Without trusting the CA, the client rejects the fake certificate and the connection fails. This is exactly how HTTPS protects against real MITM attacks -- unless the attacker controls a trusted CA, the fake cert is rejected.

## License

MIT
