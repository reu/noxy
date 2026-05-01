# Noxy — HTTP Proxy

Noxy is an HTTP proxy written in Rust supporting both forward (TLS MITM) and reverse proxy modes. In forward mode, it intercepts CONNECT requests, generates fake certificates signed by a local CA, and relays HTTP traffic. In reverse proxy mode, it forwards all incoming HTTP traffic to a fixed upstream. Both modes relay traffic through a tower middleware pipeline.

## Architecture

### Entry point (`src/main.rs`)
- `main()` — CLI interface (behind `cli` feature), loads config or synthesizes one from CLI flags, builds all listeners, runs them concurrently via `JoinSet` with a single broadcast shutdown signal
- CA generation via `--generate` flag
- `--config <path>` loads a KDL config file; CLI middleware flags append to `config.body` (global rules applied to every listener)
- When the config has no listener blocks, a synthetic listener is built from CLI flags: `--upstream` → reverse listener, otherwise → forward listener using `--cert`/`--key`. `--tls-cert`/`--tls-key` add client-facing TLS to the synthetic reverse listener.

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
- `layer()` — accepts any `tower::Layer<HttpService>`, type-erased via `LayerFn` closures

### Scripting middleware (`src/middleware/script.rs`, behind `scripting` feature)
- `ScriptLayer` / `ScriptService` — tower Layer/Service that runs JS/TS scripts via embedded V8 (`deno_core`)
- Dedicated V8 thread (since `JsRuntime` is `!Send`) communicating via `mpsc`/`oneshot` channels
- TypeScript transpiled at construction time via `deno_ast`
- Three constructors: `ScriptLayer::from_file(path)` reads + transpiles + builds; `ScriptLayer::from_ts_source(source, name)` transpiles inline TS/JS; `ScriptLayer::from_source(source)` for already-JS source (skips transpile)
- JS runtime shim (`script_runtime.js`) provides `Headers`, `Request`, `Response` classes and the `__noxy_handle` orchestrator
- User script exports a default async function receiving `(req, respond)` — `respond` forwards upstream, returning without it short-circuits
- KDL exposes both forms: `script r#"..."#` (inline source, positional arg) and `script-file "path.ts"` (file path)

### Config (`src/config.rs`, behind `config` feature)
KDL config (parsed via `knus` 3.x, KDL 1.x syntax — `true`/`false`, **not** `#true`/`#false`). The top level has three zones:

1. **Process settings** — `accept-invalid-upstream-certs`, timeouts, pool sizing, optional `redis`. Apply across every listener.
2. **Global rules** (`body: Vec<RuleNode>`) — middleware leaves and match scopes declared at the top level. Cloned into every listener's compile pass; shadowed innermost-wins by listener-internal rules of the same exclusive kind.
3. **Listener blocks** — `forwards: Vec<ForwardListener>` and `reverses: Vec<ReverseListener>`. Each owns its own port, mode-specific config (forward: `ca`, optional `credential`s, optional client-facing `tls`; reverse: `upstream`, optional client-facing `tls`), and rule body.

`ProxyConfig::into_listeners()` returns `Vec<CompiledListener { addr, proxy }>` — one per listener block. The CLI runs them all concurrently.

**Compilation flow:**
1. `walk_body` walks the global rule tree once, producing `global_decls` at path `[]`.
2. `validate_min_scope_global` rejects rules whose `min_scope` is `Listener` (e.g. `upstream`) at the global level.
3. For each listener: `walk_listener_body` clones `global_decls`, walks the listener body with `path = [0]` so listener-internal decls strict-extend globals, then `resolve_exclusions` over the union.
4. Per-listener `emit_decl` is a thin dispatcher that calls `Rule::emit` on each leaf config; the listener's `ProxyBuilder` is wired with mode-specific config (`ca`/`credential`/`upstream`/`tls`) before emission, then `.build()`.

Same-kind comparison in `resolve_exclusions` uses `std::mem::discriminant(&decl.leaf)` — no parallel kind-tag enum. When a descendant decl has `scope_pred = None` (always-fires within its containing scope), the contributed exclusion is "always true," so the outer is fully shadowed within that scope.

**`Rule` trait** — every leaf config (`LogConfig`, `LatencyConfig`, `BlockConfig`, …) implements this:
```rust
trait Rule {
    fn is_exclusive(&self) -> bool { false }
    fn min_scope(&self) -> RuleScope { RuleScope::Global }
    fn emit(self, builder: ProxyBuilder, ctx: &EmitCtx<'_>) -> anyhow::Result<ProxyBuilder>;
}
```
`is_exclusive` controls shadowing (default: additive). `min_scope` controls where the rule is allowed (default: anywhere; `UpstreamConfig` overrides to `Listener` since routing needs a listener context). `emit` constructs the actual tower layer (or route, for `UpstreamConfig`) and applies it via `apply_layer` using `ctx.pred`.

**Exclusive vs additive (shadowing model):**
- Exclusive (innermost-wins, outer carved out): `log`, `latency`, `bandwidth`, `fault`, `retry`, `circuit-breaker`, `respond`. Inner declaration replaces outer for matching requests.
- Additive (all stack): `block`, all six `*-request-header` / `*-response-header` ops, `rewrite-path`, `rewrite-path-regex`, `rate-limit`, `sliding-window`, `upstream`, `script`, `script-file`. (`rate-limit` and `sliding-window` are technically additive because most-restrictive-wins is already correct semantics; `upstream` routes via the Router's first-match-wins.)

**Path matching:** `path "/v1"` is exact match. `path "/v1/"` (trailing slash) means "subtree-including-self" — matches `/v1`, `/v1/foo`, `/v1/foo/bar`. `path "/v1/**"` matches `/v1/foo` and below but not `/v1` itself. There is no `path-prefix` field anymore; trailing-slash is the convention.

**Config alias nodes:** `host "X" { ... }`, `path "Y" { ... }`, `method "GET" { ... }`, and variadic `methods "GET" "POST" { ... }` desugar to the corresponding `match` block. Per-op header types (`SetRequestHeader`, `AppendRequestHeader`, `RemoveRequestHeader`, `SetResponseHeader`, `AppendResponseHeader`, `RemoveResponseHeader`) and rewrite types (`RewritePath`, `RewritePathRegex`) are all dedicated structs — no shared `HeaderEntry` / `RewriteSpec`, since each variant needs its own `impl Rule`.

**Stateful middleware caveat:** a global `rate-limit`/`sliding-window`/`circuit-breaker` is cloned into each listener's compile pass, so each listener gets its own state instance — they do *not* share buckets across listeners. For genuinely shared state across listeners or processes, configure `redis` and rely on its scope keys.

**Redis scope shadowing:** `redis` is allowed at process top-level, inside listener blocks, and inside match blocks — same shadowing rule as middleware. The walker maintains a `redis_stack`; each match's `walk_match` pushes/pops if the match has a `redis` field. Each `Decl` captures the closest-enclosing connection at walk time (`decl.redis: Option<RedisConnection>`). `EmitCtx.redis` is sourced from the decl, not the listener. The compiler maintains a `RedisCache: HashMap<(url, prefix), RedisConnection>` so the same `(url, prefix)` declared at multiple scopes shares one connection. `RedisConnection::prefix()` exposes the prefix for diagnostics and tests.

## TODO

### Performance
- Improve route dispatch scaling by avoiding linear predicate scans for large routing tables (evaluate indexed/prefix/glob-aware routing structure)

### Benchmarking
- Expanded workload matrix:
  - concurrency levels: `1`, `32`, `256`
  - key distributions: hot-key and high-cardinality
- Capture req/s, ns/op, p50/p95; optionally allocation metrics in a separate profile
- Add CI guardrails for meaningful regressions (avoid failing on small noise)

### Security
- Redact sensitive headers by default in traffic logs (`Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`, etc.)
- Use constant-time credential comparison for proxy auth checks
- Harden CONNECT authority parsing (avoid naive string split on `:`; use authority-aware parsing including IPv6)

### Scripting
- Expose body as an async iterable for chunk-by-chunk streaming without full buffering
- External module support (`https://`, `jsr:`, `npm:`) in scripting — currently only the user script is served; users can work around this by bundling dependencies into a single file

### Middleware ideas (`src/middleware/`, tower layers)
- Script injection — inject JS/CSS into HTML responses
- Find & replace — regex replacement in response bodies
- Cache — cache responses, serve on subsequent matching requests
- Sensitive data scanner — flag responses containing API keys, tokens, SSNs, etc.
- Cookie tracker — log and analyze cookies across domains
- HAR recorder — capture full request/response pairs as HAR files

### Scoped-resource abstraction (`src/config.rs`)
The redis scope-shadowing infrastructure (`redis_stack` + `redis_cache` on `WalkCtx`, per-decl `redis: Option<RedisConnection>`, `walk_match` push/pop, `EmitCtx.redis`) is a specific instance of a general pattern: **scope-shadowed external resource captured per-leaf with dedup-by-key**. Other resources that fit it (database/connection pools, cache backends, telemetry sinks, object storage, auth providers, secret stores) would each need a copy of the same plumbing today.

When the second such resource arrives, factor out a `ScopedContext<T, K>` owning the stack + cache + push/pop + `open_or_cached` so the mechanical part is shared. The schema-side glue (`#[knus(child)] xxx: Option<XxxConfig>` on each match struct + both listener structs — 7 sites per resource) is harder to deduplicate without a proc macro and probably not worth the complexity until we have ≥3 resources. Apply the rule of three: build the abstraction in the PR that introduces the second resource, once we know what the trait actually wants to look like, rather than designing it around redis alone.

## Project Guidelines

### No `mod.rs` files
Use `src/foo.rs` + `src/foo/bar.rs` layout instead of `src/foo/mod.rs`. This applies everywhere, including `tests/` — use `tests/common.rs` not `tests/common/mod.rs`.

### Middleware checklist
Every middleware should support all five surfaces. Adding one touches:

1. **Direct API** — `RateLimiter::global(30, Duration::from_secs(1))` via `layer()` (in `src/middleware/<name>.rs`)
2. **ProxyBuilder helper** — convenience method like `.rate_limit(30, Duration::from_secs(1))` (in `src/lib.rs`)
3. **KDL config** (`src/config.rs`):
   - Define a `<Name>Config` struct with `#[derive(knus::Decode)]`
   - Add a `RuleNode::<Name>(<Name>Config)` variant
   - Add the dispatch arm in `emit_decl` (one line: `RuleNode::<Name>(c) => c.emit(builder, &ctx)`)
   - Add the dispatch arm in `rule_is_exclusive` (one line: `RuleNode::<Name>(c) => c.is_exclusive()`)
   - Write `impl Rule for <Name>Config { ... }` — all per-middleware logic (exclusivity, layer construction) lives here
4. **CLI flag** — e.g. `--rate-limit 30/1s` (`src/main.rs`)
5. **README** — update the features list, CLI options, example config, and the relevant rule-nodes table

When deciding shadowing for a new middleware: override `is_exclusive(&self) -> bool { true }` if a deeper redeclaration should *replace* the outer one (typical for layers whose effects compound: latency, fault rates, body logging). Leave the default `false` if effects should naturally stack.

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

### Bug fixes
- Every bug fix must include a regression test that fails without the fix and passes with it

### After every Rust file change
- Run `cargo fmt` to format the code
- Run `cargo clippy` and fix all warnings before considering the task done
