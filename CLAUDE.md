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

## TODO

### Scripting middleware (`ScriptLayer`)
Embed a TypeScript runtime via `deno_core` (behind a feature gate) so users can write middleware in `.ts` files.

**API:**
```typescript
export default async function(req, respond) {
  // Mutate headers directly
  req.headers.set("X-Foo", "bar");

  // req.body() returns a Promise — buffers on demand, zero-copy if never called
  const body = await req.body();

  // respond(req) forwards upstream with original body streaming through
  // respond(req, newBody) forwards with a replacement body
  const res = await respond(req, transform(body));

  // Mutate response headers
  res.headers.set("X-Proxied", "true");

  // res.body() same lazy pattern as req.body()
  return res;

  // Or short-circuit without forwarding:
  // return new Response("blocked", { status: 403 });
}
```

**Decided:**
- Single exported function receives `req` and `respond`
- `respond` is an injected function (not a method on req) — calls the inner tower service
- `respond(req)` forwards with original body untouched; `respond(req, newBody)` overrides body
- `req.body()` / `res.body()` returns a `Promise` — buffers on demand; not calling it means zero-copy passthrough
- Body handle stored as `Arc<Mutex<Option<Body>>>` on Rust side; the `body()` op drains it
- Headers mutated directly on the req/res objects
- Returning a `Response` without calling `respond` short-circuits (mock/block)
- One V8 isolate reused across requests
- Feature-gated to avoid V8 build times for users who don't need it

**Future:**
- Expose body as an async iterable for chunk-by-chunk streaming without full buffering
- Config file integration (`script = "middleware.ts"` in rules)
- External module support (`https://`, `jsr:`, `npm:`) — the current `ScriptModuleLoader` only serves the user script; supporting remote/registry modules would require HTTP fetching, transitive dependency resolution, and potentially `deno_node` for npm compatibility. Users can work around this today by bundling dependencies with `deno bundle` or `esbuild` into a single file.

## Project Guidelines

### Comments
- Avoid unnecessary comments, especially section dividers (e.g., `// -- Section name --`)
- Only add comments that genuinely help understand the code, such as explanations of non-obvious logic, examples, or important caveats

### After every Rust file change
- Run `cargo fmt` to format the code
- Run `cargo clippy` and fix all warnings before considering the task done
