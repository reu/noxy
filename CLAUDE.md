# Noxy — TLS MITM Proxy

Noxy is a TLS man-in-the-middle proxy written in Rust. It intercepts CONNECT requests, generates fake certificates signed by a local CA, and relays HTTP traffic between client and upstream through a tower middleware pipeline.

## Architecture

### Entry point (`src/main.rs`)
- `main()` — CLI interface (behind `cli` feature), loads CA cert/key, builds proxy, runs accept loop
- CA generation via `--generate` flag

### HTTP types and forwarding (`src/http.rs`)
- `Body` — `BoxBody<Bytes, BoxError>`, supports both buffered and streaming access
- `HttpService` — `BoxService<Request<Body>, Response<Body>, BoxError>`
- `ForwardService` — innermost tower service, wraps a hyper `SendRequest` handle to forward requests upstream. Supports both HTTP/1.1 and HTTP/2 via `UpstreamSender` enum.
- `incoming_to_body()` — converts hyper's `Incoming` to our `BoxBody`
- `full_body()` / `empty_body()` — body construction helpers

### Proxy core (`src/lib.rs`)
- `CertificateAuthority` — wraps CA cert/key, generates per-host leaf certificates
- `ProxyBuilder` — configures CA, tower layers, upstream cert verification
- `Proxy` — cloneable via internal `Arc`s, runs accept loop
- `handle_connection()` — handles a single CONNECT tunnel:
  1. Parses CONNECT request, establishes TLS on both sides (with ALPN for h2/http1.1)
  2. Hyper client handshake on upstream (HTTP/1.1 or HTTP/2 based on ALPN)
  3. Builds tower service chain: `ForwardService` → user layers
  4. Hyper server auto-builder serves the client connection through the service chain
- `HyperServiceAdapter` — bridges tower's `&mut self` call model to hyper's `&self` via `Arc<Mutex>`
- `http_layer()` — accepts any `tower::Layer<HttpService>`, type-erased via `LayerFn` closures

## Project Guidelines

## After every Rust file change
- Run `cargo fmt` to format the code
- Run `cargo clippy` and fix all warnings before considering the task done
