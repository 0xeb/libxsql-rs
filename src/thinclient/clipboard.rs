// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use std::sync::{Arc, Mutex};

use super::json::json_escape;

type ClipboardCopyFn = dyn Fn(&str) -> bool + Send + Sync + 'static;

static CLIPBOARD_COPY_OVERRIDE: Mutex<Option<Arc<ClipboardCopyFn>>> = Mutex::new(None);

/// Return `host` unchanged, or `127.0.0.1` if it is empty.
pub fn normalize_clipboard_host(host: &str) -> String {
    if host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    }
}

/// Normalize `host` and wrap bare IPv6 literals in `[...]` for use in a URL.
pub fn format_url_host(host: &str) -> String {
    let normalized = normalize_clipboard_host(host);
    if normalized.contains(':') && !normalized.starts_with('[') {
        format!("[{normalized}]")
    } else {
        normalized
    }
}

/// The host a client should dial to reach a server bound to `bind_host`,
/// rendered for a URL. A wildcard bind (`0.0.0.0`, `::`) accepts connections
/// but is not a dialable address (curl on Windows refuses both), so it is
/// advertised as the matching loopback; every other host goes through
/// [`format_url_host`].
pub fn format_connect_url_host(bind_host: &str) -> String {
    match normalize_clipboard_host(bind_host).as_str() {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" | "[::]" => "[::1]".to_string(),
        host => format_url_host(host),
    }
}

/// Paste-ready instructions for reaching a running HTTP query server, written
/// for whoever receives them (usually an AI agent): the base URL, a pointer to
/// `GET /help` (endpoint and schema discovery), and two curl lines -- `/help`,
/// then a `POST /query` carrying `sample_sql` verbatim as the body
/// (`--data-binary`, so newlines and a leading `@` survive). `sample_sql` must
/// not contain double quotes. A non-empty `auth_token` adds the Authorization
/// header to the query line; `/help` is public and never needs it.
pub fn build_http_clipboard_payload(
    tool_name: &str,
    host: &str,
    port: u16,
    sample_sql: &str,
    auth_token: &str,
) -> String {
    let base_url = format!("http://{}:{port}", format_connect_url_host(host));
    let (auth_line, auth_header) = if auth_token.is_empty() {
        (String::new(), String::new())
    } else {
        (
            "Send the Authorization header shown with every request; \
             GET / and GET /help work without it.\n"
                .to_string(),
            format!(" -H \"Authorization: Bearer {auth_token}\""),
        )
    };
    format!(
        "{tool_name} HTTP server at {base_url}\n\
         Read GET /help first (endpoints, schema discovery), then send SQL as the \
         body of POST /query; responses are JSON.\n\
         {auth_line}\
         \x20 curl {base_url}/help\n\
         \x20 curl -X POST {base_url}/query{auth_header} --data-binary \"{sample_sql}\"\n"
    )
}

/// Build a clipboard-ready `mcpServers` JSON snippet pointing at the server's
/// `/sse` endpoint.
pub fn build_mcp_clipboard_payload(server_name: &str, host: &str, port: u16) -> String {
    format!(
        "{{\n  \"mcpServers\": {{\n    \"{}\": {{\n      \"url\": \"http://{}:{port}/sse\"\n    }}\n  }}\n}}\n",
        json_escape(server_name),
        format_connect_url_host(host)
    )
}

/// Parse the port that immediately follows `prefix` in `output`, returning
/// `None` if absent or zero.
pub fn parse_started_port(output: &str, prefix: &str) -> Option<u16> {
    let rest = output.strip_prefix(prefix)?;
    let digits = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u16>().ok().filter(|port| *port > 0)
}

/// Extract the host portion of a URL on the line beginning at `prefix`,
/// preserving bracketed IPv6 literals and stripping any `:port` suffix.
pub fn extract_url_host_from_output_line(output: &str, prefix: &str) -> Option<String> {
    let start = output.find(prefix)? + prefix.len();
    let line = output[start..]
        .split(['\r', '\n'])
        .next()
        .unwrap_or_default();
    let host_start = line.find("://").map(|pos| pos + 3).unwrap_or(0);
    let rest = line.get(host_start..)?;
    if rest.starts_with('[') {
        let end = rest.find(']')?;
        return Some(rest[..=end].to_string());
    }
    let port_sep = rest.rfind(':')?;
    Some(rest[..port_sep].to_string())
}

/// Parse the `(host, port)` of an HTTP server from its startup banner, defaulting
/// the host to `127.0.0.1`.
pub fn extract_http_start_endpoint(output: &str) -> Option<(String, u16)> {
    let port = parse_started_port(output, "HTTP server started on port ")?;
    let host = extract_url_host_from_output_line(output, "URL: ")
        .unwrap_or_else(|| "127.0.0.1".to_string());
    Some((host, port))
}

/// Parse the `(host, port)` of an MCP server from its startup banner, defaulting
/// the host to `127.0.0.1`.
pub fn extract_mcp_start_endpoint(output: &str) -> Option<(String, u16)> {
    let port = parse_started_port(output, "MCP server started on port ")?;
    let host = extract_url_host_from_output_line(output, "SSE endpoint: ")
        .unwrap_or_else(|| "127.0.0.1".to_string());
    Some((host, port))
}

/// Render the multi-line HTTP server startup banner (URL, endpoint listing, curl
/// example, and `stop_hint`).
///
/// The first line is `HTTP server started on port <port>`, which
/// [`extract_http_start_endpoint`] / [`parse_started_port`] parse, so it must
/// stay at the start of the banner. The tool name is shown on the `GET /`
/// landing page (see [`crate::thinclient::HttpQueryServerConfig::tool_name`]), not
/// in this banner — matching the C++ reference.
pub fn format_http_info(port: u16, bind_addr: &str, stop_hint: &str) -> String {
    let rendered_host = format_url_host(bind_addr);
    let connect_host = format_connect_url_host(bind_addr);
    format!(
        "HTTP server started on port {port}\n\
         URL: http://{rendered_host}:{port}\n\n\
         Endpoints:\n\
         \x20 GET  /help     - API documentation\n\
         \x20 POST /query    - Execute SQL query\n\
         \x20 GET  /status   - Health check\n\
         \x20 POST /shutdown - Stop server\n\n\
         Example:\n\
         \x20 curl -X POST http://{connect_host}:{port}/query --data-binary \"SELECT name FROM sqlite_master WHERE type='table' LIMIT 10\"\n\n\
         {stop_hint}\n"
    )
}

/// [`format_http_info`] with the bind address fixed to `127.0.0.1`.
pub fn format_http_info_default(port: u16, stop_hint: &str) -> String {
    format_http_info(port, "127.0.0.1", stop_hint)
}

/// Render a one/two-line status string reporting whether the HTTP server is
/// running and, if so, its URL.
pub fn format_http_status(port: u16, running: bool, bind_addr: &str) -> String {
    let rendered_host = format_url_host(bind_addr);
    if running {
        format!("HTTP server running on port {port}\nURL: http://{rendered_host}:{port}\n")
    } else {
        "HTTP server not running\nUse '.http start' to start\n".to_string()
    }
}

/// [`format_http_status`] with the bind address fixed to `127.0.0.1`.
pub fn format_http_status_default(port: u16, running: bool) -> String {
    format_http_status(port, running, "127.0.0.1")
}

/// Install a process-wide clipboard copy function used by
/// [`try_copy_text_to_clipboard_windows`] instead of the OS clipboard, e.g. to
/// capture or redirect copied text in a headless environment.
pub fn set_clipboard_copy_override<F>(copy: F)
where
    F: Fn(&str) -> bool + Send + Sync + 'static,
{
    let mut slot = CLIPBOARD_COPY_OVERRIDE
        .lock()
        .expect("clipboard override lock poisoned");
    *slot = Some(Arc::new(copy));
}

/// Remove any installed clipboard copy override.
pub fn clear_clipboard_copy_override() {
    let mut slot = CLIPBOARD_COPY_OVERRIDE
        .lock()
        .expect("clipboard override lock poisoned");
    *slot = None;
}

/// Copy `text` to the clipboard, using the installed override if any and
/// otherwise the Windows clipboard; returns whether the copy succeeded.
pub fn try_copy_text_to_clipboard_windows(text: &str) -> bool {
    if let Some(copy) = CLIPBOARD_COPY_OVERRIDE
        .lock()
        .expect("clipboard override lock poisoned")
        .as_ref()
        .cloned()
    {
        return copy(text);
    }
    copy_text_to_windows_clipboard(text)
}

#[cfg(not(windows))]
fn copy_text_to_windows_clipboard(_text: &str) -> bool {
    false
}

#[cfg(windows)]
fn copy_text_to_windows_clipboard(text: &str) -> bool {
    use std::ffi::c_void;

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(hwnd_new_owner: *mut c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
        fn CloseClipboard() -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    }

    let wide = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let bytes = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        let mut copied = false;
        if EmptyClipboard() != 0 {
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if !handle.is_null() {
                let locked = GlobalLock(handle).cast::<u16>();
                if locked.is_null() {
                    let _ = GlobalFree(handle);
                } else {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), locked, wide.len());
                    let _ = GlobalUnlock(handle);
                    if !SetClipboardData(CF_UNICODETEXT, handle).is_null() {
                        copied = true;
                    } else {
                        let _ = GlobalFree(handle);
                    }
                }
            }
        }
        let _ = CloseClipboard();
        copied
    }
}
