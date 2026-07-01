// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{Database, Error, Result, ScriptExecutionMode, StepResult};
use libsqlite3_sys as ffi;
use std::ffi::CString;
use std::time::Instant;

/// Options for the canonical multi-statement SQL runner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScriptOptions {
    /// Keep running subsequent statements after one fails (otherwise stop at the first error).
    pub continue_on_error: bool,
    /// Record each statement's source SQL in its [`StatementResult::sql`].
    pub include_sql: bool,
}

/// Output for one SQL statement inside a script.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatementResult {
    /// Zero-based position of this statement within the script.
    pub statement_index: usize,
    /// Whether the statement executed without error.
    pub success: bool,
    /// Column names for the result set (empty for non-query statements).
    pub columns: Vec<String>,
    /// Result rows; each cell is `None` for a SQL NULL or `Some(text)` otherwise.
    pub rows: Vec<Vec<Option<String>>>,
    /// Number of rows in [`rows`](StatementResult::rows).
    pub row_count: usize,
    /// Wall-clock execution time in milliseconds. Matches the C++ reference's
    /// `double elapsed_ms`, which holds an integer-millisecond measurement.
    pub elapsed_ms: f64,
    /// Error message when the statement failed, otherwise `None`.
    pub error: Option<String>,
    /// Source SQL of the statement (populated only when [`ScriptOptions::include_sql`] is set).
    pub sql: String,
}

impl StatementResult {
    /// Returns `true` if the cell at the given row and column is a SQL NULL.
    ///
    /// Out-of-range indices return `false`.
    pub fn is_null_cell(&self, row_index: usize, col_index: usize) -> bool {
        self.rows
            .get(row_index)
            .and_then(|row| row.get(col_index))
            .map(Option::is_none)
            .unwrap_or(false)
    }
}

/// Alias for [`StatementResult`], matching the C++ reference's type name.
pub type ScriptStatementResult = StatementResult;

/// Aggregated result for a multi-statement script run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScriptResult {
    /// Whether every executed statement succeeded.
    pub success: bool,
    /// Total number of statements parsed from the script.
    pub statement_count: usize,
    /// Per-statement results in execution order.
    pub results: Vec<StatementResult>,
    /// Sum of [`StatementResult::row_count`] across all statements.
    pub row_count_total: usize,
    /// Sum of [`StatementResult::elapsed_ms`] across all statements.
    pub elapsed_ms_total: f64,
    /// Index of the first failing statement, or `None` if all succeeded.
    pub first_error_index: Option<usize>,
    /// Message set when the script could not be split into statements (empty otherwise).
    pub parse_error: String,
}

/// Quote a SQL identifier with double quotes, doubling any embedded double quotes.
pub fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Split a multi-statement SQL script into individual trimmed statements.
///
/// Statement boundaries are detected with `sqlite3_complete`; empty fragments
/// are dropped and any trailing incomplete text is kept as a final statement.
pub fn collect_statements(script: &str) -> Result<Vec<String>> {
    let mut statements = Vec::new();
    let mut current = String::new();

    for ch in script.chars() {
        current.push(ch);
        if sqlite_complete(&current)? {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                statements.push(trimmed.to_string());
            }
            current.clear();
        }
    }

    let tail = current.trim();
    if !tail.is_empty() {
        statements.push(tail.to_string());
    }

    Ok(statements)
}

/// Alias for [`collect_statements`]: split a script into individual statements.
pub fn split_script(script: &str) -> Result<Vec<String>> {
    collect_statements(script)
}

/// Run a script in fail-fast mode and return only the statements that produced rows.
///
/// Statements with no columns are filtered out. On failure, returns an
/// [`Error::Message`] carrying the first statement's error.
pub fn execute_script(db: &mut Database, script: &str) -> Result<Vec<StatementResult>> {
    let result = run_script(db, script, ScriptExecutionMode::FailFast)?;
    if result.success {
        Ok(result
            .results
            .into_iter()
            .filter(|statement| !statement.columns.is_empty())
            .collect())
    } else {
        let message = result
            .first_error_index
            .and_then(|index| {
                result
                    .results
                    .iter()
                    .find(|statement| statement.statement_index == index)
            })
            .and_then(|statement| statement.error.clone())
            .unwrap_or_else(|| "script execution failed".to_string());
        Err(Error::Message(message))
    }
}

/// Execute a script against `db` with SQL recording enabled, honoring the given mode.
///
/// `mode` selects fail-fast vs. continue-on-error. A parse failure is surfaced
/// as an [`Error::Message`]; statement-level errors are reported within the
/// returned [`ScriptResult`].
pub fn run_script(
    db: &mut Database,
    script: &str,
    mode: ScriptExecutionMode,
) -> Result<ScriptResult> {
    let options = ScriptOptions {
        continue_on_error: mode == ScriptExecutionMode::ContinueOnError,
        include_sql: true,
    };
    let result = run_database_script(db, script, options);
    if result.parse_error.is_empty() {
        Ok(result)
    } else {
        Err(Error::Message(result.parse_error))
    }
}

/// Execute a script against `db` with the given [`ScriptOptions`], timing each statement.
///
/// Returns a [`ScriptResult`]; a parse failure is reported via
/// [`ScriptResult::parse_error`] rather than as an `Err`.
pub fn run_database_script(
    db: &mut Database,
    script: &str,
    options: ScriptOptions,
) -> ScriptResult {
    run_script_with_executor(script, options, |sql, out| {
        let statement_started = Instant::now();
        match run_one_statement(db, sql) {
            Ok(mut result) => {
                result.success = true;
                result.elapsed_ms = statement_started.elapsed().as_millis() as f64;
                *out = result;
            }
            Err(error) => {
                out.success = false;
                out.error = Some(error.to_string());
                out.elapsed_ms = statement_started.elapsed().as_millis() as f64;
            }
        }
    })
}

/// Split a script and run each statement through a caller-supplied `executor`.
///
/// The executor receives the statement SQL and a mutable [`StatementResult`] to
/// fill in; this function handles statement splitting, indexing, aggregation
/// (row/elapsed totals, first error), and continue-on-error vs. fail-fast
/// stopping per `options`. A parse failure returns a [`ScriptResult`] with
/// [`ScriptResult::parse_error`] set.
pub fn run_script_with_executor<F>(
    script: &str,
    options: ScriptOptions,
    mut executor: F,
) -> ScriptResult
where
    F: FnMut(&str, &mut StatementResult),
{
    let statements = match collect_statements(script) {
        Ok(statements) => statements,
        Err(error) => {
            return ScriptResult {
                success: false,
                parse_error: error.to_string(),
                ..Default::default()
            };
        }
    };
    let mut output = ScriptResult {
        success: true,
        statement_count: statements.len(),
        results: Vec::with_capacity(statements.len()),
        row_count_total: 0,
        elapsed_ms_total: 0.0,
        first_error_index: None,
        parse_error: String::new(),
    };

    for (statement_index, sql) in statements.iter().enumerate() {
        let mut result = StatementResult {
            statement_index,
            ..Default::default()
        };
        executor(sql, &mut result);
        result.statement_index = statement_index;
        result.row_count = result.rows.len();
        if options.include_sql {
            result.sql = sql.clone();
        } else {
            result.sql.clear();
        }
        if result.error.is_some() {
            result.success = false;
        }
        if !result.success {
            if output.first_error_index.is_none() {
                output.first_error_index = Some(statement_index);
            }
            output.success = false;
        }
        output.row_count_total += result.row_count;
        output.elapsed_ms_total += result.elapsed_ms;
        output.results.push(result);
        if !output.success && !options.continue_on_error {
            break;
        }
    }

    output
}

/// Render a script result as human-readable text.
///
/// Each statement is prefixed with a `-- statement i/N (ms, rows)` comment
/// followed by an aligned column table, or an `ERROR:` line on failure. A parse
/// error yields a single `PARSE ERROR:` line.
pub fn script_result_to_text(result: &ScriptResult) -> String {
    if !result.parse_error.is_empty() {
        return format!("PARSE ERROR: {}", result.parse_error);
    }

    let mut out = String::new();
    for (pos, statement) in result.results.iter().enumerate() {
        out.push_str(&format!(
            "-- statement {}/{} ({} ms, {} row{})\n",
            statement.statement_index + 1,
            result.statement_count,
            statement.elapsed_ms,
            statement.row_count,
            if statement.row_count == 1 { "" } else { "s" }
        ));
        if statement.success {
            out.push_str(&render_statement_table(statement));
        } else {
            out.push_str("ERROR: ");
            out.push_str(statement.error.as_deref().unwrap_or_default());
            out.push('\n');
        }
        if pos + 1 < result.results.len() {
            out.push('\n');
        }
    }
    out
}

/// Render a script result as CSV (RFC 4180 quoting) for terminal / shell
/// consumption. JSON stays the canonical machine format; this is for humans and
/// unix pipes. NULL cells render as empty fields. A single statement is a clean
/// table (header + rows); multiple statements prefix each table with a
/// `# statement i/N` comment and a blank separator. Mirrors the C++ reference's
/// `script_result_to_csv`.
pub fn script_result_to_csv(result: &ScriptResult) -> String {
    script_result_to_delimited(result, ',', true)
}

/// Render a script result as TSV (tab-delimited, no quoting; embedded tab/
/// newline neutralized to keep one record per line). Mirrors the C++ reference's
/// `script_result_to_tsv`.
pub fn script_result_to_tsv(result: &ScriptResult) -> String {
    script_result_to_delimited(result, '\t', false)
}

fn script_result_to_delimited(result: &ScriptResult, delim: char, csv: bool) -> String {
    if !result.parse_error.is_empty() {
        return format!("# PARSE ERROR: {}\n", result.parse_error);
    }
    let mut out = String::new();
    let multi = result.results.len() > 1;
    for (pos, statement) in result.results.iter().enumerate() {
        if multi {
            out.push_str(&format!(
                "# statement {}/{}\n",
                statement.statement_index + 1,
                result.statement_count
            ));
        }
        if statement.success {
            out.push_str(&render_statement_delimited(statement, delim, csv));
        } else {
            out.push_str("# ERROR: ");
            out.push_str(statement.error.as_deref().unwrap_or_default());
            out.push('\n');
        }
        if multi && pos + 1 < result.results.len() {
            out.push('\n');
        }
    }
    out
}

fn render_statement_delimited(statement: &StatementResult, delim: char, csv: bool) -> String {
    let mut out = String::new();
    for (index, column) in statement.columns.iter().enumerate() {
        if index > 0 {
            out.push(delim);
        }
        append_delimited_field(&mut out, Some(column.as_str()), csv);
    }
    out.push('\n');
    for row in &statement.rows {
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                out.push(delim);
            }
            append_delimited_field(&mut out, cell.as_deref(), csv);
        }
        out.push('\n');
    }
    out
}

/// Append one field. `None` (a SQL NULL) becomes an empty field — matching the
/// JSON-null semantics the C++ formatter applies to its "NULL" sentinel. Unlike
/// C++, the Rust row model carries real nullability, so an actual `"NULL"` text
/// value is preserved (the C++ formatter cannot tell the two apart).
fn append_delimited_field(out: &mut String, value: Option<&str>, csv: bool) {
    let value = match value {
        Some(value) => value,
        None => return,
    };
    if csv {
        if !value.contains([',', '"', '\n', '\r']) {
            out.push_str(value);
            return;
        }
        out.push('"');
        for ch in value.chars() {
            if ch == '"' {
                out.push_str("\"\"");
            } else {
                out.push(ch);
            }
        }
        out.push('"');
    } else {
        for ch in value.chars() {
            out.push(if ch == '\t' || ch == '\n' || ch == '\r' {
                ' '
            } else {
                ch
            });
        }
    }
}

/// Serialize a script result to the canonical JSON envelope (without per-statement SQL).
///
/// Requires the `serde` feature.
#[cfg(any(feature = "serde", feature = "thinclient"))]
pub fn script_result_to_json(result: &ScriptResult) -> Result<String> {
    script_result_to_json_with_sql(result, false)
}

/// Serialize a script result to the canonical JSON envelope.
///
/// When `include_sql` is set, each statement object also carries its `sql`
/// field. NULL cells become JSON `null`; a parse error produces an envelope with
/// `success: false` and `parse_error` populated. Requires the `serde` feature.
#[cfg(any(feature = "serde", feature = "thinclient"))]
pub fn script_result_to_json_with_sql(result: &ScriptResult, include_sql: bool) -> Result<String> {
    use serde_json::json;

    if !result.parse_error.is_empty() {
        return Ok(serde_json::to_string(&json!({
            "success": false,
            "statement_count": 0,
            "results": [],
            "row_count_total": 0,
            "elapsed_ms_total": 0,
            "first_error_index": null,
            "parse_error": result.parse_error,
        }))?);
    }

    let results = result
        .results
        .iter()
        .map(|statement| {
            let rows = statement
                .rows
                .iter()
                .map(|row| {
                    serde_json::Value::Array(
                        row.iter()
                            .map(|cell| match cell {
                                Some(value) => serde_json::Value::String(value.clone()),
                                None => serde_json::Value::Null,
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();
            let mut object = serde_json::Map::new();
            object.insert(
                "statement_index".to_string(),
                json!(statement.statement_index),
            );
            object.insert("success".to_string(), json!(statement.success));
            if include_sql && !statement.sql.is_empty() {
                object.insert("sql".to_string(), json!(statement.sql));
            }
            object.insert("columns".to_string(), json!(statement.columns));
            object.insert("rows".to_string(), serde_json::Value::Array(rows));
            object.insert("row_count".to_string(), json!(statement.row_count));
            object.insert(
                "elapsed_ms".to_string(),
                elapsed_ms_json(statement.elapsed_ms),
            );
            object.insert("error".to_string(), json!(statement.error));
            serde_json::Value::Object(object)
        })
        .collect::<Vec<_>>();

    Ok(serde_json::to_string(&json!({
        "success": result.success,
        "statement_count": result.statement_count,
        "results": results,
        "row_count_total": result.row_count_total,
        "elapsed_ms_total": elapsed_ms_json(result.elapsed_ms_total),
        "first_error_index": result.first_error_index,
    }))?)
}

/// Rebuild a `ScriptResult` from the canonical JSON envelope — the inverse of
/// [`script_result_to_json`]. Used when a serialized result is already in hand
/// (e.g. an HTTP query callback) but must be re-rendered as text/csv/tsv. JSON
/// `null` cells become `None`. Tolerant of missing/extra fields; unparseable
/// input yields a failed `ScriptResult` carrying `parse_error`. Mirrors the C++
/// reference's `json_to_script_result`.
#[cfg(any(feature = "serde", feature = "thinclient"))]
pub fn json_to_script_result(json_str: &str) -> ScriptResult {
    let failed = || ScriptResult {
        success: false,
        parse_error: "json_to_script_result: could not parse result JSON".to_string(),
        ..Default::default()
    };
    let value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(value) => value,
        Err(_) => return failed(),
    };
    let object = match value.as_object() {
        Some(object) => object,
        None => return failed(),
    };

    let results = object
        .get("results")
        .and_then(|results| results.as_array())
        .map(|results| {
            results
                .iter()
                .filter_map(|statement| statement.as_object())
                .map(json_to_statement_result)
                .collect()
        })
        .unwrap_or_default();

    ScriptResult {
        success: object
            .get("success")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        statement_count: object
            .get("statement_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize,
        results,
        row_count_total: object
            .get("row_count_total")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize,
        elapsed_ms_total: object
            .get("elapsed_ms_total")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
        first_error_index: object
            .get("first_error_index")
            .and_then(serde_json::Value::as_u64)
            .map(|index| index as usize),
        parse_error: object
            .get("parse_error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

#[cfg(any(feature = "serde", feature = "thinclient"))]
fn json_to_statement_result(
    object: &serde_json::Map<String, serde_json::Value>,
) -> StatementResult {
    let columns = object
        .get("columns")
        .and_then(|columns| columns.as_array())
        .map(|columns| {
            columns
                .iter()
                .map(|column| match column.as_str() {
                    Some(text) => text.to_string(),
                    None => column.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let rows = object
        .get("rows")
        .and_then(|rows| rows.as_array())
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|cells| {
                            cells
                                .iter()
                                .map(|cell| {
                                    if cell.is_null() {
                                        None
                                    } else if let Some(text) = cell.as_str() {
                                        Some(text.to_string())
                                    } else {
                                        Some(cell.to_string())
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();

    StatementResult {
        statement_index: object
            .get("statement_index")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize,
        success: object
            .get("success")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        columns,
        rows,
        row_count: object
            .get("row_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize,
        elapsed_ms: object
            .get("elapsed_ms")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
        error: object
            .get("error")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        sql: object
            .get("sql")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// Serialize an elapsed-millisecond value the way the C++ `append_json_number`
/// does: whole values become integers (`0`, `1`), keeping byte-for-byte parity
/// with the reference; fractional values fall back to a JSON float.
#[cfg(any(feature = "serde", feature = "thinclient"))]
fn elapsed_ms_json(value: f64) -> serde_json::Value {
    if value.is_finite() && value == (value as i64) as f64 {
        serde_json::json!(value as i64)
    } else {
        serde_json::json!(value)
    }
}

#[cfg(any(feature = "serde", feature = "thinclient"))]
impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::Message(value.to_string())
    }
}

fn render_statement_table(statement: &StatementResult) -> String {
    if statement.columns.is_empty() {
        return "(no result)\n".to_string();
    }

    let rendered_rows = statement
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| value.as_deref().unwrap_or("NULL").to_string())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut widths = statement
        .columns
        .iter()
        .map(String::len)
        .collect::<Vec<_>>();
    for row in &rendered_rows {
        for (index, value) in row.iter().enumerate().take(widths.len()) {
            widths[index] = widths[index].max(value.len());
        }
    }

    let mut out = String::new();
    write_table_row(&mut out, &statement.columns, &widths);
    let separator = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>();
    write_table_row(&mut out, &separator, &widths);
    for row in &rendered_rows {
        write_table_row(&mut out, row, &widths);
    }
    out
}

fn write_table_row(out: &mut String, cells: &[String], widths: &[usize]) {
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(cell);
        if index + 1 < cells.len() {
            let width = widths.get(index).copied().unwrap_or(cell.len());
            if cell.len() < width {
                out.push_str(&" ".repeat(width - cell.len()));
            }
        }
    }
    out.push('\n');
}

fn run_one_statement(db: &mut Database, sql: &str) -> Result<StatementResult> {
    let mut statement = db.prepare(sql)?;
    let column_count = statement.column_count();
    let mut result = StatementResult {
        columns: Vec::new(),
        rows: Vec::new(),
        ..Default::default()
    };

    if column_count > 0 {
        for col in 0..column_count {
            result.columns.push(statement.column_name(col));
        }
    }

    loop {
        match statement.step() {
            StepResult::Row => {
                let mut row = Vec::with_capacity(column_count as usize);
                for col in 0..column_count {
                    row.push(if statement.column_is_null(col) {
                        None
                    } else {
                        Some(statement.text(col))
                    });
                }
                result.rows.push(row);
            }
            StepResult::Done => return Ok(result),
            StepResult::Busy | StepResult::Error => {
                let message = if statement.error().is_empty() {
                    db.last_error().to_string()
                } else {
                    statement.error().to_string()
                };
                return Err(Error::Message(message));
            }
        }
    }
}

fn sqlite_complete(sql: &str) -> Result<bool> {
    let sql = CString::new(sql)?;
    Ok(unsafe { ffi::sqlite3_complete(sql.as_ptr()) != 0 })
}
