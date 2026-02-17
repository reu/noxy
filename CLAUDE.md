# Noxy — HTTP Proxy

Noxy is an HTTP proxy written in Rust supporting both forward (TLS MITM) and reverse proxy modes. In forward mode, it intercepts CONNECT requests, generates fake certificates signed by a local CA, and relays HTTP traffic. In reverse proxy mode, it forwards all incoming HTTP traffic to a fixed upstream. Both modes relay traffic through a tower middleware pipeline.

## Architecture

### Entry point (`src/main.rs`)
- `main()` — CLI interface (behind `cli` feature), loads CA cert/key or upstream URL, builds proxy, runs accept loop
- CA generation via `--generate` flag
- `--upstream <URL>` enables reverse proxy mode; `--tls-cert`/`--tls-key` for client-facing TLS

### HTTP types and forwarding (`src/http.rs`)
- `Body` — `BoxBody<Bytes, BoxError>`, supports both buffered and streaming access
- `HttpService` — `BoxService<Request<Body>, Response<Body>, BoxError>`
- `UpstreamIo` — enum wrapping `TlsStream` (HTTPS) and plain `TcpStream` (HTTP) upstream connections
- `ForwardService` — innermost tower service, wraps hyper-util's pooled `Client` to forward requests upstream. Takes authority + scheme to support both HTTP and HTTPS upstreams.
- `incoming_to_body()` — converts hyper's `Incoming` to our `BoxBody`
- `full_body()` / `empty_body()` — body construction helpers

### Proxy core (`src/lib.rs`)
- `CertificateAuthority` — wraps CA cert/key, generates per-host leaf certificates
- `ProxyMode` — enum: `Forward { ca, server_config_cache, credentials }` or `Reverse { upstream_authority, upstream_scheme, tls_acceptor }`
- `ProxyBuilder` — configures CA (forward) or upstream URL (reverse), tower layers, upstream cert verification
  - `.reverse_proxy(url)` — enables reverse proxy mode
  - `.tls_identity(cert, key)` — client-facing TLS for reverse proxy
- `Proxy` — cloneable via internal `Arc`s, runs accept loop, dispatches to mode-specific handlers
- `handle_forward_connection()` — CONNECT tunnel: parses CONNECT, generates per-host cert, establishes TLS both sides, builds tower chain, serves via hyper
- `handle_reverse_connection()` — reverse proxy: builds tower chain with fixed upstream, optionally TLS-accepts client, serves via hyper
- `serve_client()` — shared hyper auto-builder + idle timeout + shutdown select, used by both modes
- `HyperServiceAdapter` — bridges tower's `&mut self` call model to hyper's `&self` via `Arc<Mutex>`
- `http_layer()` — accepts any `tower::Layer<HttpService>`, type-erased via `LayerFn` closures

### Scripting middleware (`src/middleware/script.rs`, behind `scripting` feature)
- `ScriptLayer` / `ScriptService` — tower Layer/Service that runs JS/TS scripts via embedded V8 (`deno_core`)
- Dedicated V8 thread (since `JsRuntime` is `!Send`) communicating via `mpsc`/`oneshot` channels
- TypeScript transpiled at construction time via `deno_ast`
- JS runtime shim (`script_runtime.js`) provides `Headers`, `Request`, `Response` classes and the `__noxy_handle` orchestrator
- User script exports a default async function receiving `(req, respond)` — `respond` forwards upstream, returning without it short-circuits

## TODO

### Performance
- Fix config behavior for `log = false` so traffic logging middleware is not installed (currently still pays formatting/locking overhead)
- Improve route dispatch scaling by avoiding linear predicate scans for large routing tables (evaluate indexed/prefix/glob-aware routing structure)

### Security
- Redact sensitive headers by default in traffic logs (`Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`, etc.)
- Use constant-time credential comparison for proxy auth checks
- Harden CONNECT authority parsing (avoid naive string split on `:`; use authority-aware parsing including IPv6)

### Scripting
- Expose body as an async iterable for chunk-by-chunk streaming without full buffering
- Config file integration for scripting (`script = "middleware.ts"` in rules)
- External module support (`https://`, `jsr:`, `npm:`) in scripting — currently only the user script is served; users can work around this by bundling dependencies into a single file

### Middleware ideas (`src/middleware/`, tower layers)
- Script injection — inject JS/CSS into HTML responses
- Find & replace — regex replacement in response bodies
- Cache — cache responses, serve on subsequent matching requests
- Sensitive data scanner — flag responses containing API keys, tokens, SSNs, etc.
- Cookie tracker — log and analyze cookies across domains
- HAR recorder — capture full request/response pairs as HAR files

## Project Guidelines

### No `mod.rs` files
Use `src/foo.rs` + `src/foo/bar.rs` layout instead of `src/foo/mod.rs`. This applies everywhere, including `tests/` — use `tests/common.rs` not `tests/common/mod.rs`.

### Middleware checklist
Every middleware should support all five surfaces:
1. **Direct API** — `RateLimiter::global(30, Duration::from_secs(1))` via `http_layer()`
2. **ProxyBuilder helper** — convenience method like `.rate_limit(30, Duration::from_secs(1))`
3. **TOML config** — `rate_limit = { count = 30, window = "1s" }` in `[[rules]]` (`src/config.rs`)
4. **CLI flag** — e.g. `--rate-limit 30/1s` (`src/main.rs`)
5. **README** — update the features list, CLI options, example config, and rules table

### Middleware design
- Prefer general, composable APIs over narrow ones. Middleware that operates on a per-request basis should accept a key function (`Fn(&Request<Body>) -> String`) so users can partition state by any criteria (global, per-host, per-header, per-API-key, etc.)
- Provide convenience constructors for common cases: `::global(...)`, `::per_host(...)`, and `::keyed(...)` for custom key functions
- Use builder methods for optional behavior (e.g., `.failure_policy(...)`, `.burst(...)`) rather than constructor parameters
- Follow the shared-state pattern (`Arc<Mutex<HashMap<String, State>>>`) used by `RateLimiter`, `SlidingWindow`, and `CircuitBreaker` for keyed middleware

### Comments
- Avoid unnecessary comments, especially section dividers (e.g., `// -- Section name --`)
- Only add comments that genuinely help understand the code, such as explanations of non-obvious logic, examples, or important caveats

### Doc-tests
- The README is included as crate-level docs (`#![doc = include_str!("../README.md")]`), so all `rust` code blocks are compiled as doc-tests
- In source-level doc comments (`///`), never use `` ```rust,ignore `` — use `` ```rust,no_run `` with hidden boilerplate (`# fn main() -> anyhow::Result<()> {`) so examples are compile-checked and catch API drift
- In `README.md`, use `` ```rust,ignore `` — hidden `# ` lines clutter the GitHub rendering, and the README is primarily read on GitHub

### After every Rust file change
- Run `cargo fmt` to format the code
- Run `cargo clippy` and fix all warnings before considering the task done
