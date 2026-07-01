// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{
    Error, Result, ScriptOptions, ScriptResult, json_to_script_result, run_script_with_executor,
    script_result_to_csv, script_result_to_json_with_sql, script_result_to_text,
    script_result_to_tsv,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::net::{SocketAddr, TcpListener};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tiny_http::{
    Header as TinyHeader, Request as TinyRequest, Response as TinyResponse, Server as TinyServer,
};

use super::clipboard::format_url_host;
use super::json::{json_error, json_error_with_hint};
use super::{
    DurationFn, HttpRequest, InterruptFn, QueryFn, RouteFn, ScriptExecutorFn, SizeFn,
    StatementExecutorFn, StatusFn, bind_address_port, ensure_secure_bind, io_error, split_target,
};

/// HTTP query server settings.
#[derive(Clone)]
pub struct HttpQueryServerConfig {
    /// Tool name reported in `/status` and the `GET /` landing page.
    pub tool_name: String,
    /// Body returned by `GET /help`.
    pub help_text: String,
    /// Address to bind the listener to (default `127.0.0.1`).
    pub bind_address: String,
    /// Port to bind; `0` picks a random free port in `[8100, 8199]`.
    pub port: u16,
    /// Optional auth token required on `/query`, `/status`, and `/shutdown`.
    pub auth_token: Option<String>,
    /// Allow binding a non-loopback address without an auth token. Default
    /// `false`: [`HttpQueryServer::start`] refuses to bind a public address
    /// unless an auth token is set or this is explicitly enabled. Mirrors the
    /// `ServerConfig` policy so both thinclient servers behave consistently.
    pub allow_insecure_no_auth: bool,
    /// Extra string fields merged into the `/status` JSON object.
    pub status_fields: BTreeMap<String, String>,
    /// Optional callback whose JSON is merge-patched onto the `/status` body.
    pub status_fn: Option<Arc<StatusFn>>,
    /// Additional non-standard routes appended after the built-in routes.
    pub extra_routes: Vec<ExtraRoute>,
    /// When `true`, queries are enqueued for an owner thread instead of run on
    /// the connection thread.
    pub queue_mode: bool,
    /// How long a queued request waits for admission before timing out.
    pub queue_admission_timeout: Duration,
    /// Maximum pending queued commands; `0` means unbounded.
    pub max_queue: usize,
    /// Optional dynamic override for [`Self::queue_admission_timeout`].
    pub queue_admission_timeout_fn: Option<Arc<DurationFn>>,
    /// Optional dynamic override for [`Self::max_queue`].
    pub max_queue_fn: Option<Arc<SizeFn>>,
    /// Optional callback polled to request a cooperative server shutdown.
    pub interrupt_check: Option<Arc<InterruptFn>>,
    /// Preferred executor: runs a single statement and fills its result. When
    /// set, the server owns multi-statement orchestration
    /// ([`run_script_with_executor`]), option parsing
    /// (`continue_on_error`/`include_sql` from the query string or a JSON body),
    /// and output formatting from the [`ScriptResult`]
    /// — no JSON round-trip. Takes precedence over the `query_fn` passed to
    /// [`HttpQueryServer::new`]. Honors [`Self::queue_mode`].
    pub statement_executor: Option<Arc<StatementExecutorFn>>,
    /// Whole-script executor: the server parses options/format and hands the
    /// entire script to this callback (for batch/atomic semantics where a later
    /// statement must not read stale data after an earlier mutation). The
    /// returned [`ScriptResult`] is formatted by the server.
    /// Takes precedence over [`Self::statement_executor`] and the `query_fn`.
    pub script_executor: Option<Arc<ScriptExecutorFn>>,
    /// When `true` (and [`Self::queue_mode`] is `false`), serialize `/query`
    /// execution under an internal lock so requests run one at a time, honoring
    /// [`Self::max_queue`] (503) and the admission timeout (408). For executors
    /// that are not concurrency-safe yet run on the HTTP worker rather than a
    /// main-thread queue. Ignored when `queue_mode` is `true`.
    pub serialize_requests: bool,
}

impl Default for HttpQueryServerConfig {
    fn default() -> Self {
        Self {
            tool_name: "xsql".to_string(),
            help_text: "xsql HTTP query server".to_string(),
            bind_address: "127.0.0.1".to_string(),
            // 0 matches the C++ http_query_server_config default: pick a random
            // port from [8100, 8199].
            port: 0,
            auth_token: None,
            allow_insecure_no_auth: false,
            status_fields: BTreeMap::new(),
            status_fn: None,
            extra_routes: Vec::new(),
            queue_mode: false,
            queue_admission_timeout: Duration::from_secs(60),
            max_queue: 0,
            queue_admission_timeout_fn: None,
            max_queue_fn: None,
            interrupt_check: None,
            statement_executor: None,
            script_executor: None,
            serialize_requests: false,
        }
    }
}

/// Extra HTTP route registered beside the standard query routes.
#[derive(Clone)]
pub struct ExtraRoute {
    /// HTTP method the route matches (e.g. `GET`, `POST`).
    pub method: String,
    /// Request path the route matches (e.g. `/decompile`).
    pub path: String,
    /// `Content-Type` used for the route's successful response.
    pub content_type: String,
    pub(crate) handler: Arc<RouteFn>,
}

impl ExtraRoute {
    /// Build a route for `method`+`path` whose `handler` receives the request
    /// body and returns the response body served with `content_type`.
    pub fn new<F>(
        method: impl Into<String>,
        path: impl Into<String>,
        content_type: impl Into<String>,
        handler: F,
    ) -> Self
    where
        F: Fn(&str) -> Result<String> + Send + Sync + 'static,
    {
        Self {
            method: method.into(),
            path: path.into(),
            content_type: content_type.into(),
            handler: Arc::new(handler),
        }
    }
}

/// Blocking HTTP query server with optional owner-thread queue mode.
pub struct HttpQueryServer {
    config: HttpQueryServerConfig,
    query: Arc<QueryFn>,
    shutdown: Arc<AtomicBool>,
}

impl HttpQueryServer {
    /// Build a server from `config` whose `query` callback executes each SQL
    /// request and returns the JSON response body.
    pub fn new<F>(config: HttpQueryServerConfig, query: F) -> Self
    where
        F: Fn(&str) -> Result<String> + Send + Sync + 'static,
    {
        Self {
            config,
            query: Arc::new(query),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Bind a listener per `config` and start serving on a background thread,
    /// returning a handle to the running server.
    ///
    /// Refuses to bind a non-loopback `bind_address` unless `config.auth_token`
    /// is set or `config.allow_insecure_no_auth` is `true`. Use
    /// [`Self::start_with_listener`] to serve on a caller-provided listener,
    /// which bypasses this policy (a caller-supplied listener cannot be
    /// reliably policy-checked).
    pub fn start(self) -> Result<HttpQueryServerHandle> {
        let listener = bind_listener(&self.config)?;
        self.start_with_listener(listener)
    }

    /// Start serving on a caller-provided listener instead of binding from
    /// `config.port`. Useful for embedding in an existing listener, or to hold a
    /// unique, collision-free port for the server's lifetime.
    ///
    /// **Security:** unlike [`Self::start`], this does not apply the secure-bind
    /// policy — the listener is caller-provided, so enforcing loopback-only or an
    /// auth token on its bind address is the caller's responsibility.
    pub fn start_with_listener(self, listener: TcpListener) -> Result<HttpQueryServerHandle> {
        let local_addr = listener.local_addr().map_err(io_error)?;
        let http = Arc::new(
            TinyServer::from_listener(listener, None)
                .map_err(|err| Error::Message(format!("tiny_http server: {err}")))?,
        );
        let config = self.config.clone();
        let shutdown = self.shutdown.clone();
        let thread_shutdown = shutdown.clone();
        let queue = self
            .config
            .queue_mode
            .then(|| Arc::new(CommandQueue::default()));
        let executor = match queue.as_ref() {
            Some(queue) => QueryExecutor::Queue(queue.clone()),
            None => QueryExecutor::Direct {
                query: self.query.clone(),
                gate: self
                    .config
                    .serialize_requests
                    .then(|| Arc::new(SerializeGate::new())),
            },
        };
        let thread_http = http.clone();
        let join = thread::spawn(move || {
            serve_loop(thread_http, local_addr, config, executor, thread_shutdown);
        });
        let interrupt_check = Arc::new(Mutex::new(self.config.interrupt_check.clone()));
        Ok(HttpQueryServerHandle {
            local_addr,
            shutdown,
            http,
            query: self.query,
            script_executor: self.config.script_executor.clone(),
            statement_executor: self.config.statement_executor.clone(),
            queue,
            interrupt_check,
            join: Some(join),
        })
    }

    /// Start the server and block the current thread, draining queued commands
    /// and honoring the interrupt check until shutdown, then join.
    pub fn serve(self) -> Result<()> {
        let mut handle = self.start()?;
        while !handle.shutdown.load(Ordering::SeqCst) {
            if handle.interrupt_requested() {
                handle.shutdown();
                break;
            }
            handle.process_one_command_for(Duration::from_millis(100));
        }
        handle.join()
    }
}

/// Running HTTP query server handle.
pub struct HttpQueryServerHandle {
    local_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    http: Arc<TinyServer>,
    query: Arc<QueryFn>,
    script_executor: Option<Arc<ScriptExecutorFn>>,
    statement_executor: Option<Arc<StatementExecutorFn>>,
    queue: Option<Arc<CommandQueue>>,
    interrupt_check: Arc<Mutex<Option<Arc<InterruptFn>>>>,
    join: Option<JoinHandle<()>>,
}

impl HttpQueryServerHandle {
    /// Local socket address the server is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Port the server is bound to.
    pub fn port(&self) -> u16 {
        self.local_addr.port()
    }

    /// `http://host:port` base URL for the server, bracketing IPv6 hosts.
    pub fn url(&self) -> String {
        format!(
            "http://{}:{}",
            format_url_host(&self.local_addr.ip().to_string()),
            self.port()
        )
    }

    /// Return `true` until shutdown has been requested.
    pub fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::SeqCst)
    }

    /// Request shutdown: set the flag, unblock the accept loop, and fail any
    /// pending queued commands.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.http.unblock();
        if let Some(queue) = &self.queue {
            queue.drain_pending(Err(Error::Message("HTTP server stopped".to_string())));
        }
    }

    /// Run one queued command without blocking; returns `false` if none was
    /// ready (or the server is not in queue mode).
    pub fn process_one_command(&self) -> bool {
        self.process_one_command_for(Duration::ZERO)
    }

    /// Run one queued command, waiting up to `timeout` for one to arrive;
    /// returns `false` on timeout or when not in queue mode.
    pub fn process_one_command_for(&self, timeout: Duration) -> bool {
        let Some(queue) = &self.queue else {
            return false;
        };
        process_queued_command(
            queue,
            self.script_executor.as_ref(),
            self.statement_executor.as_ref(),
            &self.query,
            timeout,
            &self.shutdown,
        )
    }

    /// Block, draining queued commands and polling the interrupt check, until
    /// shutdown is requested.
    pub fn run_until_stopped(&self) {
        while !self.shutdown.load(Ordering::SeqCst) {
            if self.interrupt_requested() {
                self.shutdown();
                break;
            }
            self.process_one_command_for(Duration::from_millis(100));
        }
    }

    /// Install a callback polled by the blocking loops to request a cooperative
    /// shutdown when it returns `true`.
    pub fn set_interrupt_check<F>(&self, check: F)
    where
        F: Fn() -> bool + Send + Sync + 'static,
    {
        let mut slot = self
            .interrupt_check
            .lock()
            .expect("interrupt check lock poisoned");
        *slot = Some(Arc::new(check));
    }

    /// Remove any installed interrupt check.
    pub fn clear_interrupt_check(&self) {
        let mut slot = self
            .interrupt_check
            .lock()
            .expect("interrupt check lock poisoned");
        *slot = None;
    }

    /// Join the server thread, returning an error if it panicked. Idempotent.
    pub fn join(&mut self) -> Result<()> {
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| Error::Message("thinclient server thread panicked".to_string()))?;
        }
        Ok(())
    }

    fn interrupt_requested(&self) -> bool {
        self.interrupt_check
            .lock()
            .expect("interrupt check lock poisoned")
            .as_ref()
            .map(|check| check())
            .unwrap_or(false)
    }
}

#[derive(Clone)]
enum QueryExecutor {
    Direct {
        query: Arc<QueryFn>,
        gate: Option<Arc<SerializeGate>>,
    },
    Queue(Arc<CommandQueue>),
}

/// One-at-a-time admission gate for `serialize_requests`: a single-permit timed
/// semaphore with bounded waiting (`max_queue`) and an admission timeout.
struct SerializeGate {
    state: Mutex<GateState>,
    available: Condvar,
}

#[derive(Default)]
struct GateState {
    busy: bool,
    waiting: usize,
}

/// Result of a denied [`SerializeGate::acquire`].
enum Admit {
    QueueFull,
    Timeout,
}

struct SerializeGuard<'a>(&'a SerializeGate);

impl Drop for SerializeGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().expect("serialize gate poisoned");
        state.busy = false;
        self.0.available.notify_one();
    }
}

impl SerializeGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(GateState::default()),
            available: Condvar::new(),
        }
    }

    /// Block until the single permit is free, returning a guard that releases it
    /// on drop. `max_queue == 0` is unbounded; `timeout` zero waits indefinitely.
    fn acquire(
        &self,
        timeout: Duration,
        max_queue: usize,
    ) -> std::result::Result<SerializeGuard<'_>, Admit> {
        let mut state = self.state.lock().expect("serialize gate poisoned");
        if max_queue > 0 && state.waiting >= max_queue {
            return Err(Admit::QueueFull);
        }
        state.waiting += 1;
        let deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
        loop {
            if !state.busy {
                state.busy = true;
                state.waiting -= 1;
                return Ok(SerializeGuard(self));
            }
            match deadline {
                None => {
                    state = self.available.wait(state).expect("serialize gate poisoned");
                }
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        state.waiting -= 1;
                        return Err(Admit::Timeout);
                    }
                    let (guard, _) = self
                        .available
                        .wait_timeout(state, deadline - now)
                        .expect("serialize gate poisoned");
                    state = guard;
                }
            }
        }
    }
}

#[derive(Default)]
struct CommandQueue {
    pending: Mutex<VecDeque<Arc<QueuedCommand>>>,
    available: Condvar,
}

struct QueuedCommand {
    sql: String,
    opts: ScriptOptions,
    format: String,
    result: Mutex<Option<Result<String>>>,
    done: Condvar,
}

impl CommandQueue {
    /// Enqueue a request for the owner thread, which renders it (statement
    /// executor or query callback) per `opts`/`format`, and wait for the body.
    fn enqueue_and_wait(
        &self,
        sql: &str,
        opts: &ScriptOptions,
        format: &str,
        config: &HttpQueryServerConfig,
        shutdown: &AtomicBool,
    ) -> String {
        if shutdown.load(Ordering::SeqCst) {
            return json_error("Server not running");
        }
        let command = Arc::new(QueuedCommand {
            sql: sql.to_string(),
            opts: *opts,
            format: format.to_string(),
            result: Mutex::new(None),
            done: Condvar::new(),
        });
        let wait_timeout = config
            .queue_admission_timeout_fn
            .as_ref()
            .map(|timeout| timeout())
            .unwrap_or(config.queue_admission_timeout);
        let max_queue = config
            .max_queue_fn
            .as_ref()
            .map(|max_queue| max_queue())
            .unwrap_or(config.max_queue);
        {
            let mut pending = self.pending.lock().expect("queue lock poisoned");
            if max_queue > 0 && pending.len() >= max_queue {
                return json_error_with_hint(
                    "Queue full",
                    "Reduce concurrency or increase max_queue",
                );
            }
            pending.push_back(command.clone());
        }
        self.available.notify_one();

        let started = Instant::now();
        let mut result = command.result.lock().expect("command lock poisoned");
        loop {
            if let Some(result) = result.take() {
                return match result {
                    Ok(body) => body,
                    Err(err) => json_error(err.to_string()),
                };
            }
            if wait_timeout.is_zero() {
                result = command.done.wait(result).expect("command wait poisoned");
                continue;
            }
            let Some(remaining) = wait_timeout.checked_sub(started.elapsed()) else {
                return json_error_with_hint(
                    "Request timed out while waiting in queue",
                    "Reduce concurrency or increase queue_admission_timeout_ms",
                );
            };
            let (guard, wait) = command
                .done
                .wait_timeout(result, remaining)
                .expect("command wait poisoned");
            result = guard;
            if wait.timed_out() && result.is_none() {
                return json_error_with_hint(
                    "Request timed out while waiting in queue",
                    "Reduce concurrency or increase queue_admission_timeout_ms",
                );
            }
        }
    }

    fn drain_pending(&self, result: Result<String>) {
        let mut pending = self.pending.lock().expect("queue lock poisoned");
        let commands = pending.drain(..).collect::<Vec<_>>();
        drop(pending);
        let mut result = Some(result);
        for command in commands {
            let mut slot = command.result.lock().expect("command lock poisoned");
            if slot.is_none() {
                *slot = Some(
                    result
                        .take()
                        .unwrap_or_else(|| Err(Error::Message("HTTP server stopped".to_string()))),
                );
            }
            command.done.notify_one();
        }
    }
}

impl Drop for HttpQueryServerHandle {
    fn drop(&mut self) {
        self.shutdown();
        let _ = self.join();
    }
}

fn serve_loop(
    http: Arc<TinyServer>,
    local_addr: SocketAddr,
    config: HttpQueryServerConfig,
    executor: QueryExecutor,
    shutdown: Arc<AtomicBool>,
) {
    // tiny_http parks the accept thread until a request arrives; shutdown() calls
    // http.unblock(), which ends incoming_requests() and exits this loop.
    for request in http.incoming_requests() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let config = config.clone();
        let executor = executor.clone();
        let shutdown = shutdown.clone();
        let http = http.clone();
        thread::spawn(move || {
            let _ = handle_connection(request, local_addr, &config, executor, shutdown, &http);
        });
    }
    shutdown.store(true, Ordering::SeqCst);
}

fn handle_connection(
    mut request: TinyRequest,
    local_addr: SocketAddr,
    config: &HttpQueryServerConfig,
    executor: QueryExecutor,
    shutdown: Arc<AtomicBool>,
    http: &Arc<TinyServer>,
) -> Result<()> {
    let req = to_http_request(&mut request);
    if !auth_ok(config, &req) {
        return respond(
            request,
            401,
            "application/json",
            &json_error("Unauthorized"),
        );
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => respond(
            request,
            200,
            "text/plain",
            &root_welcome(&config.tool_name, local_addr.port()),
        ),
        ("GET", "/help") => respond(request, 200, "text/plain", &config.help_text),
        ("POST", "/query") => {
            let response = handle_query(config, &executor, &shutdown, &req);
            respond(
                request,
                response.status,
                &response.content_type,
                &response.body,
            )
        }
        ("GET", "/status") => match status_json(config) {
            Ok(body) => respond(request, 200, "application/json", &body),
            Err(err) => respond(
                request,
                500,
                "application/json",
                &json_error(err.to_string()),
            ),
        },
        ("POST", "/shutdown") => {
            shutdown.store(true, Ordering::SeqCst);
            http.unblock();
            if let QueryExecutor::Queue(queue) = &executor {
                queue.drain_pending(Err(Error::Message("HTTP server stopped".to_string())));
            }
            respond(
                request,
                200,
                "application/json",
                &json!({"success": true, "message": "Shutting down"}).to_string(),
            )
        }
        _ => {
            if let Some(route) = config
                .extra_routes
                .iter()
                .find(|route| route.method == req.method && route.path == req.path)
            {
                match (route.handler)(&req.body) {
                    Ok(body) => respond(request, 200, &route.content_type, &body),
                    Err(err) => respond(
                        request,
                        500,
                        "application/json",
                        &json_error(err.to_string()),
                    ),
                }
            } else {
                respond(request, 404, "text/plain", "not found")
            }
        }
    }
}

/// Build the internal [`HttpRequest`] from a `tiny_http` request: method, the
/// split path + query params, headers, and the body read to a string.
pub(crate) fn to_http_request(request: &mut TinyRequest) -> HttpRequest {
    let method = request.method().as_str().to_string();
    let (path, params) = split_target(request.url());
    let headers = request
        .headers()
        .iter()
        .map(|header| {
            (
                header.field.as_str().as_str().to_string(),
                header.value.as_str().to_string(),
            )
        })
        .collect();
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);
    HttpRequest {
        method,
        path,
        params,
        headers,
        body,
    }
}

/// Send a `tiny_http` response with the given status, `Content-Type`, and body.
pub(crate) fn respond(
    request: TinyRequest,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let header = TinyHeader::from_bytes(b"Content-Type".as_slice(), content_type.as_bytes())
        .map_err(|_| Error::Message("invalid content-type header".to_string()))?;
    let response = TinyResponse::from_string(body)
        .with_status_code(status)
        .with_header(header);
    request.respond(response).map_err(io_error)
}

struct QueryHttpResponse {
    status: u16,
    content_type: String,
    body: String,
}

/// Render a [`ScriptResult`] per the `format` query param (json/text/csv/tsv).
fn format_script_result(result: &ScriptResult, format: &str, include_sql: bool) -> Result<String> {
    Ok(match format {
        "text" => script_result_to_text(result),
        "csv" => script_result_to_csv(result),
        "tsv" => script_result_to_tsv(result),
        _ => script_result_to_json_with_sql(result, include_sql)?,
    })
}

/// `Content-Type` for a `format` query param; non-json formats are for terminal
/// or pipe use, while json stays the canonical response format.
fn content_type_for(format: &str) -> &'static str {
    match format {
        "text" => "text/plain",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        _ => "application/json",
    }
}

/// Execute one resolved request by executor precedence:
/// `script_executor` > `statement_executor` > `query_fn`. The first two own
/// orchestration and format directly from a [`ScriptResult`]; the callback path
/// returns a JSON string, re-parsed only for non-json output formats.
fn render_query(
    script_executor: Option<&Arc<ScriptExecutorFn>>,
    statement_executor: Option<&Arc<StatementExecutorFn>>,
    query: &Arc<QueryFn>,
    sql: &str,
    opts: &ScriptOptions,
    format: &str,
) -> Result<String> {
    if let Some(script_executor) = script_executor {
        let result = catch_unwind(AssertUnwindSafe(|| script_executor(sql, opts)))
            .map_err(|_| Error::Message("thinclient script executor panicked".to_string()))?;
        format_script_result(&result, format, opts.include_sql)
    } else if let Some(statement_executor) = statement_executor {
        let statement_executor = statement_executor.clone();
        let opts_owned = *opts;
        let result = catch_unwind(AssertUnwindSafe(|| {
            run_script_with_executor(sql, opts_owned, |stmt, out| statement_executor(stmt, out))
        }))
        .map_err(|_| Error::Message("thinclient statement executor panicked".to_string()))?;
        format_script_result(&result, format, opts.include_sql)
    } else {
        let json = run_query_callback(query, sql)?;
        if format == "json" {
            Ok(json)
        } else {
            format_script_result(&json_to_script_result(&json), format, false)
        }
    }
}

/// Read a boolean-ish flag from a JSON request body (bool, number, `"1"`/`"true"`).
fn read_json_flag(map: &Map<String, Value>, key: &str) -> bool {
    match map.get(key) {
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().map(|n| n != 0.0).unwrap_or(false),
        Some(Value::String(value)) => value == "1" || value == "true",
        _ => false,
    }
}

/// Resolve the SQL text from the request body, applying any JSON-body options to
/// `opts`. Two shapes: raw SQL (default), or `{"sql", "continue_on_error",
/// "include_sql"}` when the body is declared/looks like JSON. Returns the 400
/// error body when `application/json` is declared but the body is not a usable
/// object with a string `sql`.
fn resolve_query_sql(
    request: &HttpRequest,
    opts: &mut ScriptOptions,
) -> std::result::Result<String, String> {
    let ctype = request.header("content-type").unwrap_or_default();
    let json_declared = ctype.contains("application/json");
    let looks_json = json_declared || request.body.trim_start().starts_with('{');
    if looks_json {
        match serde_json::from_str::<Value>(&request.body) {
            Ok(Value::Object(map)) if map.get("sql").and_then(Value::as_str).is_some() => {
                let sql = map["sql"].as_str().unwrap_or_default().to_string();
                if read_json_flag(&map, "continue_on_error") {
                    opts.continue_on_error = true;
                }
                if read_json_flag(&map, "include_sql") {
                    opts.include_sql = true;
                }
                return Ok(sql);
            }
            _ if json_declared => {
                return Err(json_error(
                    "JSON request body must be an object with a string \"sql\"",
                ));
            }
            // Undeclared leading-'{' that isn't valid JSON-with-sql: raw SQL.
            _ => {}
        }
    }
    Ok(request.body.clone())
}

/// Resolve `/query` into (sql, opts, format) and dispatch by execution mode.
fn handle_query(
    config: &HttpQueryServerConfig,
    executor: &QueryExecutor,
    shutdown: &AtomicBool,
    request: &HttpRequest,
) -> QueryHttpResponse {
    let json_response = |status: u16, body: String| QueryHttpResponse {
        status,
        content_type: "application/json".to_string(),
        body,
    };

    let format = request
        .param("format")
        .filter(|format| !format.is_empty())
        .unwrap_or("json")
        .to_string();

    let mut opts = ScriptOptions::default();
    let sql_text = match resolve_query_sql(request, &mut opts) {
        Ok(sql) => sql,
        Err(body) => return json_response(400, body),
    };
    if sql_text.is_empty() {
        return json_response(400, json_error("Empty query"));
    }
    if request.param("continue_on_error") == Some("1") {
        opts.continue_on_error = true;
    }
    if request.param("include_sql") == Some("1") {
        opts.include_sql = true;
    }

    match executor {
        QueryExecutor::Queue(queue) => QueryHttpResponse {
            status: 200,
            content_type: content_type_for(&format).to_string(),
            body: queue.enqueue_and_wait(&sql_text, &opts, &format, config, shutdown),
        },
        QueryExecutor::Direct { query, gate } => {
            let _guard = match gate {
                Some(gate) => {
                    let timeout = config
                        .queue_admission_timeout_fn
                        .as_ref()
                        .map(|timeout| timeout())
                        .unwrap_or(config.queue_admission_timeout);
                    let max_queue = config
                        .max_queue_fn
                        .as_ref()
                        .map(|max_queue| max_queue())
                        .unwrap_or(config.max_queue);
                    match gate.acquire(timeout, max_queue) {
                        Ok(guard) => Some(guard),
                        Err(Admit::QueueFull) => {
                            return json_response(
                                503,
                                json_error_with_hint(
                                    "Queue full",
                                    "Reduce concurrency or increase max_queue",
                                ),
                            );
                        }
                        Err(Admit::Timeout) => {
                            return json_response(
                                408,
                                json_error_with_hint(
                                    "Request timed out while waiting for serialization",
                                    "Reduce concurrency or increase queue_admission_timeout",
                                ),
                            );
                        }
                    }
                }
                None => None,
            };
            match render_query(
                config.script_executor.as_ref(),
                config.statement_executor.as_ref(),
                query,
                &sql_text,
                &opts,
                &format,
            ) {
                Ok(body) => QueryHttpResponse {
                    status: 200,
                    content_type: content_type_for(&format).to_string(),
                    body,
                },
                Err(err) => json_response(500, json_error(err.to_string())),
            }
        }
    }
}

fn process_queued_command(
    queue: &CommandQueue,
    script_executor: Option<&Arc<ScriptExecutorFn>>,
    statement_executor: Option<&Arc<StatementExecutorFn>>,
    query: &Arc<QueryFn>,
    timeout: Duration,
    shutdown: &AtomicBool,
) -> bool {
    let command = {
        let mut pending = queue.pending.lock().expect("queue lock poisoned");
        if pending.is_empty() && !timeout.is_zero() {
            let (guard, _) = queue
                .available
                .wait_timeout_while(pending, timeout, |pending| {
                    pending.is_empty() && !shutdown.load(Ordering::SeqCst)
                })
                .expect("queue wait poisoned");
            pending = guard;
        }
        pending.pop_front()
    };

    let Some(command) = command else {
        return false;
    };
    let outcome = render_query(
        script_executor,
        statement_executor,
        query,
        &command.sql,
        &command.opts,
        &command.format,
    );
    let mut slot = command.result.lock().expect("command lock poisoned");
    *slot = Some(outcome);
    command.done.notify_one();
    true
}

fn run_query_callback(query: &Arc<QueryFn>, sql: &str) -> Result<String> {
    match catch_unwind(AssertUnwindSafe(|| query(sql))) {
        Ok(result) => result,
        Err(_) => Err(Error::Message(
            "thinclient query callback panicked".to_string(),
        )),
    }
}

fn auth_ok(config: &HttpQueryServerConfig, request: &HttpRequest) -> bool {
    let Some(token) = &config.auth_token else {
        return true;
    };
    if request.method == "GET" && (request.path == "/" || request.path == "/help") {
        return true;
    }
    request
        .header("x-xsql-token")
        .map(|value| value == *token)
        .unwrap_or(false)
        || request
            .header("authorization")
            .map(|value| value == format!("Bearer {token}"))
            .unwrap_or(false)
}

/// Build the `GET /` landing page, matching the C++ reference: the tool name is
/// uppercased and followed by the endpoint listing and a curl example.
fn root_welcome(tool: &str, port: u16) -> String {
    let display = tool.to_uppercase();
    format!(
        "{display} HTTP Server\n\nEndpoints:\n\
         \x20 GET  /help     - API documentation\n\
         \x20 POST /query    - Execute SQL query\n\
         \x20 GET  /status   - Health check\n\
         \x20 POST /shutdown - Stop server\n\n\
         Example: curl -X POST http://localhost:{port}/query -d \
         \"SELECT name FROM sqlite_master WHERE type='table' LIMIT 10\"\n"
    )
}

fn status_json(config: &HttpQueryServerConfig) -> Result<String> {
    let mut fields = Map::new();
    fields.insert("success".to_string(), Value::Bool(true));
    fields.insert("status".to_string(), Value::String("ok".to_string()));
    fields.insert("tool".to_string(), Value::String(config.tool_name.clone()));
    for (key, value) in &config.status_fields {
        fields.insert(key.clone(), Value::String(value.clone()));
    }
    let mut status = Value::Object(fields);
    if let Some(status_fn) = &config.status_fn {
        merge_patch(&mut status, status_fn()?);
    }
    Ok(status.to_string())
}

/// Apply an RFC 7386 (JSON Merge Patch) update of `patch` onto `target`, matching
/// the C++ reference's `nlohmann::json::merge_patch`. Object patches deep-merge;
/// a member set to JSON null removes that key; any non-object patch replaces the
/// whole target.
fn merge_patch(target: &mut Value, patch: Value) {
    match patch {
        Value::Object(patch_map) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let target_map = target.as_object_mut().expect("target is object");
            for (key, value) in patch_map {
                if value.is_null() {
                    target_map.remove(&key);
                } else {
                    merge_patch(target_map.entry(key).or_insert(Value::Null), value);
                }
            }
        }
        other => *target = other,
    }
}

fn bind_listener(config: &HttpQueryServerConfig) -> Result<TcpListener> {
    ensure_secure_bind(
        &config.bind_address,
        config.auth_token.as_deref(),
        config.allow_insecure_no_auth,
    )?;
    bind_address_port(&config.bind_address, config.port)
}
