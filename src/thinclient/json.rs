// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use serde_json::json;

/// Build a `{"success": false, "error": ...}` JSON string.
pub fn make_error_json(error: impl AsRef<str>) -> String {
    json_error(error)
}

/// Build a `{"success": true}` JSON string, including `"message"` when non-empty.
pub fn make_success_json(message: impl AsRef<str>) -> String {
    let message = message.as_ref();
    if message.is_empty() {
        json!({"success": true}).to_string()
    } else {
        json!({"success": true, "message": message}).to_string()
    }
}

/// Build a status JSON string (`success`/`status`/`tool`), splicing in
/// `extra_json` as additional object members when non-empty.
pub fn make_status_json(tool: impl AsRef<str>, extra_json: impl AsRef<str>) -> String {
    let mut out = format!(
        "{{\"success\":true,\"status\":\"ok\",\"tool\":\"{}\"",
        json_escape(tool.as_ref())
    );
    let extra_json = extra_json.as_ref();
    if !extra_json.is_empty() {
        out.push(',');
        out.push_str(extra_json);
    }
    out.push('}');
    out
}

/// Query-result shape consumed by [`result_to_json`].
pub trait ThinclientJsonResult {
    /// Whether the query succeeded.
    fn success(&self) -> bool;

    /// Result column names (empty by default).
    fn columns(&self) -> &[String] {
        &[]
    }

    /// Result rows as string cells (empty by default).
    fn rows(&self) -> Vec<Vec<String>> {
        Vec::new()
    }

    /// Error message when [`Self::success`] is `false` (empty by default).
    fn error(&self) -> &str {
        ""
    }
}

/// Concrete [`ThinclientJsonResult`] holding owned columns, rows, and error.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JsonQueryResult {
    /// Whether the query succeeded.
    pub success: bool,
    /// Result column names.
    pub columns: Vec<String>,
    /// Result rows as string cells.
    pub rows: Vec<Vec<String>>,
    /// Error message when [`Self::success`] is `false`.
    pub error: String,
}

impl ThinclientJsonResult for JsonQueryResult {
    fn success(&self) -> bool {
        self.success
    }

    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows.clone()
    }

    fn error(&self) -> &str {
        &self.error
    }
}

impl ThinclientJsonResult for crate::QueryResult {
    fn success(&self) -> bool {
        true
    }

    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows.iter().map(|row| row.values.clone()).collect()
    }
}

/// Serialize a [`ThinclientJsonResult`] to the standard query response JSON:
/// `columns`/`rows`/`row_count` on success, or `error` on failure.
pub fn result_to_json<R>(result: &R) -> String
where
    R: ThinclientJsonResult + ?Sized,
{
    if result.success() {
        let rows = result.rows();
        json!({
            "success": true,
            "columns": result.columns(),
            "rows": rows,
            "row_count": rows.len(),
        })
        .to_string()
    } else {
        json!({
            "success": false,
            "error": result.error(),
        })
        .to_string()
    }
}

pub(crate) fn json_error(error: impl AsRef<str>) -> String {
    json!({"success": false, "error": error.as_ref()}).to_string()
}

pub(crate) fn json_error_with_hint(error: impl AsRef<str>, hint: impl AsRef<str>) -> String {
    json!({
        "success": false,
        "error": error.as_ref(),
        "hint": hint.as_ref(),
    })
    .to_string()
}

/// Escape `value` for inclusion inside a JSON string literal (quotes,
/// backslashes, control characters).
pub fn json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if ch < ' ' => out.push_str(&format!("\\u{:04x}", ch as u32)),
            _ => out.push(ch),
        }
    }
    out
}
