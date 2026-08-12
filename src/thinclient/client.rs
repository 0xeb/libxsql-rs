// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::{Error, Result};
use std::time::Duration;

use super::clipboard::format_url_host;

/// HTTP client connection settings.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Server host to connect to (default `127.0.0.1`).
    pub host: String,
    /// Server TCP port to connect to (default `5555`).
    pub port: u16,
    /// Read/write timeout applied to the connection (default 30s).
    pub timeout: Duration,
    /// Optional auth token sent as the `X-XSQL-Token` header.
    pub token: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 5555,
            timeout: Duration::from_secs(30),
            token: None,
        }
    }
}

/// Blocking HTTP client for an xsql thinclient server.
pub struct ThinClient {
    config: ClientConfig,
}

impl ThinClient {
    /// Create a client bound to the given connection settings.
    pub fn new(config: ClientConfig) -> Self {
        Self { config }
    }

    /// POST `sql` to `/query` and return the response body, or an error on a
    /// non-200 status.
    pub fn query(&self, sql: &str) -> Result<String> {
        let (status, body) = self.request("POST", "/query", Some(sql))?;
        if status == 200 {
            Ok(body)
        } else {
            Err(Error::Message(body))
        }
    }

    /// GET `/status` and return the response body, or an error on a non-200
    /// status.
    pub fn status(&self) -> Result<String> {
        let (status, body) = self.request("GET", "/status", None)?;
        if status == 200 {
            Ok(body)
        } else {
            Err(Error::Message(body))
        }
    }

    /// POST `/cancel` to cooperatively cancel queries already in flight.
    pub fn cancel(&self) -> Result<()> {
        let (status, body) = self.request("POST", "/cancel", Some(""))?;
        if status == 200 {
            Ok(())
        } else {
            Err(Error::Message(body))
        }
    }

    /// POST `/shutdown` to ask the server to stop, ignoring the response body.
    pub fn shutdown(&self) -> Result<()> {
        let _ = self.request("POST", "/shutdown", Some(""))?;
        Ok(())
    }

    /// Return `true` if the server answers `/status` successfully.
    pub fn ping(&self) -> bool {
        self.status().is_ok()
    }

    /// Issue one blocking request over `ureq`, returning `(status, body)`. A
    /// `Some(body)` is sent as a POST; `None` is a GET. 4xx/5xx still return the
    /// body (the server's JSON error envelope) rather than erroring.
    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
        let url = format!(
            "http://{}:{}{}",
            format_url_host(&self.config.host),
            self.config.port,
            path
        );
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(self.config.timeout))
            .http_status_as_error(false)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let token = self.config.token.as_deref();
        let mut response = if method == "POST" {
            let mut request = agent.post(&url);
            if let Some(token) = token {
                request = request.header("X-XSQL-Token", token);
            }
            request
                .send(body.unwrap_or(""))
                .map_err(|err| Error::Message(err.to_string()))?
        } else {
            let mut request = agent.get(&url);
            if let Some(token) = token {
                request = request.header("X-XSQL-Token", token);
            }
            request
                .call()
                .map_err(|err| Error::Message(err.to_string()))?
        };
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|err| Error::Message(err.to_string()))?;
        Ok((status, body))
    }
}
