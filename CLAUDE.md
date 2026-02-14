# Noxy — TLS MITM Proxy

Noxy is a TLS man-in-the-middle proxy written in Rust. It intercepts CONNECT requests, generates fake certificates signed by a local CA, and relays traffic between client and upstream while running it through a middleware pipeline.

## Architecture

### Entry point (`src/main.rs`)
- `main()` — binds listener, loads CA cert/key, configures middleware layers, spawns per-connection tasks
- `handle_connect()` — handles a single CONNECT tunnel: TLS termination on both sides, relay loop with middleware
- `generate_cert()` — generates per-host certificates signed by the CA

### Middleware system (`src/middleware.rs`)
- `Direction` — `Upstream` / `Downstream`
- `ConnectionInfo` — per-connection metadata (client addr, target host/port)
- `TcpMiddleware` trait — stateful per-connection middleware with `on_data()` and `flush()`
- `TcpMiddlewareLayer` trait — factory that creates `TcpMiddleware` instances per connection
- `flush_middlewares()` — drains all middleware buffers on connection close

### TCP middlewares (`src/middleware/tcp/`)
- `FindReplaceLayer` / `FindReplace` — byte-level find and replace with partial-match buffering
- `TrafficLoggerLayer` / `TrafficLogger` — logs bytes sent/received per direction

### Middleware levels (design intent)
- **TcpMiddleware** — operates on raw byte streams; suitable for traffic logging, bandwidth throttling, latency injection, byte-level find/replace, connection stats
- **HttpMiddleware** — not yet implemented; would operate on parsed HTTP requests/responses (headers, body, status codes); suitable for header rewriting, content injection, cookie extraction, sensitive data scanning

## Project Guidelines

## After every Rust file change
- Run `cargo fmt` to format the code
- Run `cargo clippy` and fix all warnings before considering the task done
