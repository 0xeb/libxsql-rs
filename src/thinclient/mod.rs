// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::{Error, Result, ScriptOptions, ScriptResult, StatementResult};
use serde_json::Value;
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) type QueryFn = dyn Fn(&str) -> Result<String> + Send + Sync + 'static;
pub(crate) type RouteFn = dyn Fn(&str) -> Result<String> + Send + Sync + 'static;
pub(crate) type StatusFn = dyn Fn() -> Result<Value> + Send + Sync + 'static;
pub(crate) type DurationFn = dyn Fn() -> Duration + Send + Sync + 'static;
pub(crate) type SizeFn = dyn Fn() -> usize + Send + Sync + 'static;
pub(crate) type InterruptFn = dyn Fn() -> bool + Send + Sync + 'static;
/// Single-statement executor: runs one statement and fills the result.
pub(crate) type StatementExecutorFn = dyn Fn(&str, &mut StatementResult) + Send + Sync + 'static;
/// Whole-script executor: runs an entire script under caller-owned orchestration.
pub(crate) type ScriptExecutorFn =
    dyn Fn(&str, &ScriptOptions) -> ScriptResult + Send + Sync + 'static;

mod cli;
mod client;
mod clipboard;
mod custom_server;
mod http_query_server;
mod json;

pub use cli::{CliArgs, CliMode, parse_cli_args};
pub use client::{ClientConfig, ThinClient};
pub use clipboard::{
    build_http_clipboard_payload, build_mcp_clipboard_payload, clear_clipboard_copy_override,
    extract_http_start_endpoint, extract_mcp_start_endpoint, extract_url_host_from_output_line,
    format_http_info, format_http_info_default, format_http_status, format_http_status_default,
    format_url_host, normalize_clipboard_host, parse_started_port, set_clipboard_copy_override,
    try_copy_text_to_clipboard_windows,
};
pub use custom_server::{Server, ServerConfig};
pub use http_query_server::{
    ExtraRoute, HttpQueryServer, HttpQueryServerConfig, HttpQueryServerHandle,
};
pub use json::{
    JsonQueryResult, ThinclientJsonResult, json_escape, make_error_json, make_status_json,
    make_success_json, result_to_json,
};

#[derive(Debug)]
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) params: BTreeMap<String, String>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl HttpRequest {
    pub(crate) fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    /// Returns the value of the `name` query-string parameter, if present.
    pub(crate) fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }
}

/// Split a request target into its path and decoded query parameters.
pub(crate) fn split_target(target: &str) -> (String, BTreeMap<String, String>) {
    let mut params = BTreeMap::new();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (target, ""),
    };
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(key), percent_decode(value));
    }
    (path.to_string(), params)
}

/// Minimal percent-decoding for query-string keys/values (`%XX` and `+`).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn bind_address_port(bind_address: &str, port: u16) -> Result<TcpListener> {
    if port != 0 {
        return TcpListener::bind((bind_address, port)).map_err(io_error);
    }
    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u16 % 900)
        .unwrap_or(0);
    let mut last_error = None;
    for offset in 0..900 {
        let port = 8100 + ((start + offset) % 900);
        match TcpListener::bind((bind_address, port)) {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = Some(error),
        }
    }
    // Keep binding atomic even when the preferred range is exhausted: ask the
    // OS for an ephemeral port and return that still-open listener.
    TcpListener::bind((bind_address, 0)).map_err(|fallback| {
        let preferred = last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no port available in 8100-8999".to_string());
        Error::Message(format!(
            "{preferred}; ephemeral-port fallback failed: {fallback}"
        ))
    })
}

pub(crate) fn ensure_secure_bind(
    bind_address: &str,
    auth_token: Option<&str>,
    allow_insecure_no_auth: bool,
) -> Result<()> {
    if allow_insecure_no_auth || auth_token.map(|token| !token.is_empty()).unwrap_or(false) {
        return Ok(());
    }
    if is_loopback_bind_address(bind_address) {
        return Ok(());
    }
    Err(Error::Message(format!(
        "refusing to bind to {bind_address} without auth token"
    )))
}

fn is_loopback_bind_address(addr: &str) -> bool {
    addr == "localhost" || addr == "127.0.0.1" || addr == "::1" || addr.starts_with("127.")
}

pub(crate) fn io_error(error: std::io::Error) -> Error {
    Error::Message(error.to_string())
}

/// Constant-time bearer-token comparison shared by every thinclient server,
/// mirroring the C++ `xsql::thinclient::detail::timing_safe_equal`.
///
/// `str`/`String` equality short-circuits on the first differing byte, which
/// turns an authenticated endpoint into a remote timing oracle for the secret's
/// matching prefix. This accumulates differences over the presented token's full
/// length instead. The secret's LENGTH is not concealed (a length mismatch is
/// unequal), only prefix-match timing.
pub(crate) fn timing_safe_equal(presented: &str, expected: &str) -> bool {
    let presented = presented.as_bytes();
    let expected = expected.as_bytes();
    let mut acc: u8 = if presented.len() == expected.len() {
        0
    } else {
        1
    };
    for (i, &p) in presented.iter().enumerate() {
        let e = if expected.is_empty() {
            0
        } else {
            expected[i % expected.len()]
        };
        acc |= p ^ e;
    }
    acc == 0
}

#[cfg(test)]
mod auth_compare_tests {
    use super::timing_safe_equal;

    #[test]
    fn timing_safe_equal_matches_only_on_exact_secret() {
        assert!(timing_safe_equal("s3cret", "s3cret"));
        // Same length, wrong content.
        assert!(!timing_safe_equal("s3creX", "s3cret"));
        // Correct prefix, wrong suffix (the timing-oracle case).
        assert!(!timing_safe_equal("s3cre", "s3cret"));
        // Longer than the secret.
        assert!(!timing_safe_equal("s3cretary", "s3cret"));
        // Empty presented vs non-empty secret.
        assert!(!timing_safe_equal("", "s3cret"));
        // Empty vs empty is equal (mirrors operator== on two empty strings).
        assert!(timing_safe_equal("", ""));
    }
}
