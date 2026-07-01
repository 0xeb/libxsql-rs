// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::aggregate::AggregateContext;
use crate::error::{Error, Result, sqlite_ok};
use crate::function::{FunctionArg, FunctionContext, build_args, sqlite_error};
use crate::script::{ScriptResult, StatementResult};
use crate::statement::{ConnectionInner, Statement, StepResult, open_connection};
use crate::value::ValueType;
use crate::vtab::{CachedTableDef, GeneratorTableDef, TableDef};
use libsqlite3_sys as ffi;
use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// One SQLite result row represented as display-ready cell strings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// Display-ready cell strings, one per result column.
    pub values: Vec<String>,
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryOptions {
    /// Query deadline in milliseconds; `0` disables the timeout.
    pub timeout_ms: u64,
    /// SQLite VM steps between timeout checks; values `<= 0` default to 1000.
    pub progress_steps: i32,
}

/// Query result plus timeout, warning, and error metadata.
#[derive(Clone, Debug)]
pub struct QueryOutcome {
    /// The rows and columns gathered before the query finished or stopped.
    pub result: QueryResult,
    /// Error message if the query failed; `None` on success or partial timeout.
    pub error: Option<String>,
    /// Non-fatal warnings, such as the partial-rows timeout notice.
    pub warnings: Vec<String>,
    /// `true` if the query hit its deadline.
    pub timed_out: bool,
    /// `true` if a row-producing query timed out and `result` holds partial rows.
    pub partial: bool,
    /// Wall-clock time spent executing the query, in milliseconds.
    pub elapsed_ms: u64,
}

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
    interrupted: Rc<Cell<bool>>,
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
            interrupted: Rc::new(Cell::new(false)),
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
        crate::vtab::register_index_table(self.raw()?, def.name(), def)
    }

    /// Registers `def` as a virtual-table module under `module_name`.
    pub fn register_table_as(&mut self, module_name: &str, def: &TableDef) -> Result<()> {
        crate::vtab::register_index_table(self.raw()?, module_name, def)
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
        crate::vtab::register_cached_table(self.raw()?, def.name(), def)
    }

    /// Registers `def` as a cached-row virtual-table module under `module_name`.
    pub fn register_cached_table_as<Row: 'static>(
        &mut self,
        module_name: &str,
        def: &CachedTableDef<Row>,
    ) -> Result<()> {
        crate::vtab::register_cached_table(self.raw()?, module_name, def)
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
        crate::vtab::register_generator_table(self.raw()?, def.name(), def)
    }

    /// Registers `def` as a generator virtual-table module under `module_name`.
    pub fn register_generator_table_as<Row: 'static>(
        &mut self,
        module_name: &str,
        def: &GeneratorTableDef<Row>,
    ) -> Result<()> {
        crate::vtab::register_generator_table(self.raw()?, module_name, def)
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
    /// When a deadline is set and reached: a row-producing query returns the
    /// rows gathered so far with `partial`/`timed_out` set and a warning (no
    /// error), while a query with no result columns returns a timeout error.
    pub fn query_with_options(&mut self, sql: &str, options: QueryOptions) -> QueryOutcome {
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

        let mut stmt = match self.prepare(sql) {
            Ok(stmt) => stmt,
            Err(err) => {
                let message = err.to_string();
                self.last_error = message.clone();
                outcome.error = Some(message);
                return outcome;
            }
        };

        let started = Instant::now();
        let timeout_enabled = options.timeout_ms > 0;
        let mut timeout_state = TimeoutState {
            started,
            timeout: Duration::from_millis(options.timeout_ms),
            timed_out: false,
        };

        if timeout_enabled {
            let progress_steps = if options.progress_steps > 0 {
                options.progress_steps
            } else {
                1000
            };
            unsafe {
                ffi::sqlite3_progress_handler(
                    db,
                    progress_steps,
                    Some(progress_callback),
                    (&mut timeout_state as *mut TimeoutState).cast(),
                );
            }
            let deadline = started + timeout_state.timeout;
            crate::vtab::set_vtab_interrupt_checker(move || Instant::now() >= deadline);
            self.interrupted.set(false);
        }

        let col_count = stmt.column_count();
        for i in 0..col_count {
            outcome.result.columns.push(stmt.column_name(i));
        }

        loop {
            match stmt.step() {
                StepResult::Row => {
                    let mut row = Row {
                        values: Vec::with_capacity(col_count as usize),
                    };
                    for col in 0..col_count {
                        row.values.push(if stmt.column_is_null(col) {
                            String::new()
                        } else {
                            stmt.text(col)
                        });
                    }
                    outcome.result.rows.push(row);
                }
                StepResult::Done => break,
                StepResult::Busy | StepResult::Error => {
                    if timeout_enabled && started.elapsed() >= timeout_state.timeout {
                        outcome.timed_out = true;
                        if col_count > 0 {
                            outcome.partial = true;
                            outcome
                                .warnings
                                .push("query timed out; returning partial rows".to_string());
                        } else {
                            outcome.error = Some("Query timed out".to_string());
                        }
                    } else {
                        let message = if stmt.error().is_empty() {
                            sqlite_error(db)
                        } else {
                            stmt.error().to_string()
                        };
                        self.last_error = message.clone();
                        outcome.error = Some(message);
                    }
                    break;
                }
            }
        }

        outcome.elapsed_ms = started.elapsed().as_millis() as u64;
        if timeout_enabled {
            unsafe {
                ffi::sqlite3_progress_handler(db, 0, None, std::ptr::null_mut());
            }
            crate::vtab::clear_vtab_interrupt_checker();
        }

        if timeout_enabled && timeout_state.timed_out {
            outcome.timed_out = true;
            if col_count > 0 {
                outcome.partial = true;
                if outcome.warnings.is_empty() {
                    outcome
                        .warnings
                        .push("query timed out; returning partial rows".to_string());
                }
                outcome.error = None;
            } else if outcome.error.is_none() {
                outcome.error = Some("Query timed out".to_string());
            }
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
        let mut err = std::ptr::null_mut();
        let rc =
            unsafe { ffi::sqlite3_exec(db, sql.as_ptr(), None, std::ptr::null_mut(), &mut err) };
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

struct TimeoutState {
    started: Instant,
    timeout: Duration,
    timed_out: bool,
}

extern "C" fn progress_callback(user_data: *mut std::ffi::c_void) -> i32 {
    if user_data.is_null() {
        return 0;
    }
    let state = unsafe { &mut *user_data.cast::<TimeoutState>() };
    if state.started.elapsed() >= state.timeout {
        state.timed_out = true;
        1
    } else {
        0
    }
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
