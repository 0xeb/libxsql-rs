// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{Error, Result};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tiny_http::{Request as TinyRequest, Server as TinyServer};

use super::http_query_server::{respond, to_http_request};
use super::json::{json_error, make_success_json};
use super::{ExtraRoute, HttpRequest, bind_address_port, ensure_secure_bind, io_error};

/// Lower-level HTTP server settings for custom routes.
#[derive(Clone)]
pub struct ServerConfig {
    /// Port to bind (default `8081`); `0` picks a random free port.
    pub port: u16,
    /// Address to bind the listener to (default `127.0.0.1`).
    pub bind_address: String,
    /// Optional auth token required on every route except where exempted.
    pub auth_token: Option<String>,
    /// Allow binding a non-loopback address without an auth token.
    pub allow_insecure_no_auth: bool,
    /// Routes served by this server (plus the built-in `/shutdown`).
    pub routes: Vec<ExtraRoute>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 8081,
            bind_address: "127.0.0.1".to_string(),
            auth_token: None,
            allow_insecure_no_auth: false,
            routes: Vec::new(),
        }
    }
}

/// Lower-level HTTP server for custom thinclient route sets.
pub struct Server {
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    local_addr: Arc<Mutex<Option<SocketAddr>>>,
    http: Arc<Mutex<Option<Arc<TinyServer>>>>,
    join: Option<JoinHandle<()>>,
}

impl Server {
    /// Create a server from `config`; not yet bound or running.
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            shutdown: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
            local_addr: Arc::new(Mutex::new(None)),
            http: Arc::new(Mutex::new(None)),
            join: None,
        }
    }

    /// Bind and serve on the current thread, blocking until shutdown.
    pub fn run(&mut self) -> Result<()> {
        let http = self.bind_http()?;
        self.running.store(true, Ordering::SeqCst);
        serve_custom_loop(
            http,
            self.config.clone(),
            self.shutdown.clone(),
            self.running.clone(),
        );
        Ok(())
    }

    /// Bind and serve on a background thread, returning immediately.
    pub fn run_async(&mut self) -> Result<()> {
        let listener = self.prepare_listener()?;
        self.run_async_with_listener(listener)
    }

    /// Serve on a caller-provided listener (see
    /// [`crate::thinclient::HttpQueryServer::start_with_listener`]).
    pub fn run_async_with_listener(&mut self, listener: TcpListener) -> Result<()> {
        let http = self.adopt_listener(listener)?;
        self.running.store(true, Ordering::SeqCst);
        let config = self.config.clone();
        let shutdown = self.shutdown.clone();
        let running = self.running.clone();
        self.join = Some(thread::spawn(move || {
            serve_custom_loop(http, config, shutdown, running);
        }));
        Ok(())
    }

    /// Bind a fresh listener per `config` and wrap it in a tiny_http server.
    fn bind_http(&self) -> Result<Arc<TinyServer>> {
        let listener = self.prepare_listener()?;
        self.adopt_listener(listener)
    }

    /// Record the listener's address and wrap it in a shared tiny_http server.
    fn adopt_listener(&self, listener: TcpListener) -> Result<Arc<TinyServer>> {
        let local_addr = listener.local_addr().map_err(io_error)?;
        *self
            .local_addr
            .lock()
            .expect("server address lock poisoned") = Some(local_addr);
        let http = Arc::new(
            TinyServer::from_listener(listener, None)
                .map_err(|err| Error::Message(format!("tiny_http server: {err}")))?,
        );
        *self.http.lock().expect("server http lock poisoned") = Some(http.clone());
        Ok(http)
    }

    /// Signal shutdown, unblock the accept loop, join any background thread, and
    /// mark the server stopped.
    pub fn stop(&mut self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(http) = self
            .http
            .lock()
            .expect("server http lock poisoned")
            .as_ref()
        {
            http.unblock();
        }
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| Error::Message("thinclient server thread panicked".to_string()))?;
        }
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Return `true` while the server is accepting and not shutting down.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst) && !self.shutdown.load(Ordering::SeqCst)
    }

    /// Bound port if running, otherwise the configured port.
    pub fn port(&self) -> u16 {
        self.local_addr
            .lock()
            .expect("server address lock poisoned")
            .map(|addr| addr.port())
            .unwrap_or(self.config.port)
    }

    /// Spawn a thread that requests shutdown after a short delay (used to let a
    /// `/shutdown` response flush before the listener closes).
    pub fn schedule_shutdown(&self) {
        let shutdown = self.shutdown.clone();
        let http = self.http.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            shutdown.store(true, Ordering::SeqCst);
            if let Some(http) = http.lock().expect("server http lock poisoned").as_ref() {
                http.unblock();
            }
        });
    }

    fn prepare_listener(&self) -> Result<TcpListener> {
        ensure_secure_bind(
            &self.config.bind_address,
            self.config.auth_token.as_deref(),
            self.config.allow_insecure_no_auth,
        )?;
        bind_address_port(&self.config.bind_address, self.config.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn serve_custom_loop(
    http: Arc<TinyServer>,
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
) {
    for request in http.incoming_requests() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let config = config.clone();
        let shutdown = shutdown.clone();
        let http = http.clone();
        thread::spawn(move || {
            let _ = handle_custom_connection(request, &config, shutdown, &http);
        });
    }
    running.store(false, Ordering::SeqCst);
    shutdown.store(true, Ordering::SeqCst);
}

fn handle_custom_connection(
    mut request: TinyRequest,
    config: &ServerConfig,
    shutdown: Arc<AtomicBool>,
    http: &Arc<TinyServer>,
) -> Result<()> {
    let req = to_http_request(&mut request);
    if !server_auth_ok(config, &req) {
        return respond(
            request,
            401,
            "application/json",
            &json_error("Unauthorized"),
        );
    }
    if req.method == "POST" && req.path == "/shutdown" {
        shutdown.store(true, Ordering::SeqCst);
        http.unblock();
        return respond(
            request,
            200,
            "application/json",
            &make_success_json("Shutting down"),
        );
    }
    if let Some(route) = config
        .routes
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

fn server_auth_ok(config: &ServerConfig, request: &HttpRequest) -> bool {
    let Some(token) = &config.auth_token else {
        return true;
    };
    request
        .header("x-xsql-token")
        .map(|value| value == *token)
        .unwrap_or(false)
        || request
            .header("authorization")
            .map(|value| value == format!("Bearer {token}"))
            .unwrap_or(false)
}
