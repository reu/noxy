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

### Scripting middleware (`src/middleware/script.rs`, behind `scripting` feature)
- `ScriptLayer` / `ScriptService` — tower Layer/Service that runs JS/TS scripts via embedded V8 (`deno_core`)
- Dedicated V8 thread (since `JsRuntime` is `!Send`) communicating via `mpsc`/`oneshot` channels
- TypeScript transpiled at construction time via `deno_ast`
- JS runtime shim (`script_runtime.js`) provides `Headers`, `Request`, `Response` classes and the `__noxy_handle` orchestrator
- User script exports a default async function receiving `(req, respond)` — `respond` forwards upstream, returning without it short-circuits

## TODO

### Scripting
- Expose body as an async iterable for chunk-by-chunk streaming without full buffering
- Config file integration for scripting (`script = "middleware.ts"` in rules)
- External module support (`https://`, `jsr:`, `npm:`) in scripting — currently only the user script is served; users can work around this by bundling dependencies into a single file

### Middleware ideas (`src/middleware/`, tower layers)
- Script injection — inject JS/CSS into HTML responses
- URL rewriting — rewrite URLs in requests or response bodies
- Find & replace — regex replacement in response bodies
- Block list — reject requests to certain domains/URL patterns
- Cache — cache responses, serve on subsequent matching requests
- Sensitive data scanner — flag responses containing API keys, tokens, SSNs, etc.
- Cookie tracker — log and analyze cookies across domains
- HAR recorder — capture full request/response pairs as HAR files

## Project Guidelines

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

### After every Rust file change
- Run `cargo fmt` to format the code
- Run `cargo clippy` and fix all warnings before considering the task done
