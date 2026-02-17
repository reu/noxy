use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use deno_core::{
    Extension, JsRuntime, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader,
    ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType, OpDecl, OpState,
    PollEventLoopOptions, ResolutionKind, RuntimeOptions,
};
use deno_error::JsErrorBox;
use http::{Request, Response};
use tower::Service;

use crate::http::{Body, BoxError, HttpService, full_body};

const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

struct RequestMeta {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
}

impl RequestMeta {
    fn into_request(self, body: Body) -> Result<Request<Body>, BoxError> {
        let mut builder = Request::builder()
            .method(self.method.as_str())
            .uri(&self.url);
        for (k, v) in &self.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        Ok(builder.body(body)?)
    }
}

#[derive(deno_core::serde::Serialize)]
struct ResponseMeta {
    status: u16,
    headers: Vec<(String, String)>,
}

struct RequestEnvelope {
    meta: RequestMeta,
    body: Arc<Mutex<Option<Body>>>,
    respond_tx: tokio::sync::mpsc::Sender<RespondCommand>,
    result_tx: tokio::sync::oneshot::Sender<Result<Response<Body>, BoxError>>,
}

type RespondReply = Result<(ResponseMeta, Arc<Mutex<Option<Body>>>), BoxError>;
type BufferedHttpService =
    tower::buffer::Buffer<Request<Body>, <HttpService as Service<Request<Body>>>::Future>;

struct RespondCommand {
    meta: RequestMeta,
    body: Option<Bytes>,
    original_body: Arc<Mutex<Option<Body>>>,
    reply_tx: tokio::sync::oneshot::Sender<RespondReply>,
}

struct RequestState {
    body: Arc<Mutex<Option<Body>>>,
    respond_tx: tokio::sync::mpsc::Sender<RespondCommand>,
    response_body: Option<Arc<Mutex<Option<Body>>>>,
    result: Option<ScriptResult>,
    max_body_bytes: usize,
}

#[derive(deno_core::serde::Deserialize)]
struct ScriptResult {
    status: u16,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    passthrough: bool,
}

#[deno_core::op2(async(lazy), nofast)]
#[buffer]
async fn op_noxy_req_body(state: Rc<RefCell<OpState>>) -> Result<Vec<u8>, JsErrorBox> {
    let (body, max_body_bytes) = {
        let mut state = state.borrow_mut();
        let req_state = state.borrow_mut::<RequestState>();
        (
            req_state.body.lock().unwrap().take(),
            req_state.max_body_bytes,
        )
    };

    match body {
        Some(body) => collect_limited(body, max_body_bytes).await,
        None => Ok(Vec::new()),
    }
}

#[deno_core::op2(async(lazy))]
#[serde]
async fn op_noxy_respond(
    state: Rc<RefCell<OpState>>,
    #[string] method: String,
    #[string] url: String,
    #[serde] headers: Vec<(String, String)>,
    #[serde] body: Option<Vec<u8>>,
) -> Result<ResponseMeta, JsErrorBox> {
    let (respond_tx, original_body) = {
        let mut state = state.borrow_mut();
        let req_state = state.borrow_mut::<RequestState>();
        (req_state.respond_tx.clone(), req_state.body.clone())
    };

    let body_bytes = body.map(Bytes::from);

    let meta = RequestMeta {
        method,
        url,
        headers,
    };

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    respond_tx
        .send(RespondCommand {
            meta,
            body: body_bytes,
            original_body,
            reply_tx,
        })
        .await
        .map_err(|_| JsErrorBox::generic("Failed to send respond command"))?;

    let (response_meta, response_body) = reply_rx
        .await
        .map_err(|_| JsErrorBox::generic("Failed to receive response"))?
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;

    {
        let mut state = state.borrow_mut();
        let req_state = state.borrow_mut::<RequestState>();
        req_state.response_body = Some(response_body);
    }

    Ok(response_meta)
}

#[deno_core::op2(async(lazy), nofast)]
#[buffer]
async fn op_noxy_res_body(state: Rc<RefCell<OpState>>) -> Result<Vec<u8>, JsErrorBox> {
    let (body, max_body_bytes) = {
        let mut state = state.borrow_mut();
        let req_state = state.borrow_mut::<RequestState>();
        (
            req_state
                .response_body
                .as_ref()
                .and_then(|b| b.lock().unwrap().take()),
            req_state.max_body_bytes,
        )
    };

    match body {
        Some(body) => collect_limited(body, max_body_bytes).await,
        None => Ok(Vec::new()),
    }
}

#[deno_core::op2]
fn op_noxy_finish(
    state: &mut OpState,
    status: u16,
    #[serde] headers: Vec<(String, String)>,
    #[buffer] body: Option<&[u8]>,
    passthrough: bool,
) {
    let req_state = state.borrow_mut::<RequestState>();
    req_state.result = Some(ScriptResult {
        status,
        headers,
        body: body.map(|b| b.to_vec()),
        passthrough,
    });
}

async fn collect_limited(mut body: Body, max_bytes: usize) -> Result<Vec<u8>, JsErrorBox> {
    use http_body_util::BodyExt;

    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| JsErrorBox::generic(e.to_string()))?;
        if let Ok(data) = frame.into_data() {
            let new_len = out.len().saturating_add(data.len());
            if new_len > max_bytes {
                return Err(JsErrorBox::generic(format!(
                    "Body exceeds script max_body_bytes limit ({max_bytes} bytes)"
                )));
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(out)
}

const RUNTIME_JS: &str = include_str!("script_runtime.js");

struct ScriptModuleLoader {
    user_source: String,
}

impl ModuleLoader for ScriptModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, deno_core::error::ModuleLoaderError> {
        deno_core::resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let specifier_str = module_specifier.as_str();

        if specifier_str == "ext:noxy/user_script" {
            let source = self.user_source.clone();
            return ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(source.into()),
                module_specifier,
                None,
            )));
        }

        ModuleLoadResponse::Sync(Err(JsErrorBox::generic(format!(
            "Module not found: {specifier_str}"
        ))))
    }
}

fn transpile_source(source: &str, filename: &str) -> anyhow::Result<String> {
    let media_type = deno_ast::MediaType::from_path(std::path::Path::new(filename));

    if !media_type.is_emittable() {
        return Ok(source.to_string());
    }

    let specifier = ModuleSpecifier::parse(&format!("file:///{filename}"))
        .map_err(|e| anyhow::anyhow!("invalid specifier: {e}"))?;

    let parsed = deno_ast::parse_module(deno_ast::ParseParams {
        specifier,
        text: source.into(),
        media_type,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })?;

    let transpiled = parsed.transpile(
        &deno_ast::TranspileOptions::default(),
        &deno_ast::TranspileModuleOptions { module_kind: None },
        &deno_ast::EmitOptions::default(),
    )?;

    Ok(transpiled.into_source().text.to_string())
}

fn spawn_v8_thread(
    transpiled_source: String,
    max_body_bytes: usize,
    mut rx: tokio::sync::mpsc::Receiver<RequestEnvelope>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let ops: Vec<OpDecl> = vec![
                op_noxy_req_body(),
                op_noxy_respond(),
                op_noxy_res_body(),
                op_noxy_finish(),
            ];

            let ext = Extension {
                name: "noxy_script",
                ops: std::borrow::Cow::Owned(ops),
                ..Default::default()
            };

            let mut runtime = JsRuntime::new(RuntimeOptions {
                module_loader: Some(Rc::new(ScriptModuleLoader {
                    user_source: transpiled_source,
                })),
                extensions: vec![ext],
                ..Default::default()
            });

            runtime
                .execute_script("<noxy:runtime>", RUNTIME_JS)
                .expect("Failed to load noxy runtime shim");

            let wrapper = ModuleSpecifier::parse("ext:noxy/main").unwrap();
            let mod_id = runtime
                .load_main_es_module_from_code(
                    &wrapper,
                    r#"import handler from "ext:noxy/user_script";
                        globalThis.__noxy_user_handler = handler;"#
                        .to_string(),
                )
                .await
                .expect("Failed to load user script module");

            let eval_result = runtime.mod_evaluate(mod_id);
            runtime
                .run_event_loop(Default::default())
                .await
                .expect("Failed to run event loop during module init");
            eval_result
                .await
                .expect("Failed to evaluate user script module");

            while let Some(envelope) = rx.recv().await {
                let result = handle_request(
                    &mut runtime,
                    envelope.meta,
                    envelope.body.clone(),
                    envelope.respond_tx,
                    max_body_bytes,
                )
                .await;

                match result {
                    Ok(response) => {
                        let _ = envelope.result_tx.send(Ok(response));
                    }
                    Err(e) => {
                        let _ = envelope.result_tx.send(Err(e));
                    }
                }
            }
        });
    })
}

async fn handle_request(
    runtime: &mut JsRuntime,
    meta: RequestMeta,
    body: Arc<Mutex<Option<Body>>>,
    respond_tx: tokio::sync::mpsc::Sender<RespondCommand>,
    max_body_bytes: usize,
) -> Result<Response<Body>, BoxError> {
    {
        let op_state = runtime.op_state();
        let mut op_state = op_state.borrow_mut();
        op_state.put(RequestState {
            body: body.clone(),
            respond_tx,
            response_body: None,
            result: None,
            max_body_bytes,
        });
    }

    let method_json = deno_core::serde_json::to_string(&meta.method)?;
    let url_json = deno_core::serde_json::to_string(&meta.url)?;
    let headers_json = deno_core::serde_json::to_string(&meta.headers)?;

    let script = format!(
        "globalThis.__noxy_handle(globalThis.__noxy_user_handler, {method_json}, {url_json}, {headers_json})"
    );

    let promise = runtime.execute_script("<noxy:handle>", script)?;
    let resolve = runtime.resolve(promise);
    runtime
        .with_event_loop_promise(resolve, PollEventLoopOptions::default())
        .await?;

    let (result, response_body) = {
        let op_state = runtime.op_state();
        let mut op_state = op_state.borrow_mut();
        let req_state = op_state.take::<RequestState>();
        (
            req_state
                .result
                .ok_or_else(|| -> BoxError { "Script did not call __noxy_finish".into() })?,
            req_state.response_body,
        )
    };

    let mut builder = Response::builder().status(result.status);
    for (k, v) in &result.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }

    let response_body = if result.passthrough {
        match response_body {
            Some(handle) => handle
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(crate::http::empty_body),
            None => crate::http::empty_body(),
        }
    } else {
        let body_bytes = result.body.unwrap_or_default();
        full_body(Bytes::from(body_bytes))
    };

    Ok(builder.body(response_body)?)
}

/// Tower layer that runs a TypeScript/JavaScript script on each request.
///
/// The script is transpiled at construction time. By default, each connection
/// gets its own V8 isolate on a dedicated thread (per-connection isolation).
/// Use [`shared`](Self::shared) to reuse a single isolate across all connections.
///
/// # Examples
///
/// ```rust,no_run
/// use noxy::{Proxy, middleware::ScriptLayer};
///
/// # fn main() -> anyhow::Result<()> {
/// let proxy = Proxy::builder()
///     .ca_pem_files("ca-cert.pem", "ca-key.pem")?
///     .http_layer(ScriptLayer::from_file("middleware.ts")?)
///     .build()?;
/// # Ok(())
/// # }
/// ```
pub struct ScriptLayer {
    source: Arc<String>,
    max_body_bytes: usize,
    shared_tx: Option<tokio::sync::mpsc::Sender<RequestEnvelope>>,
    _shared_thread: Option<Arc<std::thread::JoinHandle<()>>>,
}

impl ScriptLayer {
    /// Load a script from a file path. TypeScript files are transpiled automatically.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let source = std::fs::read_to_string(path)?;
        let filename = path.to_string_lossy();
        let transpiled = transpile_source(&source, &filename)?;
        Ok(Self::from_transpiled(transpiled))
    }

    /// Load a script from source code. The source must be valid JavaScript
    /// (use [`from_file`](Self::from_file) for automatic TypeScript transpilation).
    pub fn from_source(source: &str) -> anyhow::Result<Self> {
        Ok(Self::from_transpiled(source.to_string()))
    }

    fn from_transpiled(transpiled: String) -> Self {
        Self {
            source: Arc::new(transpiled),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            shared_tx: None,
            _shared_thread: None,
        }
    }

    /// Maximum number of bytes scripts may buffer when calling `req.body()`
    /// or `res.body()`. If exceeded, the JS promise rejects with an error.
    ///
    /// Default: 1 MiB.
    pub fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = bytes.max(1);
        if self.shared_tx.is_some() {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let thread = spawn_v8_thread((*self.source).clone(), self.max_body_bytes, rx);
            self.shared_tx = Some(tx);
            self._shared_thread = Some(Arc::new(thread));
        }
        self
    }

    /// Share a single V8 isolate across all connections instead of spawning
    /// one per connection. Global state in the script will be visible to all
    /// requests regardless of which connection they belong to.
    pub fn shared(mut self) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let thread = spawn_v8_thread((*self.source).clone(), self.max_body_bytes, rx);
        self.shared_tx = Some(tx);
        self._shared_thread = Some(Arc::new(thread));
        self
    }
}

impl tower::Layer<HttpService> for ScriptLayer {
    type Service = ScriptService;

    fn layer(&self, inner: HttpService) -> Self::Service {
        if let Some(tx) = &self.shared_tx {
            return ScriptService {
                inner: tower::buffer::Buffer::new(inner, 1024),
                tx: tx.clone(),
                _thread: None,
            };
        }

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let thread = spawn_v8_thread((*self.source).clone(), self.max_body_bytes, rx);
        ScriptService {
            inner: tower::buffer::Buffer::new(inner, 1024),
            tx,
            _thread: Some(Arc::new(thread)),
        }
    }
}

pub struct ScriptService {
    inner: BufferedHttpService,
    tx: tokio::sync::mpsc::Sender<RequestEnvelope>,
    _thread: Option<Arc<std::thread::JoinHandle<()>>>,
}

impl Service<Request<Body>> for ScriptService {
    type Response = Response<Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let tx = self.tx.clone();
        let mut inner = self.inner.clone();

        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let meta = RequestMeta {
                method: parts.method.to_string(),
                url: parts.uri.to_string(),
                headers: parts
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect(),
            };
            let body_handle = Arc::new(Mutex::new(Some(body)));

            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            let (respond_tx, mut respond_rx) = tokio::sync::mpsc::channel::<RespondCommand>(1);

            tx.send(RequestEnvelope {
                meta,
                body: body_handle,
                respond_tx,
                result_tx,
            })
            .await
            .map_err(|_| -> BoxError { "V8 thread shut down".into() })?;

            let mut result_rx = result_rx;
            loop {
                tokio::select! {
                    cmd = respond_rx.recv() => {
                        if let Some(cmd) = cmd {
                            let req_body = match cmd.body {
                                Some(bytes) => full_body(bytes),
                                None => {
                                    cmd.original_body.lock().unwrap().take()
                                        .unwrap_or_else(crate::http::empty_body)
                                }
                            };
                            let req = cmd.meta.into_request(req_body);

                            match req {
                                Ok(req) => {
                                    std::future::poll_fn(|cx| inner.poll_ready(cx)).await?;
                                    let result = inner.call(req).await;
                                    match result {
                                        Ok(resp) => {
                                            let (parts, body) = resp.into_parts();
                                            let resp_meta = ResponseMeta {
                                                status: parts.status.as_u16(),
                                                headers: parts.headers.iter()
                                                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                                                    .collect(),
                                            };
                                            let body_handle = Arc::new(Mutex::new(Some(body)));
                                            let _ = cmd.reply_tx.send(Ok((resp_meta, body_handle)));
                                        }
                                        Err(e) => {
                                            let _ = cmd.reply_tx.send(Err(e));
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = cmd.reply_tx.send(Err(e));
                                }
                            }
                        }
                    }
                    result = &mut result_rx => {
                        return result.map_err(|_| -> BoxError { "V8 thread dropped result".into() })?;
                    }
                }
            }
        })
    }
}
