// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::table_printer::{self, TablePrintOptions};
use crate::{Database, Error, Result, ScriptExecutionMode};
use libsqlite3_sys as ffi;
use std::ffi::CString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Options for the canonical multi-statement SQL runner.
///
/// Carries an optional [`should_cancel`](ScriptOptions::should_cancel) closure, so
/// it is [`Clone`] but not `Copy`/`PartialEq`.
#[derive(Clone, Default)]
pub struct ScriptOptions {
    /// Keep running subsequent statements after one fails (otherwise stop at the first error).
    pub continue_on_error: bool,
    /// Record each statement's source SQL in its [`StatementResult::sql`].
    pub include_sql: bool,
    /// Per-statement wall-clock timeout in milliseconds (`0` = no limit). Threaded
    /// into each statement's [`QueryOptions::timeout_ms`](crate::database::QueryOptions);
    /// a read-only statement that hits the deadline comes back `success` with
    /// `partial`/`timed_out` set and a warning. A mutation reports an error and
    /// rolls back instead.
    pub timeout_ms: u64,
    /// Optional cooperative cancellation predicate. When set and it returns `true`,
    /// an in-flight read-only statement stops ASAP and comes back `success` with
    /// `partial` set and a "query cancelled" warning (same shape as a timeout,
    /// but **not** `timed_out` — cancel is not a timeout). A mutation is rolled
    /// back and reports an error. Cancellation is sticky for the whole script:
    /// once observed, no later statement starts even if the original predicate
    /// subsequently returns `false`. Threaded into the buffered path
    /// ([`QueryOptions::should_cancel`](crate::database::QueryOptions), which folds it
    /// into the progress handler / interrupt checker) and checked *between* statements
    /// by [`run_script_with_executor`], so a server can abort a runaway script even
    /// under `timeout_ms == 0`. `None` => never cancelled.
    pub should_cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl std::fmt::Debug for ScriptOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptOptions")
            .field("continue_on_error", &self.continue_on_error)
            .field("include_sql", &self.include_sql)
            .field("timeout_ms", &self.timeout_ms)
            .field(
                "should_cancel",
                &self.should_cancel.as_ref().map(|_| "<fn>"),
            )
            .finish()
    }
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
    /// `true` if this statement hit its deadline (see [`ScriptOptions::timeout_ms`]).
    pub timed_out: bool,
    /// `true` if a read-only timeout, cancellation, or late provider error kept
    /// the rows gathered so far.
    pub partial: bool,
    /// Non-fatal notices — e.g. the partial-rows timeout warning.
    pub warnings: Vec<String>,
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

fn poll_cancellation(
    predicate: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> std::result::Result<bool, &'static str> {
    let Some(predicate) = predicate else {
        return Ok(false);
    };
    catch_unwind(AssertUnwindSafe(|| predicate())).map_err(|_| "cancellation predicate panicked")
}

fn with_sticky_cancellation(mut options: ScriptOptions) -> ScriptOptions {
    let Some(predicate) = options.should_cancel.take() else {
        return options;
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let panicked = Arc::new(AtomicBool::new(false));
    options.should_cancel = Some(Arc::new(move || {
        if panicked.load(Ordering::Acquire) {
            panic!("cancellation predicate panicked");
        }
        if cancelled.load(Ordering::Acquire) {
            return true;
        }
        match catch_unwind(AssertUnwindSafe(|| predicate())) {
            Ok(true) => {
                cancelled.store(true, Ordering::Release);
                true
            }
            Ok(false) => false,
            Err(payload) => {
                panicked.store(true, Ordering::Release);
                std::panic::resume_unwind(payload);
            }
        }
    }));
    options
}

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
        ..Default::default()
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
    let options = with_sticky_cancellation(options);
    // Snapshot the fields the per-statement executor needs, so `options` itself can
    // still be moved into `run_script_with_executor` for the between-statement
    // cancel check.
    let timeout_ms = options.timeout_ms;
    let should_cancel = options.should_cancel.clone();
    run_script_with_executor(script, options, |sql, out| {
        // Reuse Database::query_with_options, which owns the timeout + cancellation
        // machinery (progress handler + interrupt checker + partial-result flags),
        // and round-trip its signalling into the statement result. Mirrors the C++
        // run_database_script executor (which sets qopts.should_cancel too).
        let outcome = db.query_with_options(
            sql,
            crate::database::QueryOptions {
                timeout_ms,
                should_cancel: should_cancel.clone(),
                ..Default::default()
            },
        );
        out.columns = outcome.result.columns;
        out.rows = outcome
            .result
            .rows
            .into_iter()
            .map(|row| {
                row.values
                    .into_iter()
                    .zip(row.nulls)
                    .map(|(value, is_null)| if is_null { None } else { Some(value) })
                    .collect()
            })
            .collect();
        out.elapsed_ms = outcome.elapsed_ms as f64;
        out.success = outcome.error.is_none();
        out.error = outcome.error;
        out.timed_out = outcome.timed_out;
        out.partial = outcome.partial;
        out.warnings = outcome.warnings;
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
    let options = with_sticky_cancellation(options);
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
        // Cooperative cancellation between statements (e.g. a server POST /cancel):
        // don't start a new statement once cancel is requested. Mid-statement
        // cancellation is the executor's job (Database::query honors should_cancel).
        // Mirrors the C++ run_script between-statement check.
        let cancellation_error = match poll_cancellation(&options.should_cancel) {
            Ok(false) => None,
            Ok(true) => Some("query cancelled"),
            Err(message) => Some(message),
        };
        if let Some(error) = cancellation_error {
            output.success = false;
            output.first_error_index = Some(statement_index);
            output.results.push(StatementResult {
                statement_index,
                success: false,
                error: Some(error.to_string()),
                sql: if options.include_sql {
                    sql.clone()
                } else {
                    String::new()
                },
                ..Default::default()
            });
            break;
        }

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
        // Surface the timeout / partial-result signalling — emitted ONLY when set,
        // so a normal result is byte-identical. `--`-prefixed to match the
        // `-- statement` convention. Mirrors the C++ reference.
        for warning in &statement.warnings {
            out.push_str(&format!("-- warning: {warning}\n"));
        }
        if statement.timed_out {
            out.push_str("-- query timed out; results are partial\n");
        } else if statement.partial {
            out.push_str("-- results are partial\n");
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

/// Render a script result as JSON Lines / NDJSON: one self-describing JSON object
/// per row, one per line — keyed by **column name** (the "records" shape a bulk
/// consumer wants, e.g. a symbol cache). Cell values are emitted as JSON strings
/// (SQL NULL => `null`), matching the canonical envelope's cell treatment
/// ([`script_result_to_json`]) so a numeric column like `rva` is `"4096"` exactly
/// as it is there. A failed statement emits one
/// `{"statement_index":I,"error":"…"}` line instead of rows; a parse error emits
/// `{"error":"…"}`; a column-less statement (a successful write) emits nothing. No
/// enclosing array/envelope, so the output streams and appends cleanly. Mirrors the
/// C++ reference's `script_result_to_jsonl`. Requires the `serde` feature.
#[cfg(any(feature = "serde", feature = "thinclient"))]
pub fn script_result_to_jsonl(result: &ScriptResult) -> String {
    use serde_json::{Map, Value, json};
    use std::collections::HashSet;

    // Serialize one JSON object followed by a newline. `serde_json` never fails on a
    // plain object of strings/nulls, so the fallback is unreachable in practice.
    let push_line = |out: &mut String, value: &Value| {
        out.push_str(&serde_json::to_string(value).unwrap_or_default());
        out.push('\n');
    };

    let mut out = String::new();
    if !result.parse_error.is_empty() {
        push_line(&mut out, &json!({ "error": result.parse_error }));
        return out;
    }
    for statement in &result.results {
        if !statement.success {
            push_line(
                &mut out,
                &json!({
                    "statement_index": statement.statement_index,
                    "error": statement.error.as_deref().unwrap_or_default(),
                }),
            );
            continue;
        }
        let mut used = HashSet::new();
        let keys = statement
            .columns
            .iter()
            .map(|column| {
                let mut candidate = column.clone();
                let mut suffix = 2usize;
                while used.contains(&candidate) {
                    candidate = format!("{column}#{suffix}");
                    suffix = suffix.saturating_add(1);
                }
                used.insert(candidate.clone());
                candidate
            })
            .collect::<Vec<_>>();
        for row in &statement.rows {
            // Keyed by column name in column order (preserve_order keeps insertion
            // order). A cell with no matching column, or a SQL NULL, becomes `null`.
            let mut object = Map::new();
            for (col, column) in keys.iter().enumerate() {
                let cell = match row.get(col) {
                    Some(Some(value)) => Value::String(value.clone()),
                    _ => Value::Null,
                };
                object.insert(column.clone(), cell);
            }
            push_line(&mut out, &Value::Object(object));
        }
        if statement.timed_out || statement.partial || !statement.warnings.is_empty() {
            let mut metadata = Map::new();
            metadata.insert(
                "statement_index".to_string(),
                json!(statement.statement_index),
            );
            if statement.timed_out {
                metadata.insert("timed_out".to_string(), Value::Bool(true));
            }
            if statement.partial {
                metadata.insert("partial".to_string(), Value::Bool(true));
            }
            if !statement.warnings.is_empty() {
                metadata.insert("warnings".to_string(), json!(statement.warnings));
            }
            push_line(&mut out, &Value::Object(metadata));
        }
    }
    out
}

/// Execute a script and stream the canonical JSON envelope incrementally.
///
/// At most one read-only result row is resident in the adapter. Mutation
/// `RETURNING` rows are buffered until the statement succeeds so rolled-back
/// provisional rows never reach `sink`. The sink receives small valid JSON
/// fragments; returning `false` stops query execution and suppresses all later
/// writes (for example after an HTTP disconnect). The returned [`ScriptResult`]
/// contains statement metadata but no buffered rows.
#[cfg(any(feature = "serde", feature = "thinclient"))]
pub fn stream_database_script_json<F>(
    db: &mut Database,
    script: &str,
    options: ScriptOptions,
    mut sink: F,
) -> ScriptResult
where
    F: FnMut(&str) -> bool,
{
    use serde_json::{Value, json};
    let options = with_sticky_cancellation(options);

    let statements = match collect_statements(script) {
        Ok(statements) => statements,
        Err(error) => {
            let result = ScriptResult {
                success: false,
                parse_error: error.to_string(),
                ..ScriptResult::default()
            };
            if let Ok(encoded) = script_result_to_json(&result) {
                let _ = sink(&encoded);
            }
            return result;
        }
    };
    let active = std::cell::Cell::new(true);
    let mut write = |chunk: &str| {
        if active.get() && !sink(chunk) {
            active.set(false);
        }
        active.get()
    };
    let mut output = ScriptResult {
        success: true,
        statement_count: statements.len(),
        results: Vec::with_capacity(statements.len()),
        ..ScriptResult::default()
    };
    write("{\"results\":[");

    for (statement_index, sql) in statements.iter().enumerate() {
        if !active.get() {
            break;
        }
        if !output.results.is_empty() {
            write(",");
        }
        let cancellation_error = match poll_cancellation(&options.should_cancel) {
            Ok(false) => None,
            Ok(true) => Some("query cancelled"),
            Err(message) => Some(message),
        };
        if let Some(error) = cancellation_error {
            let statement = StatementResult {
                statement_index,
                success: false,
                error: Some(error.to_string()),
                sql: if options.include_sql {
                    sql.clone()
                } else {
                    String::new()
                },
                ..StatementResult::default()
            };
            let mut object = serde_json::Map::new();
            object.insert("statement_index".into(), json!(statement_index));
            object.insert("success".into(), Value::Bool(false));
            if options.include_sql {
                object.insert("sql".into(), Value::String(sql.clone()));
            }
            object.insert("columns".into(), json!([]));
            object.insert("rows".into(), json!([]));
            object.insert("row_count".into(), json!(0));
            object.insert("elapsed_ms".into(), json!(0));
            object.insert("error".into(), Value::String(error.to_string()));
            write(&Value::Object(object).to_string());
            output.success = false;
            output.first_error_index = Some(statement_index);
            output.results.push(statement);
            break;
        }

        let mut opened = false;
        let mut first_row = true;
        let mut row_count = 0usize;
        let query_options = crate::database::QueryOptions {
            timeout_ms: options.timeout_ms,
            should_cancel: options.should_cancel.clone(),
            ..crate::database::QueryOptions::default()
        };
        let outcome = db.stream_query_with_options(sql, query_options, |columns, row| {
            if !opened {
                write("{\"statement_index\":");
                write(&statement_index.to_string());
                if options.include_sql {
                    write(",\"sql\":");
                    write(&Value::String(sql.clone()).to_string());
                }
                write(",\"columns\":");
                write(&serde_json::to_string(columns).unwrap_or_else(|_| "[]".into()));
                write(",\"rows\":[");
                opened = true;
            }
            if !first_row {
                write(",");
            }
            first_row = false;
            row_count += 1;
            let cells = row
                .values
                .iter()
                .zip(&row.nulls)
                .map(|(value, is_null)| {
                    if *is_null {
                        Value::Null
                    } else {
                        Value::String(value.clone())
                    }
                })
                .collect::<Vec<_>>();
            write(&Value::Array(cells).to_string())
        });
        if !active.get() {
            break;
        }
        if !opened {
            write("{\"statement_index\":");
            write(&statement_index.to_string());
            if options.include_sql {
                write(",\"sql\":");
                write(&Value::String(sql.clone()).to_string());
            }
            write(",\"columns\":");
            write(&serde_json::to_string(&outcome.result.columns).unwrap_or_else(|_| "[]".into()));
            write(",\"rows\":[");
        }
        write("],\"row_count\":");
        write(&row_count.to_string());
        write(",\"elapsed_ms\":");
        write(&outcome.elapsed_ms.to_string());
        write(",\"error\":");
        write(&outcome.error.as_ref().map_or_else(
            || "null".to_string(),
            |error| Value::String(error.clone()).to_string(),
        ));
        write(",\"success\":");
        write(if outcome.error.is_none() {
            "true"
        } else {
            "false"
        });
        if outcome.timed_out {
            write(",\"timed_out\":true");
        }
        if outcome.partial {
            write(",\"partial\":true");
        }
        if !outcome.warnings.is_empty() {
            write(",\"warnings\":");
            write(&serde_json::to_string(&outcome.warnings).unwrap_or_else(|_| "[]".into()));
        }
        write("}");

        let success = outcome.error.is_none();
        let statement = StatementResult {
            statement_index,
            success,
            columns: outcome.result.columns,
            rows: Vec::new(),
            row_count,
            elapsed_ms: outcome.elapsed_ms as f64,
            error: outcome.error,
            sql: if options.include_sql {
                sql.clone()
            } else {
                String::new()
            },
            timed_out: outcome.timed_out,
            partial: outcome.partial,
            warnings: outcome.warnings,
        };
        if !success {
            output.success = false;
            output.first_error_index.get_or_insert(statement_index);
        }
        output.row_count_total += row_count;
        output.elapsed_ms_total += statement.elapsed_ms;
        output.results.push(statement);
        if !success && !options.continue_on_error {
            break;
        }
    }

    if active.get() {
        write("],\"success\":");
        write(if output.success { "true" } else { "false" });
        write(",\"statement_count\":");
        write(&output.statement_count.to_string());
        write(",\"row_count_total\":");
        write(&output.row_count_total.to_string());
        write(",\"elapsed_ms_total\":");
        write(&elapsed_ms_json(output.elapsed_ms_total).to_string());
        write(",\"first_error_index\":");
        write(
            &output
                .first_error_index
                .map_or_else(|| "null".to_string(), |index| index.to_string()),
        );
        write("}");
    }
    output
}

/// Execute a script and stream JSON Lines / NDJSON one result row at a time.
///
/// Failed statements and timeout/partial metadata use the same line shapes as
/// [`script_result_to_jsonl`]. Read-only rows are not retained; mutation
/// `RETURNING` rows are held only until successful statement completion.
#[cfg(any(feature = "serde", feature = "thinclient"))]
pub fn stream_database_script_ndjson<F>(
    db: &mut Database,
    script: &str,
    options: ScriptOptions,
    mut sink: F,
) -> ScriptResult
where
    F: FnMut(&str) -> bool,
{
    use serde_json::{Map, Value, json};
    use std::collections::HashSet;
    let options = with_sticky_cancellation(options);

    let statements = match collect_statements(script) {
        Ok(statements) => statements,
        Err(error) => {
            let parse_error = error.to_string();
            let _ = sink(&format!("{}\n", json!({ "error": parse_error })));
            return ScriptResult {
                success: false,
                parse_error,
                ..ScriptResult::default()
            };
        }
    };
    let mut output = ScriptResult {
        success: true,
        statement_count: statements.len(),
        results: Vec::with_capacity(statements.len()),
        ..ScriptResult::default()
    };
    let mut active = true;

    for (statement_index, sql) in statements.iter().enumerate() {
        if !active {
            break;
        }
        let cancellation_error = match poll_cancellation(&options.should_cancel) {
            Ok(false) => None,
            Ok(true) => Some("query cancelled"),
            Err(message) => Some(message),
        };
        if let Some(error) = cancellation_error {
            let _ = sink(&format!(
                "{}\n",
                json!({"statement_index": statement_index, "error": error})
            ));
            output.success = false;
            output.first_error_index = Some(statement_index);
            output.results.push(StatementResult {
                statement_index,
                success: false,
                error: Some(error.to_string()),
                sql: if options.include_sql {
                    sql.clone()
                } else {
                    String::new()
                },
                ..StatementResult::default()
            });
            break;
        }

        let mut keys: Option<Vec<String>> = None;
        let mut row_count = 0usize;
        let query_options = crate::database::QueryOptions {
            timeout_ms: options.timeout_ms,
            should_cancel: options.should_cancel.clone(),
            ..crate::database::QueryOptions::default()
        };
        let outcome = db.stream_query_with_options(sql, query_options, |columns, row| {
            let keys = keys.get_or_insert_with(|| {
                let mut used = HashSet::new();
                columns
                    .iter()
                    .map(|column| {
                        let mut candidate = column.clone();
                        let mut suffix = 2usize;
                        while used.contains(&candidate) {
                            candidate = format!("{column}#{suffix}");
                            suffix += 1;
                        }
                        used.insert(candidate.clone());
                        candidate
                    })
                    .collect()
            });
            let mut object = Map::new();
            for (index, key) in keys.iter().enumerate() {
                let value = if row.nulls.get(index).copied().unwrap_or(true) {
                    Value::Null
                } else {
                    Value::String(row.values.get(index).cloned().unwrap_or_default())
                };
                object.insert(key.clone(), value);
            }
            row_count += 1;
            active = sink(&format!("{}\n", Value::Object(object)));
            active
        });
        if !active {
            break;
        }
        let success = outcome.error.is_none();
        if let Some(error) = outcome.error.as_ref() {
            active = sink(&format!(
                "{}\n",
                json!({"statement_index": statement_index, "error": error})
            ));
        } else if outcome.timed_out || outcome.partial || !outcome.warnings.is_empty() {
            let mut metadata = Map::new();
            metadata.insert("statement_index".into(), json!(statement_index));
            if outcome.timed_out {
                metadata.insert("timed_out".into(), Value::Bool(true));
            }
            if outcome.partial {
                metadata.insert("partial".into(), Value::Bool(true));
            }
            if !outcome.warnings.is_empty() {
                metadata.insert("warnings".into(), json!(outcome.warnings));
            }
            active = sink(&format!("{}\n", Value::Object(metadata)));
        }

        let statement = StatementResult {
            statement_index,
            success,
            columns: outcome.result.columns,
            rows: Vec::new(),
            row_count,
            elapsed_ms: outcome.elapsed_ms as f64,
            error: outcome.error,
            sql: if options.include_sql {
                sql.clone()
            } else {
                String::new()
            },
            timed_out: outcome.timed_out,
            partial: outcome.partial,
            warnings: outcome.warnings,
        };
        if !success {
            output.success = false;
            output.first_error_index.get_or_insert(statement_index);
        }
        output.row_count_total += row_count;
        output.elapsed_ms_total += statement.elapsed_ms;
        output.results.push(statement);
        if !active || (!success && !options.continue_on_error) {
            break;
        }
    }
    output
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
    if statement.columns.is_empty() {
        return "(no result)\n".to_string();
    }
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
            if statement.timed_out {
                object.insert("timed_out".to_string(), serde_json::Value::Bool(true));
            }
            if statement.partial {
                object.insert("partial".to_string(), serde_json::Value::Bool(true));
            }
            if !statement.warnings.is_empty() {
                object.insert("warnings".to_string(), json!(statement.warnings));
            }
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
        timed_out: object
            .get("timed_out")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        partial: object
            .get("partial")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        warnings: object
            .get("warnings")
            .and_then(serde_json::Value::as_array)
            .map(|warnings| {
                warnings
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
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
    let rendered_rows = statement
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| value.as_deref().unwrap_or("NULL").to_string())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    // Match the C++ core: render_statement_table calls print_table with default
    // options (newline_after_no_result = false), so an empty result renders
    // "(no result)" with no trailing newline. The surrounding join logic (header
    // line + inter-statement blank line) is identical on both sides.
    table_printer::print_table(
        &statement.columns,
        &rendered_rows,
        TablePrintOptions::default(),
    )
}

fn sqlite_complete(sql: &str) -> Result<bool> {
    let sql = CString::new(sql)?;
    Ok(unsafe { ffi::sqlite3_complete(sql.as_ptr()) != 0 })
}
