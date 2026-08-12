// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::aggregate::AggregateContext;
use crate::error::{Error, Result, sqlite_ok};
use crate::function::{FunctionArg, FunctionContext, build_args, sqlite_error};
use crate::script::{ScriptResult, StatementResult};
use crate::statement::{ConnectionInner, Statement, StepResult, open_connection};
use crate::value::ValueType;
use crate::vtab::{CachedTableDef, GeneratorTableDef, TableDef};
use libsqlite3_sys as ffi;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

static NEXT_QUERY_SAVEPOINT: AtomicU64 = AtomicU64::new(1);

/// One SQLite result row represented as display-ready cell strings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// Display-ready cell strings, one per result column.
    pub values: Vec<String>,
    /// Per-cell SQL-NULL flags, parallel to `values` (`true` => the cell was SQL
    /// NULL; its `values` entry is then an empty placeholder). May be empty for
    /// rows built without null tracking, in which case [`Row::is_null`] returns
    /// `false`. Lets a genuine text value (even "" or "NULL") stay distinct from a
    /// real SQL NULL — mirroring the C++ `xsql::Row` null bitmap.
    pub nulls: Vec<bool>,
}

impl Row {
    /// Returns the number of cells in the row.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` if the row has no cells.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns `true` if cell `i` was a SQL NULL (vs a genuine text value).
    pub fn is_null(&self, i: usize) -> bool {
        self.nulls.get(i).copied().unwrap_or(false)
    }
}

impl std::ops::Index<usize> for Row {
    type Output = str;

    fn index(&self, index: usize) -> &Self::Output {
        &self.values[index]
    }
}

/// Complete row result for a SQL statement with result columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryResult {
    /// Result column names, in column order.
    pub columns: Vec<String>,
    /// Result rows, in the order produced by the query.
    pub rows: Vec<Row>,
}

impl QueryResult {
    /// Returns the number of rows in the result.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Returns `true` if the result contains no rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Returns an iterator over the result rows (mirrors C++ `Result::begin/end`).
    pub fn iter(&self) -> std::slice::Iter<'_, Row> {
        self.rows.iter()
    }
}

impl std::ops::Index<usize> for QueryResult {
    type Output = Row;

    fn index(&self, index: usize) -> &Self::Output {
        &self.rows[index]
    }
}

impl<'a> IntoIterator for &'a QueryResult {
    type Item = &'a Row;
    type IntoIter = std::slice::Iter<'a, Row>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

/// Deadline and polling controls for `query_with_options`.
///
/// Carries an optional [`should_cancel`](QueryOptions::should_cancel) closure, so
/// unlike the other option structs it is [`Clone`] but not `Copy`/`PartialEq`.
#[derive(Clone, Default)]
pub struct QueryOptions {
    /// Query deadline in milliseconds; `0` disables the timeout.
    pub timeout_ms: u64,
    /// SQLite VM steps between timeout checks; values `<= 0` default to 1000.
    pub progress_steps: i32,
    /// Optional cooperative cancellation predicate (see
    /// [`ScriptOptions::should_cancel`](crate::script::ScriptOptions::should_cancel)).
    /// When set and it returns `true`, the query stops ASAP. A read-only row
    /// query keeps rows gathered so far (`partial` + a "query cancelled"
    /// warning, but **not** `timed_out`). A mutation, including one with a
    /// `RETURNING` clause, reports a hard "Query cancelled" error and rolls back
    /// the incomplete statement. Works even when `timeout_ms == 0`, so a server
    /// can bound an otherwise-unlimited query. `None` => never cancelled.
    pub should_cancel: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl std::fmt::Debug for QueryOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryOptions")
            .field("timeout_ms", &self.timeout_ms)
            .field("progress_steps", &self.progress_steps)
            .field(
                "should_cancel",
                &self.should_cancel.as_ref().map(|_| "<fn>"),
            )
            .finish()
    }
}

/// Query result plus timeout, warning, and error metadata.
#[derive(Clone, Debug)]
pub struct QueryOutcome {
    /// The rows and columns gathered before the query finished or stopped.
    pub result: QueryResult,
    /// Error message if the query failed; `None` on success or a read-only
    /// partial timeout/cancellation.
    pub error: Option<String>,
    /// Non-fatal warnings, such as the partial-rows timeout notice.
    pub warnings: Vec<String>,
    /// `true` if the query hit its deadline.
    pub timed_out: bool,
    /// `true` if a read-only row query stopped after producing rows (or columns)
    /// and `result` holds the partial result.
    pub partial: bool,
    /// Wall-clock time spent executing the query, in milliseconds.
    pub elapsed_ms: u64,
}

type RowSink<'a> = dyn FnMut(&[String], &Row) -> bool + 'a;

impl QueryOutcome {
    /// Returns `true` if the query produced no error.
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }

    /// Returns an iterator over the gathered result rows.
    pub fn iter(&self) -> std::slice::Iter<'_, Row> {
        self.result.rows.iter()
    }
}

impl<'a> IntoIterator for &'a QueryOutcome {
    type Item = &'a Row;
    type IntoIter = std::slice::Iter<'a, Row>;

    fn into_iter(self) -> Self::IntoIter {
        self.result.rows.iter()
    }
}

/// Script execution mode for multi-statement SQL input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptExecutionMode {
    /// Stop at the first statement that errors.
    FailFast,
    /// Keep executing remaining statements after an error.
    ContinueOnError,
}

/// Owned SQLite database connection with xsql registration helpers.
pub struct Database {
    inner: Option<Rc<ConnectionInner>>,
    last_error: String,
}

impl Database {
    /// Opens a new in-memory database (alias for [`Database::open_memory`]).
    pub fn new() -> Result<Self> {
        Self::open_memory()
    }

    /// Opens the database at `path`, registering built-in aggregates.
    pub fn open(path: impl AsRef<str>) -> Result<Self> {
        let inner = open_connection(path.as_ref())?;
        let mut db = Self {
            inner: Some(inner),
            last_error: String::new(),
        };
        db.register_builtin_aggregates()?;
        Ok(db)
    }

    /// Opens a transient in-memory database (`:memory:`).
    pub fn open_memory() -> Result<Self> {
        Self::open(":memory:")
    }

    /// Closes the connection, dropping the handle and clearing the last error.
    pub fn close(&mut self) {
        self.inner = None;
        self.last_error.clear();
    }

    /// Closes the current connection and opens a new one at `path`.
    ///
    /// On failure the database is left closed and the error is recorded as the
    /// last error.
    pub fn reopen(&mut self, path: impl AsRef<str>) -> Result<()> {
        self.close();
        match open_connection(path.as_ref()) {
            Ok(inner) => {
                self.inner = Some(inner);
                self.register_builtin_aggregates()?;
                self.last_error.clear();
                Ok(())
            }
            Err(err) => {
                self.last_error = err.to_string();
                Err(err)
            }
        }
    }

    /// Returns `true` if the connection is open and its handle is non-null.
    pub fn is_open(&self) -> bool {
        self.inner
            .as_ref()
            .map(|inner| !inner.raw().is_null())
            .unwrap_or(false)
    }

    /// Compiles `sql` into a prepared [`Statement`] on this connection.
    pub fn prepare(&self, sql: &str) -> Result<Statement> {
        Statement::prepare(self.connection()?, sql)
    }

    /// Compiles `sql` into a prepared [`Statement`] (alias for [`Database::prepare`]).
    pub fn prepare_statement(&self, sql: &str) -> Result<Statement> {
        self.prepare(sql)
    }

    /// Returns `true` if `sql` compiles to a read-only statement.
    ///
    /// Returns `false` if the statement cannot be prepared.
    pub fn is_readonly_statement(&self, sql: &str) -> bool {
        self.prepare(sql)
            .map(|stmt| stmt.is_readonly())
            .unwrap_or(false)
    }

    /// Registers `def` as a virtual-table module named after the table.
    pub fn register_table(&mut self, def: &TableDef) -> Result<()> {
        self.register_table_as(def.name(), def)
    }

    /// Registers `def` as a virtual-table module under `module_name`.
    pub fn register_table_as(&mut self, module_name: &str, def: &TableDef) -> Result<()> {
        let connection = self.connection()?;
        crate::vtab::register_index_table(
            connection.raw(),
            connection.write_surface_registry(),
            module_name,
            def,
        )
    }

    /// Creates a virtual table `table_name` backed by the module `module_name`.
    pub fn create_table(&mut self, table_name: &str, module_name: &str) -> Result<()> {
        let sql = format!(
            "CREATE VIRTUAL TABLE {} USING {}",
            quote_identifier(table_name),
            quote_identifier(module_name)
        );
        self.exec(&sql)
    }

    /// Registers `def` and creates a virtual table of the same name.
    pub fn register_and_create_table(&mut self, def: &TableDef) -> Result<()> {
        self.register_table(def)?;
        self.create_table(def.name(), def.name())
    }

    /// Registers `def` and creates a virtual table named `table_name` from it.
    pub fn register_and_create_table_as(&mut self, def: &TableDef, table_name: &str) -> Result<()> {
        self.register_table(def)?;
        self.create_table(table_name, def.name())
    }

    /// Registers and creates a virtual table for each definition in `defs`.
    pub fn register_and_create_tables<'a, I>(&mut self, defs: I) -> Result<()>
    where
        I: IntoIterator<Item = &'a TableDef>,
    {
        for def in defs {
            self.register_and_create_table(def)?;
        }
        Ok(())
    }

    /// Registers `def` as a cached-row virtual-table module named after the table.
    pub fn register_cached_table<Row: 'static>(&mut self, def: &CachedTableDef<Row>) -> Result<()> {
        self.register_cached_table_as(def.name(), def)
    }

    /// Registers `def` as a cached-row virtual-table module under `module_name`.
    pub fn register_cached_table_as<Row: 'static>(
        &mut self,
        module_name: &str,
        def: &CachedTableDef<Row>,
    ) -> Result<()> {
        let connection = self.connection()?;
        crate::vtab::register_cached_table(
            connection.raw(),
            connection.write_surface_registry(),
            module_name,
            def,
        )
    }

    /// Registers a cached-row `def` and creates a virtual table of the same name.
    pub fn register_and_create_cached_table<Row: 'static>(
        &mut self,
        def: &CachedTableDef<Row>,
    ) -> Result<()> {
        self.register_cached_table(def)?;
        self.create_table(def.name(), def.name())
    }

    /// Registers a cached-row `def` and creates a virtual table named `table_name`.
    pub fn register_and_create_cached_table_as<Row: 'static>(
        &mut self,
        def: &CachedTableDef<Row>,
        table_name: &str,
    ) -> Result<()> {
        self.register_cached_table(def)?;
        self.create_table(table_name, def.name())
    }

    /// Registers `def` as a generator virtual-table module named after the table.
    pub fn register_generator_table<Row: 'static>(
        &mut self,
        def: &GeneratorTableDef<Row>,
    ) -> Result<()> {
        self.register_generator_table_as(def.name(), def)
    }

    /// Registers `def` as a generator virtual-table module under `module_name`.
    pub fn register_generator_table_as<Row: 'static>(
        &mut self,
        module_name: &str,
        def: &GeneratorTableDef<Row>,
    ) -> Result<()> {
        let connection = self.connection()?;
        crate::vtab::register_generator_table(
            connection.raw(),
            connection.write_surface_registry(),
            module_name,
            def,
        )
    }

    /// Registers a generator `def` and creates a virtual table of the same name.
    pub fn register_and_create_generator_table<Row: 'static>(
        &mut self,
        def: &GeneratorTableDef<Row>,
    ) -> Result<()> {
        self.register_generator_table(def)?;
        self.create_table(def.name(), def.name())
    }

    /// Registers a generator `def` and creates a virtual table named `table_name`.
    pub fn register_and_create_generator_table_as<Row: 'static>(
        &mut self,
        def: &GeneratorTableDef<Row>,
        table_name: &str,
    ) -> Result<()> {
        self.register_generator_table(def)?;
        self.create_table(table_name, def.name())
    }

    /// Runs `sql` and returns its result rows, or an error on failure.
    ///
    /// Uses default [`QueryOptions`] (no timeout). For timeout and partial-row
    /// handling use [`Database::query_with_options`].
    pub fn query(&mut self, sql: &str) -> Result<QueryResult> {
        let outcome = self.query_with_options(sql, QueryOptions::default());
        if let Some(error) = outcome.error {
            Err(Error::Message(error))
        } else {
            Ok(outcome.result)
        }
    }

    /// Runs `sql` under the given `options`, returning a [`QueryOutcome`].
    ///
    /// When a deadline is set and reached: a read-only row query returns the
    /// rows gathered so far with `partial`/`timed_out` set and a warning (no
    /// error), while a mutation (including `RETURNING`) reports a timeout error
    /// and rolls back the incomplete statement.
    pub fn query_with_options(&mut self, sql: &str, options: QueryOptions) -> QueryOutcome {
        self.query_with_options_impl(sql, options, None, true)
    }

    /// Step a query while handing each row to `sink` instead of retaining it.
    ///
    /// The returned outcome preserves columns, errors, timeout/cancellation
    /// metadata, and elapsed time, but its row vector stays empty. Returning
    /// `false` from `sink` stops stepping, which lets a transport bound memory
    /// and stop promptly after a disconnected client. Read-only rows reach the
    /// sink incrementally. Mutation `RETURNING` rows are buffered until the
    /// statement succeeds so rolled-back provisional rows never reach it.
    pub fn stream_query_with_options<F>(
        &mut self,
        sql: &str,
        options: QueryOptions,
        mut sink: F,
    ) -> QueryOutcome
    where
        F: FnMut(&[String], &Row) -> bool,
    {
        self.query_with_options_impl(sql, options, Some(&mut sink), false)
    }

    fn query_with_options_impl(
        &mut self,
        sql: &str,
        options: QueryOptions,
        mut sink: Option<&mut RowSink<'_>>,
        collect_rows: bool,
    ) -> QueryOutcome {
        let mut outcome = QueryOutcome {
            result: QueryResult::default(),
            error: None,
            warnings: Vec::new(),
            timed_out: false,
            partial: false,
            elapsed_ms: 0,
        };
        let db = match self.raw() {
            Ok(db) => db,
            Err(err) => {
                let message = err.to_string();
                self.last_error = message.clone();
                outcome.error = Some(message);
                return outcome;
            }
        };

        let started = Instant::now();
        let timeout_enabled = options.timeout_ms > 0;
        let cancel_enabled = options.should_cancel.is_some();
        // The progress-handler + interrupt-checker machinery is needed whenever
        // EITHER a deadline OR a cancel predicate is in play (cancel must work under
        // timeout_ms == 0). Mirrors the C++ `guard_enabled`.
        let guard_enabled = timeout_enabled || cancel_enabled;
        let cancellation = options
            .should_cancel
            .clone()
            .map(CancellationState::new)
            .map(Arc::new);
        let mut timeout_state = Box::new(TimeoutState {
            started,
            timeout: Duration::from_millis(options.timeout_ms),
            timeout_enabled,
            timed_out: false,
            cancellation: cancellation.clone(),
        });
        let progress_steps = if options.progress_steps > 0 {
            options.progress_steps
        } else {
            1000
        };
        // Install both guards before prepare so xConnect/xBestIndex work is part
        // of the operation deadline. The guards restore any outer libxsql query.
        let deadline = started + timeout_state.timeout;
        let has_deadline = timeout_enabled;
        let checker_cancellation = cancellation.clone();
        let _interrupt_guard = QueryInterruptGuard::install(
            db,
            guard_enabled,
            progress_steps,
            (&mut *timeout_state as *mut TimeoutState).cast(),
            Box::new(move || {
                (has_deadline && Instant::now() >= deadline)
                    || checker_cancellation
                        .as_ref()
                        .is_some_and(|cancellation| cancellation.poll())
            }),
        );

        let mut stmt = match self.prepare(sql) {
            Ok(stmt) => stmt,
            Err(err) => {
                if timeout_enabled && started.elapsed() >= timeout_state.timeout {
                    timeout_state.timed_out = true;
                }
                if !classify_query_interrupt(&mut outcome, &timeout_state, false) {
                    outcome.error = Some(err.to_string());
                }
                outcome.elapsed_ms = started.elapsed().as_millis() as u64;
                if let Some(error) = outcome.error.as_ref() {
                    self.last_error.clone_from(error);
                }
                return outcome;
            }
        };

        let col_count = stmt.column_count();
        let readonly = stmt.is_readonly();
        let partial_capable = col_count > 0 && readonly;
        for i in 0..col_count {
            outcome.result.columns.push(stmt.column_name(i));
        }

        // SQLite computes all DML RETURNING changes before yielding its first
        // row. Finalizing a statement after that row commits those changes, so a
        // cancellation observed between RETURNING rows would otherwise report an
        // error after committing. A private savepoint makes that early stop
        // rollback only this statement (and preserves any caller transaction).
        let returning_savepoint =
            (guard_enabled && col_count > 0 && !readonly && has_sql_keyword(sql, "returning"))
                .then(|| {
                    format!(
                        "__libxsql_query_{}",
                        NEXT_QUERY_SAVEPOINT.fetch_add(1, Ordering::Relaxed)
                    )
                });
        if let Some(name) = returning_savepoint.as_ref()
            && let Err(message) = exec_internal_sql(db, &format!("SAVEPOINT \"{name}\""))
        {
            if timeout_enabled && started.elapsed() >= timeout_state.timeout {
                timeout_state.timed_out = true;
            }
            if !classify_query_interrupt(&mut outcome, &timeout_state, false) {
                outcome.error = Some(message);
            }
            outcome.elapsed_ms = started.elapsed().as_millis() as u64;
            if let Some(error) = outcome.error.as_ref() {
                self.last_error.clone_from(error);
            }
            return outcome;
        }

        // A mutation with RETURNING is not safe to expose incrementally: SQLite
        // may still roll the statement back on a later step, timeout, or
        // cancellation. Buffer only the streaming sink's mutation rows and
        // release them after SQLITE_DONE establishes successful completion.
        let mut deferred_sink_rows = Vec::new();
        let mut completed = false;
        // Whether at least one row has been delivered to the caller (collected or
        // streamed through the sink). The zero-row cancel/timeout error contract
        // keys on delivery, not on `outcome.result.rows`, because the streaming
        // script path delivers rows via the sink with `collect_rows == false`.
        let mut delivered_row = false;
        loop {
            // SQLite may execute a short mutation without invoking its progress
            // callback. Poll before every step so an already-cancelled request
            // cannot mutate first and only be noticed afterward.
            if guard_enabled && poll_timeout_state(&mut timeout_state) {
                let classified = classify_query_interrupt(
                    &mut outcome,
                    &timeout_state,
                    partial_capable && delivered_row,
                );
                debug_assert!(classified);
                break;
            }
            match stmt.step() {
                StepResult::Row => {
                    let mut row = Row {
                        values: Vec::with_capacity(col_count as usize),
                        nulls: Vec::with_capacity(col_count as usize),
                    };
                    for col in 0..col_count {
                        let is_null = stmt.column_is_null(col);
                        row.values.push(if is_null {
                            String::new()
                        } else {
                            stmt.text(col)
                        });
                        row.nulls.push(is_null);
                    }
                    if sink.is_some() && !partial_capable {
                        deferred_sink_rows.push(row);
                    } else {
                        delivered_row = true;
                        if let Some(sink) = sink.as_mut()
                            && !sink(&outcome.result.columns, &row)
                        {
                            break;
                        }
                        if collect_rows {
                            outcome.result.rows.push(row);
                        }
                    }
                }
                StepResult::Done => {
                    completed = true;
                    break;
                }
                StepResult::Busy | StepResult::Error => {
                    // Classify the stop by cause — cancel first (it takes precedence
                    // and is not a timeout), then a deadline, then a genuine
                    // statement error. The vtable checker cannot borrow the stack
                    // timeout state, so latch its elapsed deadline only on this
                    // non-DONE path. Never freshly poll cancellation after a step.
                    if timeout_enabled && started.elapsed() >= timeout_state.timeout {
                        timeout_state.timed_out = true;
                    }
                    if !classify_query_interrupt(
                        &mut outcome,
                        &timeout_state,
                        partial_capable && delivered_row,
                    ) {
                        let message = if stmt.error().is_empty() {
                            sqlite_error(db)
                        } else {
                            stmt.error().to_string()
                        };
                        if partial_capable && delivered_row {
                            outcome.partial = true;
                            outcome
                                .warnings
                                .push("query failed; returning partial rows".to_string());
                        } else {
                            outcome.result.rows.clear();
                        }
                        self.last_error = message.clone();
                        outcome.error = Some(message);
                    }
                    break;
                }
            }
        }

        // Finalize the DML before ending its savepoint. On an interrupted
        // RETURNING statement, rollback while the savepoint still scopes the
        // mutation; on success, RELEASE commits it. Never emit deferred rows
        // until RELEASE succeeds.
        drop(stmt);
        drop(_interrupt_guard);
        if let Some(name) = returning_savepoint.as_ref() {
            if completed && outcome.error.is_none() {
                if let Err(message) = exec_internal_sql(db, &format!("RELEASE \"{name}\"")) {
                    outcome.result.rows.clear();
                    deferred_sink_rows.clear();
                    outcome.partial = false;
                    outcome.warnings.clear();
                    outcome.error = Some(message);
                    completed = false;
                }
            } else {
                let rollback_error =
                    exec_internal_sql(db, &format!("ROLLBACK TO \"{name}\"")).err();
                let release_error = exec_internal_sql(db, &format!("RELEASE \"{name}\"")).err();
                if let Some(message) = rollback_error.or(release_error) {
                    let original = outcome.error.take().unwrap_or_default();
                    outcome.error = Some(if original.is_empty() {
                        message
                    } else {
                        format!("{original}; failed to roll back interrupted statement: {message}")
                    });
                }
            }
        }

        if completed && outcome.error.is_none() && !deferred_sink_rows.is_empty() {
            for row in &deferred_sink_rows {
                if let Some(sink) = sink.as_mut()
                    && !sink(&outcome.result.columns, row)
                {
                    break;
                }
            }
            if collect_rows {
                for row in deferred_sink_rows {
                    outcome.result.rows.push(row);
                }
            }
        }

        outcome.elapsed_ms = started.elapsed().as_millis() as u64;
        if let Some(error) = outcome.error.as_ref() {
            self.last_error.clone_from(error);
        }

        outcome
    }

    /// Runs `sql` and returns the first column of the first row as text.
    ///
    /// Returns `Ok(None)` when there are no rows or the value is NULL.
    pub fn scalar(&mut self, sql: &str) -> Result<Option<String>> {
        let mut stmt = self.prepare(sql)?;
        match stmt.step() {
            StepResult::Row if !stmt.column_is_null(0) => Ok(Some(stmt.text(0))),
            StepResult::Row | StepResult::Done => Ok(None),
            StepResult::Busy | StepResult::Error => {
                let message = if stmt.error().is_empty() {
                    match self.raw() {
                        Ok(db) => sqlite_error(db),
                        Err(err) => err.to_string(),
                    }
                } else {
                    stmt.error().to_string()
                };
                self.last_error = message.clone();
                Err(Error::Message(message))
            }
        }
    }

    /// Executes a multi-statement `script`, returning a result per statement.
    pub fn execute_script(&mut self, script: &str) -> Result<Vec<StatementResult>> {
        crate::script::execute_script(self, script)
    }

    /// Runs a multi-statement `script` under the given execution `mode`.
    pub fn run_script(&mut self, script: &str, mode: ScriptExecutionMode) -> Result<ScriptResult> {
        crate::script::run_script(self, script, mode)
    }

    /// Exports the named `tables` to a new SQLite database at `output_path`.
    pub fn export_tables(
        &mut self,
        tables: &[impl AsRef<str>],
        output_path: impl AsRef<std::path::Path>,
    ) -> Result<()> {
        crate::export::export_tables(self, tables, output_path)
    }

    /// Executes one or more SQL statements, discarding any result rows.
    pub fn exec(&mut self, sql: &str) -> Result<()> {
        let db = self.raw()?;
        let sql = CString::new(sql)?;
        // sqlite3_exec runs its own prepare internally, so clear the denial slot
        // first and, on an authorizer denial, surface the capability-scoped
        // message rather than SQLite's fixed "not authorized" (mirrors C++/C).
        crate::vtab::clear_authorizer_denial();
        let mut err = std::ptr::null_mut();
        let rc =
            unsafe { ffi::sqlite3_exec(db, sql.as_ptr(), None, std::ptr::null_mut(), &mut err) };
        if rc == ffi::SQLITE_AUTH
            && let Some(denial) = crate::vtab::take_authorizer_denial()
        {
            if !err.is_null() {
                unsafe { ffi::sqlite3_free(err.cast()) };
            }
            self.last_error = denial.clone();
            return Err(Error::sqlite(rc, denial));
        }
        if !err.is_null() {
            let message = unsafe { std::ffi::CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            unsafe { ffi::sqlite3_free(err.cast()) };
            self.last_error = message.clone();
            return Err(Error::sqlite(rc, message));
        }
        if !sqlite_ok(rc) {
            let message = sqlite_error(db);
            self.last_error = message.clone();
            return Err(Error::sqlite(rc, message));
        }
        self.last_error.clear();
        Ok(())
    }

    /// Executes `sql`, invoking `callback` once per result row.
    ///
    /// The callback receives the cell values (`None` for NULL) and column names;
    /// returning a non-zero value aborts execution. Returns the SQLite result code.
    pub fn exec_with_callback<F>(&mut self, sql: &str, callback: F) -> Result<i32>
    where
        F: FnMut(&[Option<&str>], &[&str]) -> i32,
    {
        let db = self.raw()?;
        let sql = CString::new(sql)?;
        let mut callback = callback;
        let mut data = ExecCallbackData {
            callback: (&mut callback as *mut F).cast(),
            invoke: invoke_exec_callback::<F>,
        };
        // See `exec`: clear the denial slot and, on a denial, surface the
        // capability-scoped message rather than "not authorized".
        crate::vtab::clear_authorizer_denial();
        let mut err = std::ptr::null_mut();
        let rc = unsafe {
            ffi::sqlite3_exec(
                db,
                sql.as_ptr(),
                Some(exec_callback_trampoline),
                (&mut data as *mut ExecCallbackData).cast(),
                &mut err,
            )
        };
        if rc == ffi::SQLITE_AUTH
            && let Some(denial) = crate::vtab::take_authorizer_denial()
        {
            if !err.is_null() {
                unsafe { ffi::sqlite3_free(err.cast()) };
            }
            self.last_error = denial.clone();
            return Err(Error::sqlite(rc, denial));
        }
        if !err.is_null() {
            let message = unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned();
            unsafe { ffi::sqlite3_free(err.cast()) };
            self.last_error = message.clone();
            return Err(Error::sqlite(rc, message));
        }
        if !sqlite_ok(rc) {
            let message = sqlite_error(db);
            self.last_error = message.clone();
            return Err(Error::sqlite(rc, message));
        }
        self.last_error.clear();
        Ok(rc)
    }

    /// Registers a deterministic scalar SQL function `name` taking `argc` arguments.
    ///
    /// Use `argc = -1` for a variadic function. Panics in `function` are caught
    /// and surfaced as a SQL error.
    pub fn register_function<F>(&mut self, name: &str, argc: i32, function: F) -> Result<()>
    where
        F: for<'a> Fn(&mut FunctionContext<'a>, &[FunctionArg<'a>]) + 'static,
    {
        let db = self.raw()?;
        let name = CString::new(name)?;
        let wrapper = Box::new(ScalarFnWrapper {
            function: Box::new(function),
        });
        let wrapper_ptr = Box::into_raw(wrapper).cast();
        let rc = unsafe {
            ffi::sqlite3_create_function_v2(
                db,
                name.as_ptr(),
                argc,
                ffi::SQLITE_UTF8 | ffi::SQLITE_DETERMINISTIC,
                wrapper_ptr,
                Some(scalar_callback),
                None,
                None,
                Some(destroy_scalar_wrapper),
            )
        };
        if sqlite_ok(rc) {
            self.last_error.clear();
            Ok(())
        } else {
            let message = sqlite_error(db);
            self.last_error = message.clone();
            Err(Error::sqlite(rc, message))
        }
    }

    /// Registers a deterministic aggregate SQL function `name` taking `argc` arguments.
    ///
    /// `step` accumulates each input row and `final_fn` produces the result. Use
    /// `argc = -1` for variadic; panics in either callback become a SQL error.
    pub fn register_aggregate<Step, Final>(
        &mut self,
        name: &str,
        argc: i32,
        step: Step,
        final_fn: Final,
    ) -> Result<()>
    where
        Step: for<'a> Fn(&mut AggregateContext<'a>, &[FunctionArg<'a>]) + 'static,
        Final: for<'a> Fn(&mut AggregateContext<'a>) + 'static,
    {
        let db = self.raw()?;
        let name = CString::new(name)?;
        let wrapper = Box::new(AggregateFnWrapper {
            step: Box::new(step),
            final_fn: Box::new(final_fn),
        });
        let wrapper_ptr = Box::into_raw(wrapper).cast();
        let rc = unsafe {
            ffi::sqlite3_create_function_v2(
                db,
                name.as_ptr(),
                argc,
                ffi::SQLITE_UTF8 | ffi::SQLITE_DETERMINISTIC,
                wrapper_ptr,
                None,
                Some(aggregate_step_callback),
                Some(aggregate_final_callback),
                Some(destroy_aggregate_wrapper),
            )
        };
        if sqlite_ok(rc) {
            self.last_error.clear();
            Ok(())
        } else {
            let message = sqlite_error(db);
            self.last_error = message.clone();
            Err(Error::sqlite(rc, message))
        }
    }

    /// Returns the most recent error message, or an empty string if none.
    pub fn last_error(&self) -> &str {
        &self.last_error
    }

    /// Returns the rowid of the most recent successful INSERT, or `0` if closed.
    pub fn last_insert_rowid(&self) -> i64 {
        match self.raw() {
            Ok(db) => unsafe { ffi::sqlite3_last_insert_rowid(db) },
            Err(_) => 0,
        }
    }

    /// Returns the number of rows changed by the most recent statement, or `0` if closed.
    pub fn changes(&self) -> i32 {
        match self.raw() {
            Ok(db) => unsafe { ffi::sqlite3_changes(db) },
            Err(_) => 0,
        }
    }

    fn register_builtin_aggregates(&mut self) -> Result<()> {
        self.register_aggregate("blob_concat", 1, blob_concat_step, blob_concat_final)
    }

    fn connection(&self) -> Result<Rc<ConnectionInner>> {
        self.inner
            .as_ref()
            .filter(|inner| !inner.raw().is_null())
            .cloned()
            .ok_or(Error::DatabaseNotOpen)
    }

    fn raw(&self) -> Result<*mut ffi::sqlite3> {
        self.inner
            .as_ref()
            .map(|inner| inner.raw())
            .filter(|raw| !raw.is_null())
            .ok_or(Error::DatabaseNotOpen)
    }
}

fn has_sql_keyword(sql: &str, keyword: &str) -> bool {
    let bytes = sql.as_bytes();
    let keyword = keyword.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[index];
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == quote {
                        if bytes.get(index + 1) == Some(&quote) {
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    index += 1;
                }
            }
            b'[' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b']' {
                    index += 1;
                }
                index = index.saturating_add(1);
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index += 2;
                while index < bytes.len() && !matches!(bytes[index], b'\r' | b'\n') {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = index.saturating_add(2).min(bytes.len());
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                if bytes[start..index].eq_ignore_ascii_case(keyword) {
                    return true;
                }
            }
            _ => index += 1,
        }
    }
    false
}

fn exec_internal_sql(db: *mut ffi::sqlite3, sql: &str) -> std::result::Result<(), String> {
    let sql = CString::new(sql).map_err(|error| error.to_string())?;
    let mut error = std::ptr::null_mut();
    let rc = unsafe { ffi::sqlite3_exec(db, sql.as_ptr(), None, std::ptr::null_mut(), &mut error) };
    if !error.is_null() {
        let message = unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::sqlite3_free(error.cast()) };
        return Err(message);
    }
    if sqlite_ok(rc) {
        Ok(())
    } else {
        Err(sqlite_error(db))
    }
}

struct TimeoutState {
    started: Instant,
    timeout: Duration,
    timeout_enabled: bool,
    timed_out: bool,
    cancellation: Option<Arc<CancellationState>>,
}

struct CancellationState {
    predicate: Arc<dyn Fn() -> bool + Send + Sync>,
    cancelled: AtomicBool,
    panicked: AtomicBool,
}

impl CancellationState {
    fn new(predicate: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        Self {
            predicate,
            cancelled: AtomicBool::new(false),
            panicked: AtomicBool::new(false),
        }
    }

    fn poll(&self) -> bool {
        if self.panicked() || self.cancelled() {
            return true;
        }
        match catch_unwind(AssertUnwindSafe(|| (self.predicate)())) {
            Ok(cancelled) => {
                if cancelled {
                    self.cancelled.store(true, Ordering::Release);
                }
                cancelled
            }
            Err(_) => {
                self.panicked.store(true, Ordering::Release);
                true
            }
        }
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn panicked(&self) -> bool {
        self.panicked.load(Ordering::Acquire)
    }
}

// `can_be_partial` is `partial_capable && at least one row already delivered`:
// a read-only statement with a non-empty prefix keeps that prefix as a partial
// result, but a zero-row cancel/timeout is a hard error (an empty "partial" would
// read as a valid truncation of the real result). Mirrors C++ database.cpp.
fn classify_query_interrupt(
    outcome: &mut QueryOutcome,
    timeout_state: &TimeoutState,
    can_be_partial: bool,
) -> bool {
    if timeout_state
        .cancellation
        .as_ref()
        .is_some_and(|cancellation| cancellation.panicked())
    {
        outcome.result.rows.clear();
        outcome.timed_out = false;
        outcome.partial = false;
        outcome.warnings.clear();
        outcome.error = Some("cancellation predicate panicked".to_string());
        return true;
    }
    if timeout_state
        .cancellation
        .as_ref()
        .is_some_and(|cancellation| cancellation.cancelled())
    {
        outcome.timed_out = false;
        if can_be_partial {
            outcome.partial = true;
            outcome
                .warnings
                .push("query cancelled; returning partial rows".to_string());
        } else {
            outcome.result.rows.clear();
            outcome.error = Some("Query cancelled".to_string());
        }
        return true;
    }
    if timeout_state.timed_out {
        outcome.timed_out = true;
        if can_be_partial {
            outcome.partial = true;
            outcome
                .warnings
                .push("query timed out; returning partial rows".to_string());
        } else {
            outcome.result.rows.clear();
            outcome.error = Some("Query timed out".to_string());
        }
        return true;
    }
    false
}

#[derive(Clone, Copy)]
struct ProgressRegistration {
    steps: i32,
    user_data: *mut c_void,
}

thread_local! {
    static LIBXSQL_PROGRESS_HANDLERS: RefCell<HashMap<usize, ProgressRegistration>> =
        RefCell::new(HashMap::new());
}

struct ProgressHandlerGuard {
    db: *mut ffi::sqlite3,
    previous: Option<ProgressRegistration>,
}

impl ProgressHandlerGuard {
    fn install(db: *mut ffi::sqlite3, steps: i32, user_data: *mut c_void) -> Self {
        let registration = ProgressRegistration { steps, user_data };
        let previous = LIBXSQL_PROGRESS_HANDLERS
            .with(|handlers| handlers.borrow_mut().insert(db as usize, registration));
        unsafe {
            ffi::sqlite3_progress_handler(db, steps, Some(progress_callback), user_data);
        }
        Self { db, previous }
    }
}

impl Drop for ProgressHandlerGuard {
    fn drop(&mut self) {
        LIBXSQL_PROGRESS_HANDLERS.with(|handlers| {
            let mut handlers = handlers.borrow_mut();
            if let Some(previous) = self.previous {
                handlers.insert(self.db as usize, previous);
                unsafe {
                    ffi::sqlite3_progress_handler(
                        self.db,
                        previous.steps,
                        Some(progress_callback),
                        previous.user_data,
                    );
                }
            } else {
                handlers.remove(&(self.db as usize));
                unsafe {
                    ffi::sqlite3_progress_handler(self.db, 0, None, std::ptr::null_mut());
                }
            }
        });
    }
}

struct QueryInterruptGuard {
    progress: Option<ProgressHandlerGuard>,
    previous_checker: Option<crate::vtab::VtabInterruptChecker>,
}

impl QueryInterruptGuard {
    fn install(
        db: *mut ffi::sqlite3,
        enabled: bool,
        progress_steps: i32,
        user_data: *mut c_void,
        checker: crate::vtab::VtabInterruptChecker,
    ) -> Self {
        if !enabled {
            return Self {
                progress: None,
                previous_checker: None,
            };
        }
        let progress = Some(ProgressHandlerGuard::install(db, progress_steps, user_data));
        let previous_checker = crate::vtab::replace_vtab_interrupt_checker(Some(checker));
        Self {
            progress,
            previous_checker,
        }
    }
}

impl Drop for QueryInterruptGuard {
    fn drop(&mut self) {
        if self.progress.is_some() {
            let previous = self.previous_checker.take();
            let _ = crate::vtab::replace_vtab_interrupt_checker(previous);
            drop(self.progress.take());
        }
    }
}

extern "C" fn progress_callback(user_data: *mut std::ffi::c_void) -> i32 {
    if user_data.is_null() {
        return 0;
    }
    let state = unsafe { &mut *user_data.cast::<TimeoutState>() };
    i32::from(poll_timeout_state(state))
}

fn poll_timeout_state(state: &mut TimeoutState) -> bool {
    // Cancellation takes precedence over the deadline and works even when
    // timeout_ms == 0 (a server bounding an otherwise-unlimited query). Mirrors
    // the C++ ProgressHandler::callback.
    if state
        .cancellation
        .as_ref()
        .is_some_and(|cancellation| cancellation.poll())
    {
        return true;
    }
    if state.timeout_enabled && state.started.elapsed() >= state.timeout {
        state.timed_out = true;
        return true;
    }
    false
}

type ScalarCallback = dyn for<'a> Fn(&mut FunctionContext<'a>, &[FunctionArg<'a>]) + 'static;
type AggregateStepCallback =
    dyn for<'a> Fn(&mut AggregateContext<'a>, &[FunctionArg<'a>]) + 'static;
type AggregateFinalCallback = dyn for<'a> Fn(&mut AggregateContext<'a>) + 'static;

struct ScalarFnWrapper {
    function: Box<ScalarCallback>,
}

struct AggregateFnWrapper {
    step: Box<AggregateStepCallback>,
    final_fn: Box<AggregateFinalCallback>,
}

struct ExecCallbackData {
    callback: *mut c_void,
    invoke: unsafe fn(*mut c_void, &[Option<&str>], &[&str]) -> i32,
}

unsafe fn invoke_exec_callback<F>(
    callback: *mut c_void,
    values: &[Option<&str>],
    columns: &[&str],
) -> i32
where
    F: FnMut(&[Option<&str>], &[&str]) -> i32,
{
    let callback = unsafe { &mut *callback.cast::<F>() };
    callback(values, columns)
}

extern "C" fn exec_callback_trampoline(
    data: *mut c_void,
    argc: c_int,
    argv: *mut *mut c_char,
    columns: *mut *mut c_char,
) -> c_int {
    if data.is_null() {
        return 1;
    }
    let data = unsafe { &mut *data.cast::<ExecCallbackData>() };
    let argc = argc.max(0) as usize;
    let raw_values = if argc == 0 || argv.is_null() {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(argv, argc) }
    };
    let raw_columns = if argc == 0 || columns.is_null() {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(columns, argc) }
    };

    let value_strings = raw_values
        .iter()
        .map(|value| {
            if (*value).is_null() {
                None
            } else {
                Some(
                    unsafe { CStr::from_ptr(*value) }
                        .to_string_lossy()
                        .into_owned(),
                )
            }
        })
        .collect::<Vec<_>>();
    let values = value_strings
        .iter()
        .map(|value| value.as_deref())
        .collect::<Vec<_>>();
    let column_strings = raw_columns
        .iter()
        .map(|column| {
            if (*column).is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(*column) }
                    .to_string_lossy()
                    .into_owned()
            }
        })
        .collect::<Vec<_>>();
    let column_refs = column_strings
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();

    catch_unwind(AssertUnwindSafe(|| unsafe {
        (data.invoke)(data.callback, &values, &column_refs)
    }))
    .unwrap_or(1)
}

extern "C" fn scalar_callback(
    ctx: *mut ffi::sqlite3_context,
    argc: i32,
    argv: *mut *mut ffi::sqlite3_value,
) {
    let wrapper = unsafe {
        ffi::sqlite3_user_data(ctx)
            .cast::<ScalarFnWrapper>()
            .as_ref()
    };
    let Some(wrapper) = wrapper else {
        return;
    };
    let mut fctx = FunctionContext::new(ctx);
    let args = build_args(argc, argv);
    let result = catch_unwind(AssertUnwindSafe(|| {
        (wrapper.function)(&mut fctx, &args);
    }));
    if result.is_err() {
        fctx.result_error("xsql scalar function panicked");
    }
}

extern "C" fn destroy_scalar_wrapper(ptr: *mut std::ffi::c_void) {
    if !ptr.is_null() {
        unsafe {
            drop(Box::from_raw(ptr.cast::<ScalarFnWrapper>()));
        }
    }
}

extern "C" fn aggregate_step_callback(
    ctx: *mut ffi::sqlite3_context,
    argc: i32,
    argv: *mut *mut ffi::sqlite3_value,
) {
    let wrapper = unsafe {
        ffi::sqlite3_user_data(ctx)
            .cast::<AggregateFnWrapper>()
            .as_ref()
    };
    let Some(wrapper) = wrapper else {
        return;
    };
    let mut actx = AggregateContext::new(ctx);
    let args = build_args(argc, argv);
    let result = catch_unwind(AssertUnwindSafe(|| {
        (wrapper.step)(&mut actx, &args);
    }));
    if result.is_err() {
        actx.result_error("xsql aggregate step panicked");
    }
}

extern "C" fn aggregate_final_callback(ctx: *mut ffi::sqlite3_context) {
    let wrapper = unsafe {
        ffi::sqlite3_user_data(ctx)
            .cast::<AggregateFnWrapper>()
            .as_ref()
    };
    let Some(wrapper) = wrapper else {
        return;
    };
    let mut actx = AggregateContext::new(ctx);
    let result = catch_unwind(AssertUnwindSafe(|| {
        (wrapper.final_fn)(&mut actx);
    }));
    if result.is_err() {
        actx.result_error("xsql aggregate final panicked");
    }
}

extern "C" fn destroy_aggregate_wrapper(ptr: *mut std::ffi::c_void) {
    if !ptr.is_null() {
        unsafe {
            drop(Box::from_raw(ptr.cast::<AggregateFnWrapper>()));
        }
    }
}

#[derive(Default)]
struct BlobConcatState {
    bytes: Vec<u8>,
    any_input: bool,
    errored: bool,
}

fn blob_concat_step(ctx: &mut AggregateContext<'_>, args: &[FunctionArg<'_>]) {
    let Ok(state) = ctx.state_or_init(BlobConcatState::default) else {
        ctx.result_error("blob_concat: failed to allocate aggregate state");
        return;
    };
    if state.errored || args.is_empty() {
        return;
    }

    let arg = &args[0];
    match arg.value_type() {
        ValueType::Null => {}
        ValueType::Blob => {
            state.any_input = true;
            state.bytes.extend_from_slice(arg.as_blob());
        }
        ValueType::Integer => {
            state.any_input = true;
            let value = arg.as_i64();
            if !(0..=255).contains(&value) {
                state.errored = true;
                ctx.result_error("blob_concat: INTEGER input out of byte range [0, 255]");
                return;
            }
            state.bytes.push(value as u8);
        }
        _ => {
            state.errored = true;
            ctx.result_error("blob_concat: input must be BLOB, INTEGER 0-255, or NULL");
        }
    }
}

fn blob_concat_final(ctx: &mut AggregateContext<'_>) {
    let Some(state) = ctx.take_state::<BlobConcatState>() else {
        ctx.result_null();
        return;
    };
    if state.errored {
        return;
    }
    if !state.any_input {
        ctx.result_null();
    } else {
        ctx.result_blob(&state.bytes);
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
