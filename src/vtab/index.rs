// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::error::{Error, Result, sqlite_ok};
use crate::function::{FunctionArg, FunctionContext, build_args};
use libsqlite3_sys as ffi;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use super::column_helpers;
use super::ffi::{
    callback_status, finish_cursor_unit, finish_vtab_unit, quote_identifier, set_vtab_error,
    sqlite3_vtab_nochange, unsupported_delete, unsupported_insert, unsupported_update,
};
use super::{
    ColumnType, FILTER_NONE, ModifyHook, RowIterator, TransactionHooks, TransactionLifecycle,
    WriteCaps, WriteSurfaceRegistry, apply_update_columns, connect_write_surface,
    destroy_write_surface,
};

type Getter = dyn for<'a> Fn(&mut FunctionContext<'a>, usize) + 'static;
type Setter = dyn for<'a> Fn(usize, &FunctionArg<'a>) -> bool + 'static;
pub(crate) type InsertFn = dyn for<'a> Fn(&[FunctionArg<'a>]) -> bool + 'static;
type IteratorFilterFactory = dyn for<'a> Fn(&FunctionArg<'a>) -> Box<dyn RowIterator> + 'static;
type IndexFilterFactory = dyn for<'a> Fn(&FunctionArg<'a>) -> Vec<usize> + 'static;

struct ColumnDef {
    name: String,
    column_type: ColumnType,
    getter: Box<Getter>,
    setter: Option<Box<Setter>>,
}

enum FilterFactory {
    Iterator(Box<IteratorFilterFactory>),
    Indices(Box<IndexFilterFactory>),
}

struct FilterDef {
    column_index: usize,
    filter_id: c_int,
    estimated_cost: f64,
    estimated_rows: i64,
    factory: FilterFactory,
}

struct TableInner {
    name: String,
    row_count: Box<dyn Fn() -> usize + 'static>,
    estimate_rows: Option<Box<dyn Fn() -> usize + 'static>>,
    columns: Vec<ColumnDef>,
    filters: Vec<FilterDef>,
    before_modify: Option<Box<ModifyHook>>,
    transaction_hooks: TransactionHooks,
    delete_row: Option<Box<dyn Fn(usize) -> bool + 'static>>,
    insert_row: Option<Box<InsertFn>>,
}

impl TableInner {
    fn schema(&self) -> String {
        let columns = self
            .columns
            .iter()
            .map(|column| {
                format!(
                    "{} {}",
                    quote_identifier(&column.name),
                    column.column_type.sql()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("CREATE TABLE {}({columns})", quote_identifier(&self.name))
    }

    fn find_filter_by_column(&self, column_index: usize) -> Option<&FilterDef> {
        self.filters
            .iter()
            .filter(|filter| filter.column_index == column_index)
            .min_by(|a, b| a.estimated_cost.total_cmp(&b.estimated_cost))
    }

    fn find_filter_by_id(&self, filter_id: c_int) -> Option<&FilterDef> {
        self.filters
            .iter()
            .find(|filter| filter.filter_id == filter_id)
    }
}

/// Definition for an index-based virtual table.
#[derive(Clone)]
pub struct TableDef {
    inner: Rc<TableInner>,
}

impl TableDef {
    /// Returns the table's name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Compute the write-surface capabilities for the prepare-time authorizer
    /// (mirrors the C++ `record_write_surface` call for the base module):
    /// insertable/deletable iff the respective callback is present, updatable iff
    /// any column has a setter.
    pub(crate) fn write_caps(&self) -> super::WriteCaps {
        super::WriteCaps {
            insertable: self.inner.insert_row.is_some(),
            deletable: self.inner.delete_row.is_some(),
            writable_columns: self
                .inner
                .columns
                .iter()
                .filter(|column| column.setter.is_some())
                .map(|column| column.name.to_ascii_lowercase())
                .collect(),
        }
    }
}

/// Start an index-based virtual table definition.
pub fn table(name: impl Into<String>) -> TableBuilder {
    TableBuilder {
        name: name.into(),
        row_count: None,
        estimate_rows: None,
        columns: Vec::new(),
        filters: Vec::new(),
        before_modify: None,
        transaction_hooks: TransactionHooks::default(),
        delete_row: None,
        insert_row: None,
    }
}

/// Fluent builder for index-based virtual tables.
pub struct TableBuilder {
    name: String,
    row_count: Option<Box<dyn Fn() -> usize + 'static>>,
    estimate_rows: Option<Box<dyn Fn() -> usize + 'static>>,
    columns: Vec<ColumnDef>,
    filters: Vec<FilterDef>,
    before_modify: Option<Box<ModifyHook>>,
    transaction_hooks: TransactionHooks,
    delete_row: Option<Box<dyn Fn(usize) -> bool + 'static>>,
    insert_row: Option<Box<InsertFn>>,
}

impl TableBuilder {
    /// Set the callback returning the table's row count.
    pub fn count<F>(mut self, count: F) -> Self
    where
        F: Fn() -> usize + 'static,
    {
        self.row_count = Some(Box::new(count));
        self
    }

    /// Set the callback returning an estimated row count for query planning.
    pub fn estimate_rows<F>(mut self, estimate_rows: F) -> Self
    where
        F: Fn() -> usize + 'static,
    {
        self.estimate_rows = Some(Box::new(estimate_rows));
        self
    }

    /// Set a hook invoked with the SQL operation string before any
    /// INSERT/UPDATE/DELETE.
    pub fn on_modify<F>(mut self, before_modify: F) -> Self
    where
        F: Fn(&str) + 'static,
    {
        self.before_modify = Some(Box::new(before_modify));
        self
    }

    /// Install the complete SQLite transaction lifecycle for this table.
    pub fn transaction_hooks(mut self, transaction_hooks: TransactionHooks) -> Self {
        self.transaction_hooks = transaction_hooks;
        self
    }

    /// Add a column with a custom getter that writes the value for a given row
    /// index into the context.
    pub fn column<F>(self, name: impl Into<String>, column_type: ColumnType, getter: F) -> Self
    where
        F: for<'a> Fn(&mut FunctionContext<'a>, usize) + 'static,
    {
        self.add_column(name, column_type, Box::new(getter), None)
    }

    /// Add an `INTEGER` column whose getter returns an `i32` for a row index.
    pub fn column_int<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(usize) -> i32 + 'static,
    {
        self.add_column(
            name,
            ColumnType::Integer,
            column_helpers::index_getter_int(getter),
            None,
        )
    }

    /// Add an `INTEGER` column whose getter returns an `i64` for a row index.
    pub fn column_i64<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(usize) -> i64 + 'static,
    {
        self.add_column(
            name,
            ColumnType::Integer,
            column_helpers::index_getter_i64(getter),
            None,
        )
    }

    /// Alias for [`column_i64`](Self::column_i64).
    pub fn column_int64<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(usize) -> i64 + 'static,
    {
        self.column_i64(name, getter)
    }

    /// Add a `TEXT` column whose getter returns a `String` for a row index.
    pub fn column_text<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(usize) -> String + 'static,
    {
        self.add_column(
            name,
            ColumnType::Text,
            column_helpers::index_getter_text(getter),
            None,
        )
    }

    /// Add a `REAL` column whose getter returns an `f64` for a row index.
    pub fn column_double<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(usize) -> f64 + 'static,
    {
        self.add_column(
            name,
            ColumnType::Real,
            column_helpers::index_getter_double(getter),
            None,
        )
    }

    /// Add a `BLOB` column whose getter returns a `Vec<u8>` for a row index.
    pub fn column_blob<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(usize) -> Vec<u8> + 'static,
    {
        self.add_column(
            name,
            ColumnType::Blob,
            column_helpers::index_getter_blob(getter),
            None,
        )
    }

    /// Add a writable `INTEGER` (`i32`) column with a getter and a setter; the
    /// setter returns false to reject the write.
    pub fn column_int_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(usize) -> i32 + 'static,
        S: Fn(usize, i32) -> bool + 'static,
    {
        self.add_column(
            name,
            ColumnType::Integer,
            column_helpers::index_getter_int(getter),
            Some(column_helpers::index_setter_int(setter)),
        )
    }

    /// Add a writable `INTEGER` (`i64`) column with a getter and a setter; the
    /// setter returns false to reject the write.
    pub fn column_i64_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(usize) -> i64 + 'static,
        S: Fn(usize, i64) -> bool + 'static,
    {
        self.add_column(
            name,
            ColumnType::Integer,
            column_helpers::index_getter_i64(getter),
            Some(column_helpers::index_setter_i64(setter)),
        )
    }

    /// Alias for [`column_i64_rw`](Self::column_i64_rw).
    pub fn column_int64_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(usize) -> i64 + 'static,
        S: Fn(usize, i64) -> bool + 'static,
    {
        self.column_i64_rw(name, getter, setter)
    }

    /// Add a writable `TEXT` column with a getter and a setter; the setter
    /// returns false to reject the write.
    pub fn column_text_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(usize) -> String + 'static,
        S: Fn(usize, &str) -> bool + 'static,
    {
        self.add_column(
            name,
            ColumnType::Text,
            column_helpers::index_getter_text(getter),
            Some(column_helpers::index_setter_text(setter)),
        )
    }

    /// Add a writable `TEXT` column whose getter may return `None` (emitting SQL
    /// NULL); the setter receives the raw argument.
    pub fn column_text_nullable_rw<G, S>(
        self,
        name: impl Into<String>,
        getter: G,
        setter: S,
    ) -> Self
    where
        G: Fn(usize) -> Option<String> + 'static,
        S: for<'a> Fn(usize, &FunctionArg<'a>) -> bool + 'static,
    {
        self.add_column(
            name,
            ColumnType::Text,
            column_helpers::index_getter_nullable_text(getter),
            Some(Box::new(setter)),
        )
    }

    /// Add a writable column with a custom getter and setter operating on a row
    /// index.
    pub fn column_rw<G, S>(
        self,
        name: impl Into<String>,
        column_type: ColumnType,
        getter: G,
        setter: S,
    ) -> Self
    where
        G: for<'a> Fn(&mut FunctionContext<'a>, usize) + 'static,
        S: for<'a> Fn(usize, &FunctionArg<'a>) -> bool + 'static,
    {
        self.add_column(name, column_type, Box::new(getter), Some(Box::new(setter)))
    }

    /// Make the table deletable via a callback that deletes the row at the given
    /// index.
    pub fn deletable<F>(mut self, delete_row: F) -> Self
    where
        F: Fn(usize) -> bool + 'static,
    {
        self.delete_row = Some(Box::new(delete_row));
        self
    }

    /// Make the table insertable via a callback receiving the inserted column
    /// values.
    pub fn insertable<F>(mut self, insert_row: F) -> Self
    where
        F: for<'a> Fn(&[FunctionArg<'a>]) -> bool + 'static,
    {
        self.insert_row = Some(Box::new(insert_row));
        self
    }

    /// Register an equality-pushdown filter on `column_name` backed by a
    /// [`RowIterator`] factory keyed by an `i64` value.
    pub fn filter_eq<F>(
        self,
        column_name: impl AsRef<str>,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        F: Fn(i64) -> Box<dyn RowIterator> + 'static,
    {
        self.add_iterator_filter(
            column_name.as_ref(),
            estimated_cost,
            estimated_rows,
            move |value| factory(value.as_i64()),
        )
    }

    /// Register an equality-pushdown filter on `column_name` backed by a
    /// [`RowIterator`] factory keyed by a text value.
    pub fn filter_eq_text<F>(
        self,
        column_name: impl AsRef<str>,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        F: Fn(&str) -> Box<dyn RowIterator> + 'static,
    {
        self.add_iterator_filter(
            column_name.as_ref(),
            estimated_cost,
            estimated_rows,
            move |value| {
                let text = value.as_c_str().map(CStr::to_string_lossy);
                factory(text.as_deref().unwrap_or(""))
            },
        )
    }

    /// Register an equality-pushdown filter on `column_name` that returns the
    /// matching row indices.
    pub fn filter_eq_indices<F>(
        self,
        column_name: impl AsRef<str>,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        F: Fn(i64) -> Vec<usize> + 'static,
    {
        self.add_index_filter(
            column_name.as_ref(),
            estimated_cost,
            estimated_rows,
            move |value| factory(value.as_i64()),
        )
    }

    /// Register an equality-pushdown filter on `column_name` (text key) that
    /// returns the matching row indices.
    pub fn filter_eq_text_indices<F>(
        self,
        column_name: impl AsRef<str>,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        F: Fn(&str) -> Vec<usize> + 'static,
    {
        self.add_index_filter(
            column_name.as_ref(),
            estimated_cost,
            estimated_rows,
            move |value| {
                let text = value.as_c_str().map(CStr::to_string_lossy);
                factory(text.as_deref().unwrap_or(""))
            },
        )
    }

    /// Finalize the definition; errors if no count callback was set.
    pub fn build(self) -> Result<TableDef> {
        let Some(row_count) = self.row_count else {
            return Err(Error::Message(format!(
                "table '{}' is missing a count callback",
                self.name
            )));
        };
        Ok(TableDef {
            inner: Rc::new(TableInner {
                name: self.name,
                row_count,
                estimate_rows: self.estimate_rows,
                columns: self.columns,
                filters: self.filters,
                before_modify: self.before_modify,
                transaction_hooks: self.transaction_hooks,
                delete_row: self.delete_row,
                insert_row: self.insert_row,
            }),
        })
    }

    fn add_iterator_filter<F>(
        mut self,
        column_name: &str,
        estimated_cost: f64,
        estimated_rows: f64,
        factory: F,
    ) -> Self
    where
        F: for<'a> Fn(&FunctionArg<'a>) -> Box<dyn RowIterator> + 'static,
    {
        if let Some(column_index) = self.find_column(column_name) {
            let filter_id = self.filters.len() as c_int + 1;
            self.filters.push(FilterDef {
                column_index,
                filter_id,
                estimated_cost,
                estimated_rows: estimated_rows.max(1.0) as i64,
                factory: FilterFactory::Iterator(Box::new(factory)),
            });
        }
        self
    }

    fn add_index_filter<F>(
        mut self,
        column_name: &str,
        estimated_cost: f64,
        estimated_rows: f64,
        factory: F,
    ) -> Self
    where
        F: for<'a> Fn(&FunctionArg<'a>) -> Vec<usize> + 'static,
    {
        if let Some(column_index) = self.find_column(column_name) {
            let filter_id = self.filters.len() as c_int + 1;
            self.filters.push(FilterDef {
                column_index,
                filter_id,
                estimated_cost,
                estimated_rows: estimated_rows.max(1.0) as i64,
                factory: FilterFactory::Indices(Box::new(factory)),
            });
        }
        self
    }

    fn add_column(
        mut self,
        name: impl Into<String>,
        column_type: ColumnType,
        getter: Box<Getter>,
        setter: Option<Box<Setter>>,
    ) -> Self {
        self.columns.push(ColumnDef {
            name: name.into(),
            column_type,
            getter,
            setter,
        });
        self
    }

    fn find_column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }
}

#[repr(C)]
struct ModuleState {
    module: ffi::sqlite3_module,
    def: Rc<TableInner>,
    registry: *const RefCell<WriteSurfaceRegistry>,
    caps: WriteCaps,
}

#[repr(C)]
struct Vtab {
    base: ffi::sqlite3_vtab,
    def: Rc<TableInner>,
    transaction: TransactionLifecycle,
    registry: *const RefCell<WriteSurfaceRegistry>,
    schema_name: String,
    table_name: String,
}

#[repr(C)]
struct Cursor {
    base: ffi::sqlite3_vtab_cursor,
    def: Rc<TableInner>,
    idx: usize,
    total: usize,
    indices: Option<Vec<usize>>,
    pos: usize,
    iterator: Option<Box<dyn RowIterator>>,
    iterator_eof: bool,
}

pub(crate) fn register_index_table(
    db: *mut ffi::sqlite3,
    registry: *const RefCell<WriteSurfaceRegistry>,
    module_name: &str,
    def: &TableDef,
) -> Result<()> {
    if db.is_null() {
        return Err(Error::DatabaseNotOpen);
    }
    let module_name = CString::new(module_name)?;
    let state = Box::new(ModuleState {
        module: create_module(),
        def: def.inner.clone(),
        registry,
        caps: def.write_caps(),
    });
    let state_ptr = Box::into_raw(state);
    let module_ptr = unsafe { &(*state_ptr).module as *const ffi::sqlite3_module };
    let rc = unsafe {
        ffi::sqlite3_create_module_v2(
            db,
            module_name.as_ptr(),
            module_ptr,
            state_ptr.cast::<c_void>(),
            Some(destroy_module_state),
        )
    };
    if sqlite_ok(rc) {
        Ok(())
    } else {
        Err(Error::sqlite(rc, crate::function::sqlite_error(db)))
    }
}

fn create_module() -> ffi::sqlite3_module {
    let mut module: ffi::sqlite3_module = unsafe { std::mem::zeroed() };
    module.iVersion = 3;
    module.xCreate = Some(vtab_connect);
    module.xConnect = Some(vtab_connect);
    module.xBestIndex = Some(vtab_best_index);
    module.xDisconnect = Some(vtab_disconnect);
    module.xDestroy = Some(vtab_destroy);
    module.xOpen = Some(vtab_open);
    module.xClose = Some(vtab_close);
    module.xFilter = Some(vtab_filter);
    module.xNext = Some(vtab_next);
    module.xEof = Some(vtab_eof);
    module.xColumn = Some(vtab_column);
    module.xRowid = Some(vtab_rowid);
    module.xUpdate = Some(vtab_update);
    // xBegin enrolls the vtab. The fallible hook runs from xSync, whose return
    // SQLite propagates; xCommit only clears state because SQLite ignores its
    // return code.
    module.xBegin = Some(vtab_begin);
    module.xSync = Some(vtab_sync);
    module.xCommit = Some(vtab_commit);
    module.xRollback = Some(vtab_rollback);
    module.xSavepoint = Some(vtab_savepoint);
    module.xRelease = Some(vtab_release);
    module.xRollbackTo = Some(vtab_rollback_to);
    module
}

unsafe extern "C" fn destroy_module_state(ptr: *mut c_void) {
    unsafe {
        if !ptr.is_null() {
            drop(Box::from_raw(ptr.cast::<ModuleState>()));
        }
    }
}

unsafe extern "C" fn vtab_connect(
    db: *mut ffi::sqlite3,
    p_aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp_vtab: *mut *mut ffi::sqlite3_vtab,
    pz_err: *mut *mut c_char,
) -> c_int {
    unsafe {
        callback_status(pz_err, || {
            let state = p_aux
                .cast::<ModuleState>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql module state is null".to_string()))?;
            let schema = CString::new(state.def.schema())?;
            let rc = ffi::sqlite3_declare_vtab(db, schema.as_ptr());
            if !sqlite_ok(rc) {
                return Err(Error::sqlite(rc, crate::function::sqlite_error(db)));
            }
            let transaction = TransactionLifecycle::new(&state.def.transaction_hooks)?;
            let (schema_name, table_name) =
                connect_write_surface(state.registry, argc, argv, state.caps.clone())?;
            let vtab = Box::new(Vtab {
                base: std::mem::zeroed(),
                def: state.def.clone(),
                transaction,
                registry: state.registry,
                schema_name,
                table_name,
            });
            let vtab_ptr = Box::into_raw(vtab);
            *pp_vtab = &mut (*vtab_ptr).base;
            Ok(ffi::SQLITE_OK)
        })
    }
}

unsafe extern "C" fn vtab_destroy(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if !p_vtab.is_null() {
            let vtab = Box::from_raw(p_vtab.cast::<Vtab>());
            destroy_write_surface(vtab.registry, &vtab.schema_name, &vtab.table_name);
            drop(vtab);
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn vtab_disconnect(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if !p_vtab.is_null() {
            drop(Box::from_raw(p_vtab.cast::<Vtab>()));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn vtab_open(
    p_vtab: *mut ffi::sqlite3_vtab,
    pp_cursor: *mut *mut ffi::sqlite3_vtab_cursor,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<Vtab>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql vtab is null".to_string()))?;
            let cursor = Box::new(Cursor {
                base: std::mem::zeroed(),
                def: vtab.def.clone(),
                idx: 0,
                total: 0,
                indices: None,
                pos: 0,
                iterator: None,
                iterator_eof: false,
            });
            let cursor_ptr = Box::into_raw(cursor);
            *pp_cursor = &mut (*cursor_ptr).base;
            Ok::<_, Error>(())
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn vtab_close(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        if !cursor.is_null() {
            drop(Box::from_raw(cursor.cast::<Cursor>()));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn vtab_filter(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    idx_num: c_int,
    _idx_str: *const c_char,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<Cursor>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql cursor is null".to_string()))?;
            cursor.idx = 0;
            cursor.total = 0;
            cursor.indices = None;
            cursor.pos = 0;
            cursor.iterator = None;
            cursor.iterator_eof = false;

            if idx_num != FILTER_NONE && argc > 0 {
                let filter = cursor.def.find_filter_by_id(idx_num);
                if let Some(filter) = filter {
                    let arg = FunctionArg::new(*argv);
                    match &filter.factory {
                        FilterFactory::Iterator(factory) => {
                            let mut iterator = factory(&arg);
                            cursor.iterator_eof = !iterator.next()?;
                            cursor.iterator = Some(iterator);
                        }
                        FilterFactory::Indices(factory) => {
                            let indices = factory(&arg);
                            cursor.total = indices.len();
                            cursor.indices = Some(indices);
                        }
                    }
                    return Ok(());
                }
            }

            cursor.total = (cursor.def.row_count)();
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn vtab_next(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<Cursor>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql cursor is null".to_string()))?;
            if let Some(iterator) = cursor.iterator.as_mut() {
                cursor.iterator_eof = !iterator.next()?;
            } else if cursor.indices.is_some() {
                cursor.pos = cursor.pos.saturating_add(1);
            } else {
                cursor.idx = cursor.idx.saturating_add(1);
            }
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn vtab_eof(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        let Some(cursor) = cursor.cast::<Cursor>().as_ref() else {
            return 1;
        };
        if let Some(iterator) = cursor.iterator.as_ref() {
            return i32::from(cursor.iterator_eof || iterator.eof());
        }
        if let Some(indices) = cursor.indices.as_ref() {
            return i32::from(cursor.pos >= indices.len());
        }
        i32::from(cursor.idx >= cursor.total)
    }
}

unsafe extern "C" fn vtab_column(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    ctx: *mut ffi::sqlite3_context,
    col: c_int,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            if sqlite3_vtab_nochange(ctx) != 0 {
                return Ok(());
            }
            let cursor = cursor
                .cast::<Cursor>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cursor is null".to_string()))?;
            let mut fctx = FunctionContext::new(ctx);
            if col < 0 || col as usize >= cursor.def.columns.len() {
                fctx.result_null();
                return Ok(());
            }
            if let Some(iterator) = cursor.iterator.as_ref() {
                if cursor.iterator_eof {
                    fctx.result_null();
                } else {
                    iterator.column(&mut fctx, col)?;
                }
                return Ok(());
            }
            let row = cursor.current_row_index();
            (cursor.def.columns[col as usize].getter)(&mut fctx, row);
            Ok(())
        }));
        match result {
            Ok(Ok(())) => ffi::SQLITE_OK,
            Ok(Err(err)) => {
                FunctionContext::new(ctx).result_error(err.to_string());
                ffi::SQLITE_ERROR
            }
            Err(_) => {
                FunctionContext::new(ctx).result_error("xsql virtual table column panicked");
                ffi::SQLITE_ERROR
            }
        }
    }
}

unsafe extern "C" fn vtab_rowid(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<Cursor>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql cursor is null".to_string()))?;
            if let Some(iterator) = cursor.iterator.as_ref() {
                *rowid = if cursor.iterator_eof {
                    0
                } else {
                    iterator.rowid()
                };
            } else {
                *rowid = cursor.current_row_index() as i64;
            }
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn vtab_best_index(
    p_vtab: *mut ffi::sqlite3_vtab,
    index_info: *mut ffi::sqlite3_index_info,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let vtab = p_vtab
                .cast::<Vtab>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql vtab is null".to_string()))?;
            let info = index_info
                .as_mut()
                .ok_or_else(|| Error::Message("sqlite index_info is null".to_string()))?;
            let mut best_filter: Option<&FilterDef> = None;
            let mut best_constraint_index = -1;

            for i in 0..info.nConstraint {
                let constraint = &*info.aConstraint.add(i as usize);
                if constraint.usable == 0 {
                    continue;
                }
                if constraint.op as i32 != ffi::SQLITE_INDEX_CONSTRAINT_EQ {
                    continue;
                }
                if constraint.iColumn < 0 {
                    continue;
                }
                if let Some(filter) = vtab.def.find_filter_by_column(constraint.iColumn as usize) {
                    let replace = best_filter
                        .map(|best| filter.estimated_cost < best.estimated_cost)
                        .unwrap_or(true);
                    if replace {
                        best_filter = Some(filter);
                        best_constraint_index = i;
                    }
                }
            }

            if let Some(filter) = best_filter {
                let usage = &mut *info.aConstraintUsage.add(best_constraint_index as usize);
                usage.argvIndex = 1;
                usage.omit = 1;
                info.idxNum = filter.filter_id;
                info.estimatedCost = filter.estimated_cost;
                info.estimatedRows = filter.estimated_rows;
            } else {
                let estimated_rows = vtab
                    .def
                    .estimate_rows
                    .as_ref()
                    .map(|estimate| estimate())
                    .unwrap_or(100_000);
                info.idxNum = FILTER_NONE;
                info.estimatedCost = estimated_rows as f64;
                info.estimatedRows = estimated_rows as i64;
            }
            Ok(())
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn vtab_update(
    p_vtab: *mut ffi::sqlite3_vtab,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
    rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<c_int> {
            let vtab = p_vtab
                .cast::<Vtab>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql vtab is null".to_string()))?;
            if argc <= 0 || argv.is_null() {
                return Ok(ffi::SQLITE_READONLY);
            }
            let old_rowid = FunctionArg::new(*argv);

            if argc == 1 && !old_rowid.is_null() {
                let Some(delete_row) = vtab.def.delete_row.as_ref() else {
                    return Ok(unsupported_delete(p_vtab, &vtab.def.name));
                };
                // A negative rowid cannot map to a valid 0-based row index; reject
                // it before it wraps to a huge usize handed to the delete callback.
                let raw_rowid = old_rowid.as_i64();
                if raw_rowid < 0 {
                    return Err(Error::Message("virtual table delete failed".to_string()));
                }
                vtab.transaction.touch();
                call_modify_hook(&vtab.def, &format!("DELETE FROM {}", vtab.def.name));
                if delete_row(raw_rowid as usize) {
                    vtab.transaction.mark_written();
                    return Ok(ffi::SQLITE_OK);
                }
                return Err(Error::Message("virtual table delete failed".to_string()));
            }

            if argc > 1 && !old_rowid.is_null() {
                // No writable column at all -> the UPDATE surface is unsupported;
                // report the capability-scoped error (mirrors the C++ base module's
                // unsupported_update, and matches the recorded caps / authorizer).
                if !vtab
                    .def
                    .columns
                    .iter()
                    .any(|column| column.setter.is_some())
                {
                    return Ok(unsupported_update(p_vtab, &vtab.def.name));
                }
                // A negative rowid cannot map to a valid 0-based row index; reject
                // it before it wraps to a huge usize handed to the column setters.
                let raw_rowid = old_rowid.as_i64();
                if raw_rowid < 0 {
                    return Err(Error::Message(
                        "virtual table update failed: negative rowid".to_string(),
                    ));
                }
                vtab.transaction.touch();
                call_modify_hook(&vtab.def, &format!("UPDATE {}", vtab.def.name));
                let old_rowid = raw_rowid as usize;
                // A row here is always resolved by rowid, so unchanged columns arrive
                // as NOCHANGE -- always NOCHANGE-eligible.
                let args = build_args(argc - 2, argv.add(2));
                apply_update_columns(
                    &args,
                    vtab.def.columns.len(),
                    true,
                    |i| vtab.def.columns[i].setter.is_some(),
                    |i| vtab.def.columns[i].name.clone(),
                    |i, value| {
                        vtab.def.columns[i]
                            .setter
                            .as_ref()
                            .expect("apply runs only for writable columns")(
                            old_rowid, value
                        )
                    },
                )?;
                vtab.transaction.mark_written();
                return Ok(ffi::SQLITE_OK);
            }

            if argc > 1 && old_rowid.is_null() {
                let Some(insert_row) = vtab.def.insert_row.as_ref() else {
                    return Ok(unsupported_insert(p_vtab, &vtab.def.name));
                };
                vtab.transaction.touch();
                call_modify_hook(&vtab.def, &format!("INSERT INTO {}", vtab.def.name));
                let args = build_args(argc - 2, argv.add(2));
                if insert_row(&args) {
                    if !rowid.is_null() {
                        *rowid = 0;
                    }
                    vtab.transaction.mark_written();
                    return Ok(ffi::SQLITE_OK);
                }
                return Err(Error::Message("virtual table insert failed".to_string()));
            }

            Ok(ffi::SQLITE_READONLY)
        }));
        match result {
            Ok(Ok(status)) => status,
            Ok(Err(err)) => {
                set_vtab_error(p_vtab, &err.to_string());
                ffi::SQLITE_ERROR
            }
            Err(_) => {
                set_vtab_error(p_vtab, "xsql virtual table update panicked");
                ffi::SQLITE_ERROR
            }
        }
    }
}

unsafe extern "C" fn vtab_begin(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if let Some(vtab) = p_vtab.cast::<Vtab>().as_ref() {
            vtab.transaction.begin();
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn vtab_sync(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<Vtab>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql vtab is null".to_string()))?;
            vtab.transaction.sync(&vtab.def.transaction_hooks)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn vtab_commit(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if let Some(vtab) = p_vtab.cast::<Vtab>().as_ref() {
            // SQLite ignores xCommit/xRollback errors. Contain a panic without
            // allocating zErrMsg that nobody will consume or free.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                vtab.transaction.commit(&vtab.def.transaction_hooks);
            }));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn vtab_rollback(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if let Some(vtab) = p_vtab.cast::<Vtab>().as_ref() {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                vtab.transaction.rollback(&vtab.def.transaction_hooks);
            }));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn vtab_savepoint(p_vtab: *mut ffi::sqlite3_vtab, id: c_int) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<Vtab>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql vtab is null".to_string()))?;
            vtab.transaction.savepoint(&vtab.def.transaction_hooks, id)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn vtab_release(p_vtab: *mut ffi::sqlite3_vtab, id: c_int) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<Vtab>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql vtab is null".to_string()))?;
            vtab.transaction.release(&vtab.def.transaction_hooks, id)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn vtab_rollback_to(p_vtab: *mut ffi::sqlite3_vtab, id: c_int) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<Vtab>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql vtab is null".to_string()))?;
            vtab.transaction
                .rollback_to(&vtab.def.transaction_hooks, id)
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

impl Cursor {
    fn current_row_index(&self) -> usize {
        self.indices
            .as_ref()
            .and_then(|indices| indices.get(self.pos).copied())
            .unwrap_or(self.idx)
    }
}

fn call_modify_hook(def: &TableInner, operation: &str) {
    if let Some(before_modify) = def.before_modify.as_ref() {
        before_modify(operation);
    }
}
