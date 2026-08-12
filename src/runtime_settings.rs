// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

//! Typed runtime settings and the canonical transactional SQL table.
//!
//! Value settings are changed with `UPDATE runtime_settings`; only the
//! imperative `timeout_push` and `timeout_pop` operations remain PRAGMAs.

use crate::vtab::{
    CachedTableDef, TransactionHooks, TransactionState, cached_table, set_vtab_error,
};
use crate::{Error, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};

const MAX_TIMEOUT_MS: i64 = 3_600_000;
const MAX_QUEUE_LIMIT: i64 = 10_000;

/// Immutable copy of the common runtime settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeSettingsSnapshot {
    /// Per-query execution timeout, in milliseconds.
    pub query_timeout_ms: i32,
    /// Queue admission timeout, in milliseconds.
    pub queue_admission_timeout_ms: i32,
    /// Maximum number of admitted queued queries.
    pub max_queue: usize,
    /// Whether decompiler/planner hints are enabled.
    pub hints_enabled: bool,
    /// Current nested timeout-stack depth.
    pub timeout_stack_depth: usize,
}

impl Default for RuntimeSettingsSnapshot {
    fn default() -> Self {
        Self {
            query_timeout_ms: 60_000,
            queue_admission_timeout_ms: 120_000,
            max_queue: 64,
            hints_enabled: true,
            timeout_stack_depth: 0,
        }
    }
}

/// Construction options for [`RuntimeSettingsCore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeSettingsCoreOptions {
    /// Maximum `timeout_push` depth; `0` means unbounded.
    pub max_timeout_stack_depth: usize,
}

impl Default for RuntimeSettingsCoreOptions {
    fn default() -> Self {
        Self {
            max_timeout_stack_depth: 64,
        }
    }
}

/// Type of a registered runtime setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeSettingType {
    /// Boolean, canonicalized to `"1"` or `"0"`.
    Boolean,
    /// Signed integer with inclusive bounds.
    Integer,
    /// Uninterpreted UTF-8 text.
    String,
}

impl RuntimeSettingType {
    fn name(self) -> &'static str {
        match self {
            Self::Boolean => "bool",
            Self::Integer => "int",
            Self::String => "string",
        }
    }
}

/// Registration metadata for one runtime setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSettingSpec {
    /// Stable key.
    pub key: String,
    /// Value type.
    pub setting_type: RuntimeSettingType,
    /// Owning scope (`common`, `action`, or a product name).
    pub scope: String,
    /// Canonical default value.
    pub default_value: String,
    /// Whether SQL may update the setting.
    pub writable: bool,
    /// Inclusive integer minimum.
    pub minimum: i64,
    /// Inclusive integer maximum.
    pub maximum: i64,
}

/// One row in `runtime_settings(key, value, type, scope)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSettingEntry {
    /// Setting key.
    pub key: String,
    /// Live or connection-staged canonical value.
    pub value: String,
    /// `"bool"`, `"int"`, or `"string"`.
    pub value_type: String,
    /// Owning scope.
    pub scope: String,
}

/// Internal adapter row used by [`define_runtime_settings_table`].
///
/// It is public only because Rust requires a public return type for the table
/// definition; consumers should treat it as opaque.
#[doc(hidden)]
#[derive(Clone)]
pub struct RuntimeSettingsTableRow {
    entry: RuntimeSettingEntry,
    connection_state: Option<TransactionState>,
}

#[derive(Clone, Debug)]
struct SettingRecord {
    spec: RuntimeSettingSpec,
    value: String,
}

#[derive(Debug, Default)]
struct State {
    order: Vec<String>,
    settings: HashMap<String, SettingRecord>,
    timeout_stack: Vec<i32>,
    staged_query_timeout_transactions: usize,
}

/// Thread-safe typed runtime-setting registry shared by SQL and transports.
#[derive(Debug)]
pub struct RuntimeSettingsCore {
    state: Mutex<State>,
    max_timeout_stack_depth: usize,
}

impl RuntimeSettingsCore {
    /// Create a core and register the common settings and action rows.
    #[must_use]
    pub fn new(options: RuntimeSettingsCoreOptions) -> Self {
        let core = Self {
            state: Mutex::new(State::default()),
            max_timeout_stack_depth: options.max_timeout_stack_depth,
        };
        assert!(core.register_integer_setting(
            "query_timeout_ms",
            60_000,
            0,
            MAX_TIMEOUT_MS,
            "common",
            true,
        ));
        assert!(core.register_integer_setting(
            "queue_admission_timeout_ms",
            120_000,
            0,
            MAX_TIMEOUT_MS,
            "common",
            true,
        ));
        assert!(
            core.register_integer_setting("max_queue", 64, 0, MAX_QUEUE_LIMIT, "common", true,)
        );
        assert!(core.register_bool_setting("hints_enabled", true, "common", true));
        assert!(core.register_integer_setting(
            "timeout_stack_depth",
            0,
            0,
            i64::MAX,
            "common",
            false,
        ));
        assert!(core.register_integer_setting(
            "max_timeout_stack_depth",
            i64::try_from(options.max_timeout_stack_depth).unwrap_or(i64::MAX),
            0,
            i64::MAX,
            "common",
            false,
        ));
        assert!(core.register_integer_setting(
            "timeout_push",
            60_000,
            0,
            MAX_TIMEOUT_MS,
            "action",
            false,
        ));
        assert!(core.register_integer_setting(
            "timeout_pop",
            60_000,
            0,
            MAX_TIMEOUT_MS,
            "action",
            false,
        ));
        core
    }

    /// Register a boolean setting. Duplicate/empty keys or scopes are rejected.
    pub fn register_bool_setting(
        &self,
        key: impl Into<String>,
        default_value: bool,
        scope: impl Into<String>,
        writable: bool,
    ) -> bool {
        self.register_setting(RuntimeSettingSpec {
            key: key.into(),
            setting_type: RuntimeSettingType::Boolean,
            scope: scope.into(),
            default_value: if default_value { "1" } else { "0" }.to_string(),
            writable,
            minimum: i64::MIN,
            maximum: i64::MAX,
        })
    }

    /// Register a bounded integer setting.
    #[allow(clippy::too_many_arguments)]
    pub fn register_integer_setting(
        &self,
        key: impl Into<String>,
        default_value: i64,
        minimum: i64,
        maximum: i64,
        scope: impl Into<String>,
        writable: bool,
    ) -> bool {
        if minimum > maximum || !(minimum..=maximum).contains(&default_value) {
            return false;
        }
        self.register_setting(RuntimeSettingSpec {
            key: key.into(),
            setting_type: RuntimeSettingType::Integer,
            scope: scope.into(),
            default_value: default_value.to_string(),
            writable,
            minimum,
            maximum,
        })
    }

    /// Register a string setting.
    pub fn register_string_setting(
        &self,
        key: impl Into<String>,
        default_value: impl Into<String>,
        scope: impl Into<String>,
        writable: bool,
    ) -> bool {
        self.register_setting(RuntimeSettingSpec {
            key: key.into(),
            setting_type: RuntimeSettingType::String,
            scope: scope.into(),
            default_value: default_value.into(),
            writable,
            minimum: i64::MIN,
            maximum: i64::MAX,
        })
    }

    /// Return all specifications in deterministic registration order.
    #[must_use]
    pub fn specs(&self) -> Vec<RuntimeSettingSpec> {
        let state = self.lock();
        state
            .order
            .iter()
            .map(|key| state.settings[key].spec.clone())
            .collect()
    }

    /// Return all live rows in deterministic registration order.
    #[must_use]
    pub fn enumerate(&self) -> Vec<RuntimeSettingEntry> {
        let state = self.lock();
        state
            .order
            .iter()
            .map(|key| {
                let record = &state.settings[key];
                RuntimeSettingEntry {
                    key: key.clone(),
                    value: Self::effective_value(&state, key, record),
                    value_type: record.spec.setting_type.name().to_string(),
                    scope: record.spec.scope.clone(),
                }
            })
            .collect()
    }

    /// Return only common and action rows.
    #[must_use]
    pub fn enumerate_common(&self) -> Vec<RuntimeSettingEntry> {
        self.enumerate()
            .into_iter()
            .filter(|row| row.scope == "common" || row.scope == "action")
            .collect()
    }

    /// Validate and canonicalize a value without changing committed state.
    #[must_use]
    pub fn normalize(&self, key: &str, value: &str, prefix: &str) -> RuntimePragmaReply {
        let state = self.lock();
        Self::normalize_locked(&state, key, value, prefix)
    }

    /// Validate and immediately commit a direct (non-SQL-transactional) write.
    pub fn apply(&self, key: &str, value: &str, prefix: &str) -> RuntimePragmaReply {
        let mut state = self.lock();
        let reply = Self::normalize_locked(&state, key, value, prefix);
        if reply.handled
            && reply.success
            && let Some(record) = state.settings.get_mut(key)
        {
            record.value.clone_from(&reply.value);
        }
        reply
    }

    /// Return one effective committed value.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<String> {
        let state = self.lock();
        state
            .settings
            .get(key)
            .map(|record| Self::effective_value(&state, key, record))
    }

    /// Return a boolean value or `fallback`.
    #[must_use]
    pub fn bool_value(&self, key: &str, fallback: bool) -> bool {
        self.value(key).map_or(fallback, |value| value == "1")
    }

    /// Return an integer value or `fallback`.
    #[must_use]
    pub fn integer_value(&self, key: &str, fallback: i64) -> i64 {
        self.value(key)
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback)
    }

    /// Take an immutable common-settings snapshot.
    #[must_use]
    pub fn snapshot(&self) -> RuntimeSettingsSnapshot {
        let state = self.lock();
        RuntimeSettingsSnapshot {
            query_timeout_ms: Self::integer_locked(&state, "query_timeout_ms") as i32,
            queue_admission_timeout_ms: Self::integer_locked(&state, "queue_admission_timeout_ms")
                as i32,
            max_queue: Self::integer_locked(&state, "max_queue") as usize,
            hints_enabled: state.settings["hints_enabled"].value == "1",
            timeout_stack_depth: state.timeout_stack.len(),
        }
    }

    /// Current query timeout.
    #[must_use]
    pub fn query_timeout_ms(&self) -> i32 {
        self.integer_value("query_timeout_ms", 60_000) as i32
    }

    /// Current queue admission timeout.
    #[must_use]
    pub fn queue_admission_timeout_ms(&self) -> i32 {
        self.integer_value("queue_admission_timeout_ms", 120_000) as i32
    }

    /// Current maximum admitted query count.
    #[must_use]
    pub fn max_queue(&self) -> usize {
        self.integer_value("max_queue", 64) as usize
    }

    /// Whether hints are enabled.
    #[must_use]
    pub fn hints_enabled(&self) -> bool {
        self.bool_value("hints_enabled", true)
    }

    /// Configured timeout-stack cap (`0` means unbounded).
    #[must_use]
    pub fn max_timeout_stack_depth(&self) -> usize {
        self.max_timeout_stack_depth
    }

    /// Directly commit the query timeout.
    pub fn set_query_timeout_ms(&self, value: i32) -> bool {
        self.apply("query_timeout_ms", &value.to_string(), "runtime")
            .success
    }

    /// Directly commit the queue admission timeout.
    pub fn set_queue_admission_timeout_ms(&self, value: i32) -> bool {
        self.apply("queue_admission_timeout_ms", &value.to_string(), "runtime")
            .success
    }

    /// Directly commit the maximum admitted query count.
    pub fn set_max_queue(&self, value: usize) -> bool {
        i64::try_from(value).is_ok_and(|value| {
            self.apply("max_queue", &value.to_string(), "runtime")
                .success
        })
    }

    /// Directly commit the hints toggle.
    pub fn set_hints_enabled(&self, enabled: bool) {
        let _ = self.apply("hints_enabled", if enabled { "1" } else { "0" }, "runtime");
    }

    /// Push and replace the effective query timeout.
    pub fn timeout_push(&self, timeout_ms: i32) -> Option<i32> {
        if !Self::is_valid_timeout(timeout_ms) {
            return None;
        }
        let mut state = self.lock();
        if state.staged_query_timeout_transactions != 0
            || (self.max_timeout_stack_depth != 0
                && state.timeout_stack.len() >= self.max_timeout_stack_depth)
        {
            return None;
        }
        let previous = Self::integer_locked(&state, "query_timeout_ms") as i32;
        state.timeout_stack.push(previous);
        state.settings.get_mut("query_timeout_ms")?.value = timeout_ms.to_string();
        Some(timeout_ms)
    }

    /// Pop and restore the previous query timeout.
    pub fn timeout_pop(&self) -> Option<i32> {
        let mut state = self.lock();
        if state.staged_query_timeout_transactions != 0 {
            return None;
        }
        let previous = state.timeout_stack.pop()?;
        state.settings.get_mut("query_timeout_ms")?.value = previous.to_string();
        Some(previous)
    }

    /// Whether a timeout is within the common range.
    #[must_use]
    pub fn is_valid_timeout(value: i32) -> bool {
        (0..=MAX_TIMEOUT_MS as i32).contains(&value)
    }

    /// Whether a timeout-stack action is active.
    #[must_use]
    pub fn timeout_stack_active(&self) -> bool {
        !self.lock().timeout_stack.is_empty()
    }

    /// Whether any SQL transaction owns a staged query-timeout write.
    #[must_use]
    pub fn staged_query_timeout_active(&self) -> bool {
        self.lock().staged_query_timeout_transactions != 0
    }

    /// Upper bound on a string setting's byte length, mirroring the C++
    /// `kMaxStringSettingBytes`. A staged value lives in the per-connection
    /// overlay until COMMIT, so an unbounded blob is a per-connection memory hole.
    const MAX_STRING_SETTING_BYTES: usize = 4096;

    fn register_setting(&self, spec: RuntimeSettingSpec) -> bool {
        if spec.key.trim().is_empty() || spec.scope.trim().is_empty() {
            return false;
        }
        // A string setting's default must satisfy the same bounds as a written
        // value: non-empty and within the byte cap (C++ validates the default at
        // registration too).
        if matches!(spec.setting_type, RuntimeSettingType::String)
            && (spec.default_value.is_empty()
                || spec.default_value.len() > Self::MAX_STRING_SETTING_BYTES)
        {
            return false;
        }
        let mut state = self.lock();
        if state.settings.contains_key(&spec.key) {
            return false;
        }
        let key = spec.key.clone();
        let value = spec.default_value.clone();
        state
            .settings
            .insert(key.clone(), SettingRecord { spec, value });
        state.order.push(key);
        true
    }

    fn normalize_locked(state: &State, key: &str, input: &str, prefix: &str) -> RuntimePragmaReply {
        let Some(record) = state.settings.get(key) else {
            return RuntimePragmaReply::default();
        };
        if record.spec.scope == "action" {
            return RuntimePragmaReply::default();
        }
        let mut reply = RuntimePragmaReply {
            handled: true,
            name: key.to_string(),
            ..RuntimePragmaReply::default()
        };
        if !record.spec.writable {
            reply.error = format!("runtime_settings: '{key}' is read-only");
            return reply;
        }

        reply.value = match record.spec.setting_type {
            RuntimeSettingType::Boolean => match parse_bool_value(input) {
                Some(true) => "1".to_string(),
                Some(false) => "0".to_string(),
                None => {
                    reply.error = format!("Invalid {prefix}.{key} value");
                    return reply;
                }
            },
            RuntimeSettingType::Integer => {
                let Ok(value) = input.trim().parse::<i64>() else {
                    reply.error = format!("Invalid {prefix}.{key} value");
                    return reply;
                };
                if !(record.spec.minimum..=record.spec.maximum).contains(&value) {
                    reply.error = format!("Invalid {prefix}.{key} value");
                    return reply;
                }
                value.to_string()
            }
            RuntimeSettingType::String => {
                // Reject empty (the writable table already rejects NULL, and an
                // empty string is the same "unset" shape) and bound the size.
                if input.is_empty() {
                    reply.error = format!("Invalid {prefix}.{key} value (may not be empty)");
                    return reply;
                }
                if input.len() > Self::MAX_STRING_SETTING_BYTES {
                    reply.error = format!(
                        "Invalid {prefix}.{key} value (exceeds {} bytes)",
                        Self::MAX_STRING_SETTING_BYTES
                    );
                    return reply;
                }
                input.to_string()
            }
        };
        reply.success = true;
        reply
    }

    fn effective_value(state: &State, key: &str, record: &SettingRecord) -> String {
        match key {
            "timeout_stack_depth" => state.timeout_stack.len().to_string(),
            "timeout_push" | "timeout_pop" => state.settings["query_timeout_ms"].value.clone(),
            _ => record.value.clone(),
        }
    }

    fn integer_locked(state: &State, key: &str) -> i64 {
        state.settings[key].value.parse().unwrap_or(0)
    }

    fn begin_staged_query_timeout(&self) -> bool {
        let mut state = self.lock();
        if !state.timeout_stack.is_empty() {
            return false;
        }
        state.staged_query_timeout_transactions += 1;
        true
    }

    fn commit_staged(&self, staged: &mut HashMap<String, String>, owns_timeout: bool) {
        let mut state = self.lock();
        for (key, value) in staged {
            if let Some(record) = state.settings.get_mut(key)
                && record.spec.writable
            {
                std::mem::swap(&mut record.value, value);
            }
        }
        if owns_timeout && state.staged_query_timeout_transactions != 0 {
            state.staged_query_timeout_transactions -= 1;
        }
    }

    fn discard_staged(&self, owns_timeout: bool) {
        if !owns_timeout {
            return;
        }
        let mut state = self.lock();
        if state.staged_query_timeout_transactions != 0 {
            state.staged_query_timeout_transactions -= 1;
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for RuntimeSettingsCore {
    fn default() -> Self {
        Self::new(RuntimeSettingsCoreOptions::default())
    }
}

/// A parsed `PRAGMA <prefix>.<key>[=<value>]` request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimePragmaRequest {
    /// Whether the SQL matched the requested product prefix.
    pub matched: bool,
    /// Whether an assignment was present.
    pub has_value: bool,
    /// Lowercase key without the product prefix.
    pub key: String,
    /// Unquoted assignment value.
    pub value: String,
}

/// Result of handling a runtime PRAGMA.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimePragmaReply {
    /// Whether the handler recognized the key.
    pub handled: bool,
    /// Whether the operation succeeded.
    pub success: bool,
    /// Result column name.
    pub name: String,
    /// Canonical result value.
    pub value: String,
    /// Failure message.
    pub error: String,
}

/// Replace every SQL comment (line `--…`, block `/*…*/`) with a single space,
/// leaving string/quoted literals untouched. Mirrors the C++ one-pass
/// `strip_sql_comments`: a line comment ends at `\n` OR a lone `\r`, a block
/// comment runs to `*/` or end-of-input, and a doubled quote inside a literal
/// stays inside it. Stripping comments anywhere (not just at the ends) is what
/// lets a `;;` or an interior comment normalize the same way the C++ scanner does.
fn strip_sql_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\'' || c == b'"' {
            let quote = c;
            out.push(c);
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i]);
                if bytes[i] == quote {
                    if i + 1 < bytes.len() && bytes[i + 1] == quote {
                        out.push(bytes[i + 1]); // escaped quote: stay inside
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                i += 1;
            }
            out.push(b' ');
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            match bytes[i + 2..].windows(2).position(|w| w == b"*/") {
                Some(pos) => i = i + 2 + pos + 2,
                None => i = bytes.len(),
            }
            out.push(b' ');
            continue;
        }
        out.push(c);
        i += 1;
    }
    // `out` only ever holds bytes copied verbatim from `text` (a valid &str) plus
    // ASCII spaces, so it is always valid UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// Trim leading/trailing ASCII whitespace.
#[must_use]
pub fn trim_copy(text: &str) -> &str {
    text.trim_matches(|character: char| character.is_ascii_whitespace())
}

/// Return an ASCII-lowercase copy of `text`.
#[must_use]
pub fn to_lower_copy(text: &str) -> String {
    text.chars()
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// Strip one matching pair of surrounding single or double quotes.
#[must_use]
pub fn strip_optional_quotes(text: &str) -> &str {
    if let Some(inner) = text
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
    {
        return inner;
    }
    if let Some(inner) = text
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        return inner;
    }
    text
}

/// Parse a complete base-10 `i32` after trimming ASCII whitespace.
#[must_use]
pub fn parse_int_value(text: &str) -> Option<i32> {
    trim_copy(text)
        .parse::<i64>()
        .ok()
        .and_then(|value| i32::try_from(value).ok())
}

/// Parse a common case-insensitive boolean spelling.
#[must_use]
pub fn parse_bool_value(text: &str) -> Option<bool> {
    match to_lower_copy(trim_copy(text)).as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Parse one prefixed runtime PRAGMA, tolerating SQL comments and whitespace.
#[must_use]
pub fn parse_runtime_pragma(sql: &str, product_prefix: &str) -> RuntimePragmaRequest {
    let stripped = strip_sql_comments(sql);
    let mut text = stripped.trim();
    // Statement terminators (also `;;` — comment stripping can expose more than
    // one), each optionally followed by trailing whitespace.
    loop {
        let trimmed = text.trim_end();
        match trimmed.strip_suffix(';') {
            Some(without) => text = without,
            None => {
                text = trimmed;
                break;
            }
        }
    }
    text = text.trim();
    let Some(keyword) = text.get(..6) else {
        return RuntimePragmaRequest::default();
    };
    if !keyword.eq_ignore_ascii_case("pragma") {
        return RuntimePragmaRequest::default();
    }
    // Word boundary: `pragmatic_helper(...)` starts with "pragma" but is not the
    // PRAGMA keyword — the next byte must be ASCII whitespace.
    match text.as_bytes().get(6) {
        Some(byte) if byte.is_ascii_whitespace() => {}
        _ => return RuntimePragmaRequest::default(),
    }
    let body = text.get(6..).unwrap_or_default().trim();
    let runtime_prefix = format!("{product_prefix}.");
    let Some(prefix) = body.get(..runtime_prefix.len()) else {
        return RuntimePragmaRequest::default();
    };
    if !prefix.eq_ignore_ascii_case(&runtime_prefix) {
        return RuntimePragmaRequest::default();
    }

    let expression = body.get(runtime_prefix.len()..).unwrap_or_default().trim();
    let (key, value, has_value) = expression.find('=').map_or_else(
        || (expression, "", false),
        |equals| {
            (
                expression[..equals].trim(),
                expression[equals + 1..].trim(),
                true,
            )
        },
    );
    let value = strip_optional_quotes(value);
    RuntimePragmaRequest {
        matched: true,
        has_value,
        key: key.to_ascii_lowercase(),
        value: value.to_string(),
    }
}

/// Standard unknown-key error for a product PRAGMA dispatcher.
#[must_use]
pub fn unknown_runtime_pragma_error(product_prefix: &str) -> String {
    format!("Unknown {product_prefix} pragma key")
}

/// Handle only the imperative common PRAGMAs (`timeout_push`/`timeout_pop`).
#[must_use]
pub fn handle_common_runtime_pragma(
    request: &RuntimePragmaRequest,
    product_prefix: &str,
    settings: &RuntimeSettingsCore,
) -> RuntimePragmaReply {
    if !request.matched {
        return RuntimePragmaReply::default();
    }
    match request.key.as_str() {
        "timeout_push" => {
            if request.value.is_empty() {
                return pragma_error(format!(
                    "{product_prefix}.timeout_push requires a timeout value"
                ));
            }
            let Ok(timeout_ms) = request.value.trim().parse::<i32>() else {
                return pragma_error(format!("Invalid {product_prefix}.timeout_push value"));
            };
            if let Some(effective) = settings.timeout_push(timeout_ms) {
                return pragma_result("query_timeout_ms", effective.to_string());
            }
            if settings.staged_query_timeout_active() {
                return pragma_error(format!(
                    "{product_prefix}.timeout_push is unavailable while runtime_settings has a staged query_timeout_ms write"
                ));
            }
            if !RuntimeSettingsCore::is_valid_timeout(timeout_ms) {
                return pragma_error(format!("Invalid {product_prefix}.timeout_push value"));
            }
            pragma_error(format!(
                "{product_prefix}.timeout_push stack full (max {} entries)",
                settings.max_timeout_stack_depth()
            ))
        }
        "timeout_pop" => match settings.timeout_pop() {
            Some(effective) => pragma_result("query_timeout_ms", effective.to_string()),
            None if settings.staged_query_timeout_active() => pragma_error(format!(
                "{product_prefix}.timeout_pop is unavailable while runtime_settings has a staged query_timeout_ms write"
            )),
            None => pragma_error(format!("{product_prefix}.timeout_pop stack is empty")),
        },
        _ => RuntimePragmaReply::default(),
    }
}

fn pragma_result(name: &str, value: String) -> RuntimePragmaReply {
    RuntimePragmaReply {
        handled: true,
        success: true,
        name: name.to_string(),
        value,
        error: String::new(),
    }
}

fn pragma_error(error: String) -> RuntimePragmaReply {
    RuntimePragmaReply {
        handled: true,
        success: false,
        name: String::new(),
        value: String::new(),
        error,
    }
}

#[derive(Default)]
struct RuntimeSettingsTransaction {
    staged: HashMap<String, String>,
    savepoints: HashMap<i32, HashMap<String, String>>,
    owns_staged_query_timeout: bool,
}

fn transaction(state: &TransactionState) -> Result<&RefCell<RuntimeSettingsTransaction>> {
    state
        .as_ref()
        .downcast_ref::<RefCell<RuntimeSettingsTransaction>>()
        .ok_or_else(|| {
            Error::Message("runtime_settings: invalid connection-local transaction state".into())
        })
}

fn finish_runtime_settings_transaction(
    settings: &RuntimeSettingsCore,
    state: &TransactionState,
    commit: bool,
) {
    let Ok(transaction) = transaction(state) else {
        return;
    };
    let mut transaction = transaction.borrow_mut();
    if commit {
        let owns_timeout = transaction.owns_staged_query_timeout;
        settings.commit_staged(&mut transaction.staged, owns_timeout);
    } else {
        settings.discard_staged(transaction.owns_staged_query_timeout);
    }
    transaction.staged.clear();
    transaction.savepoints.clear();
    transaction.owns_staged_query_timeout = false;
}

/// Build the canonical transactional
/// `runtime_settings(key TEXT, value TEXT, type TEXT, scope TEXT)` table.
///
/// The same definition may be registered with multiple databases; every
/// registration receives an isolated staged overlay.
pub fn define_runtime_settings_table(
    settings: Arc<RuntimeSettingsCore>,
    product_prefix: impl Into<String>,
) -> Result<CachedTableDef<RuntimeSettingsTableRow>> {
    let product_prefix = product_prefix.into();
    let mut hooks = TransactionHooks {
        state_factory: Some(Rc::new(|| {
            Ok(Rc::new(RefCell::new(RuntimeSettingsTransaction::default())) as TransactionState)
        })),
        ..TransactionHooks::default()
    };

    hooks.commit = Some({
        let settings = settings.clone();
        Rc::new(move |state| finish_runtime_settings_transaction(&settings, state, true))
    });
    hooks.rollback = Some({
        let settings = settings.clone();
        Rc::new(move |state| finish_runtime_settings_transaction(&settings, state, false))
    });
    hooks.savepoint = Some(Rc::new(|state, savepoint| {
        let transaction = transaction(state)?;
        let mut transaction = transaction.borrow_mut();
        let snapshot = transaction.staged.clone();
        transaction.savepoints.insert(savepoint, snapshot);
        Ok(())
    }));
    hooks.release = Some(Rc::new(|state, savepoint| {
        transaction(state)?
            .borrow_mut()
            .savepoints
            .retain(|saved, _| *saved < savepoint);
        Ok(())
    }));
    hooks.rollback_to = Some({
        let settings = settings.clone();
        Rc::new(move |state, savepoint| {
            let transaction = transaction(state)?;
            let mut transaction = transaction.borrow_mut();
            let target = transaction
                .savepoints
                .get(&savepoint)
                .cloned()
                .unwrap_or_default();
            let target_has_timeout = target.contains_key("query_timeout_ms");
            let current_has_timeout = transaction.owns_staged_query_timeout;
            if !current_has_timeout && target_has_timeout && !settings.begin_staged_query_timeout()
            {
                return Err(Error::Message(
                    "runtime_settings: cannot restore staged query_timeout_ms while the timeout stack is active"
                        .into(),
                ));
            }
            if current_has_timeout && !target_has_timeout {
                settings.discard_staged(true);
            }
            transaction.staged = target;
            transaction.owns_staged_query_timeout = target_has_timeout;
            transaction
                .savepoints
                .retain(|saved, _| *saved <= savepoint);
            Ok(())
        })
    });

    let row_settings = settings.clone();
    let setter_settings = settings.clone();
    let estimate_settings = settings.clone();
    cached_table("runtime_settings")
        .no_shared_cache()
        .estimate_rows(move || estimate_settings.specs().len())
        .stateful_cache_builder(move |state, rows| {
            *rows = row_settings
                .enumerate()
                .into_iter()
                .map(|entry| RuntimeSettingsTableRow {
                    entry,
                    connection_state: Some(state.clone()),
                })
                .collect();
            if let Ok(transaction) = transaction(state) {
                let transaction = transaction.borrow();
                for row in rows {
                    if let Some(value) = transaction.staged.get(&row.entry.key) {
                        row.entry.value.clone_from(value);
                    }
                }
            }
        })
        .transaction_hooks(hooks)
        .column_text("key", |row| row.entry.key.clone())
        .column_text_nullable_rw(
            "value",
            |row| Some(row.entry.value.clone()),
            move |row, value| {
                if value.is_nochange() {
                    return true;
                }
                if value.is_null() {
                    set_vtab_error(format!(
                        "runtime_settings: '{}' cannot be set to NULL",
                        row.entry.key
                    ));
                    return false;
                }
                if row.entry.scope == "action" {
                    set_vtab_error(format!(
                        "runtime_settings: '{}' is a PRAGMA verb, not a settable value",
                        row.entry.key
                    ));
                    return false;
                }
                let normalized = setter_settings.normalize(
                    &row.entry.key,
                    &value.as_text(),
                    &product_prefix,
                );
                if !normalized.handled {
                    set_vtab_error(format!(
                        "runtime_settings: unknown key '{}'",
                        row.entry.key
                    ));
                    return false;
                }
                if !normalized.success {
                    set_vtab_error(normalized.error);
                    return false;
                }
                let Some(state) = row.connection_state.as_ref() else {
                    set_vtab_error(
                        "runtime_settings: missing connection-local transaction state",
                    );
                    return false;
                };
                let Ok(transaction) = transaction(state) else {
                    set_vtab_error(
                        "runtime_settings: invalid connection-local transaction state",
                    );
                    return false;
                };
                let mut transaction = transaction.borrow_mut();
                let first_timeout = row.entry.key == "query_timeout_ms"
                    && !transaction.staged.contains_key(&row.entry.key);
                if first_timeout && !setter_settings.begin_staged_query_timeout() {
                    set_vtab_error(
                        "runtime_settings: query_timeout_ms cannot be staged while the timeout stack is active",
                    );
                    return false;
                }
                if transaction.staged.try_reserve(1).is_err() {
                    if first_timeout {
                        setter_settings.discard_staged(true);
                    }
                    set_vtab_error(format!(
                        "runtime_settings: out of memory while staging '{}'",
                        row.entry.key
                    ));
                    return false;
                }
                transaction
                    .staged
                    .insert(row.entry.key.clone(), normalized.value.clone());
                if first_timeout {
                    transaction.owns_staged_query_timeout = true;
                }
                row.entry.value = normalized.value;
                true
            },
        )
        .column_text("type", |row| row.entry.value_type.clone())
        .column_text("scope", |row| row.entry.scope.clone())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    #[test]
    fn typed_registry_canonicalizes_and_rejects_bad_values() {
        let core = RuntimeSettingsCore::default();
        assert!(core.register_string_setting("theme", "dark", "demo", true));
        assert!(!core.register_string_setting("theme", "light", "demo", true));
        assert_eq!(core.apply("hints_enabled", "yes", "demo").value, "1");
        assert_eq!(core.apply("max_queue", "0009", "demo").value, "9");
        assert!(!core.apply("max_queue", "10001", "demo").success);
        assert_eq!(core.value("theme").as_deref(), Some("dark"));
    }

    #[test]
    fn only_timeout_actions_are_common_pragmas() {
        let core = RuntimeSettingsCore::default();
        let typed = parse_runtime_pragma("PRAGMA demo.max_queue=5", "demo");
        assert!(!handle_common_runtime_pragma(&typed, "demo", &core).handled);
        let push = parse_runtime_pragma("/* lead */ PRAGMA demo.timeout_push=5; -- x", "demo");
        assert_eq!(
            handle_common_runtime_pragma(&push, "demo", &core).value,
            "5"
        );
        let pop = parse_runtime_pragma("PRAGMA demo.timeout_pop", "demo");
        assert_eq!(
            handle_common_runtime_pragma(&pop, "demo", &core).value,
            "60000"
        );
    }

    // D3 (1.0.13 parity): the one-pass comment scanner handles interior comments,
    // doubled `;;` terminators, a lone-CR line ending, the `pragma` word boundary,
    // and never treats a comment inside a quoted literal as a comment.
    #[test]
    fn pragma_scanner_handles_comments_terminators_and_word_boundary() {
        let interior = parse_runtime_pragma("PRAGMA demo.max_queue /* c */ = 5 ;;", "demo");
        assert!(interior.matched);
        assert_eq!(interior.key, "max_queue");
        assert_eq!(interior.value, "5");
        assert!(interior.has_value);

        // A lone CR (no LF) terminates a line comment.
        let lone_cr = parse_runtime_pragma("-- lead\rPRAGMA demo.timeout_pop", "demo");
        assert!(lone_cr.matched);
        assert_eq!(lone_cr.key, "timeout_pop");

        // Word boundary: `pragmademo...` (no space after "pragma") is not the keyword.
        let not_keyword = parse_runtime_pragma("pragmademo.max_queue=5", "demo");
        assert!(!not_keyword.matched);

        // A comment-looking sequence inside a quoted value is preserved verbatim.
        let quoted = parse_runtime_pragma("PRAGMA demo.theme = '/* keep */'", "demo");
        assert!(quoted.matched);
        assert_eq!(quoted.value, "/* keep */");
    }

    #[test]
    fn sql_table_stages_commits_and_rolls_back() -> Result<()> {
        let settings = Arc::new(RuntimeSettingsCore::default());
        let definition = define_runtime_settings_table(settings.clone(), "demo")?;
        let mut db = Database::open_memory()?;
        db.register_and_create_cached_table(&definition)?;

        db.exec("BEGIN")?;
        db.exec("UPDATE runtime_settings SET value='on' WHERE key='hints_enabled'")?;
        assert!(settings.hints_enabled());
        assert_eq!(
            db.query("SELECT value FROM runtime_settings WHERE key='hints_enabled'")?
                .rows[0]
                .values[0],
            "1"
        );
        db.exec("ROLLBACK")?;
        assert!(settings.hints_enabled());

        db.exec("BEGIN")?;
        db.exec("UPDATE runtime_settings SET value='off' WHERE key='hints_enabled'")?;
        db.exec("COMMIT")?;
        assert!(!settings.hints_enabled());
        Ok(())
    }
}
