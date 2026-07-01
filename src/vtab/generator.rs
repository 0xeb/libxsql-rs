// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::error::{Error, Result, sqlite_ok};
use crate::function::{FunctionArg, FunctionContext, build_args};
use libsqlite3_sys as ffi;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use super::cached::{CachedCount, CachedFilterFactory};
use super::ffi::{
    c_string_to_string, call_cached_modify_hook, callback_status, finish_cursor_unit,
    finish_vtab_unit, parse_constraint_index_list, quote_identifier, set_vtab_error,
    sqlite_mprintf_string, sqlite3_vtab_nochange,
};
use super::index::InsertFn;
use super::{
    ColumnType, ConstraintOp, ConstraintRequest, FILTER_NONE, Generator, GeneratorConstraintArg,
    ModifyHook, RowIterator,
};

const MISSING_REQUIRED_CONSTRAINT: c_int = -2;
const CONSTRAINT_FILTER_BASE: c_int = 2_000;
const PARAMETRIC_FILTER_BASE: c_int = 500;

type GeneratorFactory<Row> = dyn Fn() -> Box<dyn Generator<Row>> + 'static;
type GeneratorGetter<Row> = dyn for<'a> Fn(&mut FunctionContext<'a>, &Row) + 'static;
type GeneratorSetter<Row> = dyn for<'a> Fn(&mut Row, &FunctionArg<'a>) -> bool + 'static;
type GeneratorParametricFactory<Row> =
    dyn for<'a> Fn(&[FunctionArg<'a>]) -> Box<dyn Generator<Row>> + 'static;
type GeneratorConstraintFactory<Row> =
    dyn for<'a> Fn(&[GeneratorConstraintArg<'a>]) -> Box<dyn Generator<Row>> + 'static;
type GeneratorDelete<Row> = dyn Fn(&Row) -> bool + 'static;
type GeneratorLookup<Row> = dyn Fn(i64) -> Option<Row> + 'static;

struct GeneratorColumnDef<Row> {
    name: String,
    column_type: ColumnType,
    getter: Box<GeneratorGetter<Row>>,
    setter: Option<Box<GeneratorSetter<Row>>>,
    hidden: bool,
}

struct GeneratorFilterDef {
    column_index: usize,
    filter_id: c_int,
    estimated_cost: f64,
    estimated_rows: i64,
    factory: Box<CachedFilterFactory>,
}

struct ParametricFilterDef<Row> {
    column_indices: Vec<usize>,
    filter_id: c_int,
    estimated_cost: f64,
    estimated_rows: i64,
    factory: Box<GeneratorParametricFactory<Row>>,
}

struct GeneratorConstraintSpec {
    column_index: usize,
    op: ConstraintOp,
    required: bool,
    missing_error: String,
}

struct ConstraintFilterDef<Row> {
    specs: Vec<GeneratorConstraintSpec>,
    filter_id: c_int,
    estimated_cost: f64,
    estimated_rows: i64,
    ordered_column: Option<usize>,
    ordered_desc: bool,
    factory: Box<GeneratorConstraintFactory<Row>>,
}

struct GeneratorInner<Row> {
    name: String,
    estimate_rows: Option<Box<CachedCount>>,
    generator: Box<GeneratorFactory<Row>>,
    columns: Vec<GeneratorColumnDef<Row>>,
    filters: Vec<GeneratorFilterDef>,
    parametric_filters: Vec<ParametricFilterDef<Row>>,
    constraint_filters: Vec<ConstraintFilterDef<Row>>,
    full_scan_error: Option<String>,
    before_modify: Option<Box<ModifyHook>>,
    after_modify: Option<Box<ModifyHook>>,
    delete_row: Option<Box<GeneratorDelete<Row>>>,
    insert_row: Option<Box<InsertFn>>,
    row_lookup: Option<Box<GeneratorLookup<Row>>>,
}

impl<Row> GeneratorInner<Row> {
    fn schema(&self) -> String {
        let columns = self
            .columns
            .iter()
            .map(|column| {
                let mut declaration = format!(
                    "{} {}",
                    quote_identifier(&column.name),
                    column.column_type.sql()
                );
                if column.hidden {
                    declaration.push_str(" HIDDEN");
                }
                declaration
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("CREATE TABLE {}({columns})", quote_identifier(&self.name))
    }

    fn find_filter_by_column(&self, column_index: usize) -> Option<&GeneratorFilterDef> {
        self.filters
            .iter()
            .filter(|filter| filter.column_index == column_index)
            .min_by(|a, b| a.estimated_cost.total_cmp(&b.estimated_cost))
    }

    fn find_filter_by_id(&self, filter_id: c_int) -> Option<&GeneratorFilterDef> {
        self.filters
            .iter()
            .find(|filter| filter.filter_id == filter_id)
    }
}

/// Definition for a generator-backed virtual table.
#[derive(Clone)]
pub struct GeneratorTableDef<Row> {
    inner: Rc<GeneratorInner<Row>>,
}

impl<Row> GeneratorTableDef<Row> {
    /// Returns the table's name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }
}

/// Start a generator-backed virtual table definition.
pub fn generator_table<Row: 'static>(name: impl Into<String>) -> GeneratorTableBuilder<Row> {
    GeneratorTableBuilder {
        name: name.into(),
        estimate_rows: None,
        generator: None,
        columns: Vec::new(),
        filters: Vec::new(),
        parametric_filters: Vec::new(),
        constraint_filters: Vec::new(),
        full_scan_error: None,
        before_modify: None,
        after_modify: None,
        delete_row: None,
        insert_row: None,
        row_lookup: None,
    }
}

/// Fluent builder for generator-backed virtual tables.
pub struct GeneratorTableBuilder<Row> {
    name: String,
    estimate_rows: Option<Box<CachedCount>>,
    generator: Option<Box<GeneratorFactory<Row>>>,
    columns: Vec<GeneratorColumnDef<Row>>,
    filters: Vec<GeneratorFilterDef>,
    parametric_filters: Vec<ParametricFilterDef<Row>>,
    constraint_filters: Vec<ConstraintFilterDef<Row>>,
    full_scan_error: Option<String>,
    before_modify: Option<Box<ModifyHook>>,
    after_modify: Option<Box<ModifyHook>>,
    delete_row: Option<Box<GeneratorDelete<Row>>>,
    insert_row: Option<Box<InsertFn>>,
    row_lookup: Option<Box<GeneratorLookup<Row>>>,
}

impl<Row: 'static> GeneratorTableBuilder<Row> {
    /// Set the callback returning an estimated row count for query planning.
    pub fn estimate_rows<F>(mut self, estimate_rows: F) -> Self
    where
        F: Fn() -> usize + 'static,
    {
        self.estimate_rows = Some(Box::new(estimate_rows));
        self
    }

    /// Set the factory that produces a fresh [`Generator`] for a full scan.
    pub fn generator<F>(mut self, generator: F) -> Self
    where
        F: Fn() -> Box<dyn Generator<Row>> + 'static,
    {
        self.generator = Some(Box::new(generator));
        self
    }

    /// Make an unconstrained full scan fail with `message`, forcing callers to
    /// supply a pushdown constraint.
    pub fn full_scan_error(mut self, message: impl Into<String>) -> Self {
        self.full_scan_error = Some(message.into());
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

    /// Set a hook invoked with the SQL operation string after a successful
    /// INSERT/UPDATE/DELETE.
    pub fn after_modify<F>(mut self, after_modify: F) -> Self
    where
        F: Fn(&str) + 'static,
    {
        self.after_modify = Some(Box::new(after_modify));
        self
    }

    /// Add a column with a custom getter that writes the value for a row
    /// reference into the context.
    pub fn column<F>(mut self, name: impl Into<String>, column_type: ColumnType, getter: F) -> Self
    where
        F: for<'a> Fn(&mut FunctionContext<'a>, &Row) + 'static,
    {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type,
            getter: Box::new(getter),
            setter: None,
            hidden: false,
        });
        self
    }

    /// Add an `INTEGER` column whose getter returns an `i32` for a row.
    pub fn column_int<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> i32 + 'static,
    {
        self.column(name, ColumnType::Integer, move |ctx, row| {
            ctx.result_int(getter(row));
        })
    }

    /// Add an `INTEGER` column whose getter returns an `i64` for a row.
    pub fn column_i64<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> i64 + 'static,
    {
        self.column(name, ColumnType::Integer, move |ctx, row| {
            ctx.result_i64(getter(row));
        })
    }

    /// Alias for [`column_i64`](Self::column_i64).
    pub fn column_int64<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> i64 + 'static,
    {
        self.column_i64(name, getter)
    }

    /// Add a `TEXT` column whose getter returns a `String` for a row.
    pub fn column_text<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> String + 'static,
    {
        self.column(name, ColumnType::Text, move |ctx, row| {
            ctx.result_text(getter(row));
        })
    }

    /// Add a `REAL` column whose getter returns an `f64` for a row.
    pub fn column_double<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> f64 + 'static,
    {
        self.column(name, ColumnType::Real, move |ctx, row| {
            ctx.result_double(getter(row));
        })
    }

    /// Add a `BLOB` column whose getter returns a `Vec<u8>` for a row.
    pub fn column_blob<F>(self, name: impl Into<String>, getter: F) -> Self
    where
        F: Fn(&Row) -> Vec<u8> + 'static,
    {
        self.column(name, ColumnType::Blob, move |ctx, row| {
            ctx.result_blob(&getter(row));
        })
    }

    /// Add a writable column with a custom getter and setter operating on a row
    /// reference.
    pub fn column_rw<G, S>(
        mut self,
        name: impl Into<String>,
        column_type: ColumnType,
        getter: G,
        setter: S,
    ) -> Self
    where
        G: for<'a> Fn(&mut FunctionContext<'a>, &Row) + 'static,
        S: for<'a> Fn(&mut Row, &FunctionArg<'a>) -> bool + 'static,
    {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type,
            getter: Box::new(getter),
            setter: Some(Box::new(setter)),
            hidden: false,
        });
        self
    }

    /// Add a writable `INTEGER` (`i32`) column; the setter returns false to
    /// reject the write.
    pub fn column_int_rw<G, S>(mut self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> i32 + 'static,
        S: Fn(&mut Row, i32) -> bool + 'static,
    {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type: ColumnType::Integer,
            getter: Box::new(move |ctx, row| ctx.result_int(getter(row))),
            setter: Some(Box::new(move |row, value| setter(row, value.as_i32()))),
            hidden: false,
        });
        self
    }

    /// Add a writable `INTEGER` (`i64`) column; the setter returns false to
    /// reject the write.
    pub fn column_i64_rw<G, S>(mut self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> i64 + 'static,
        S: Fn(&mut Row, i64) -> bool + 'static,
    {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type: ColumnType::Integer,
            getter: Box::new(move |ctx, row| ctx.result_i64(getter(row))),
            setter: Some(Box::new(move |row, value| setter(row, value.as_i64()))),
            hidden: false,
        });
        self
    }

    /// Alias for [`column_i64_rw`](Self::column_i64_rw).
    pub fn column_int64_rw<G, S>(self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> i64 + 'static,
        S: Fn(&mut Row, i64) -> bool + 'static,
    {
        self.column_i64_rw(name, getter, setter)
    }

    /// Add a writable `TEXT` column; the setter returns false to reject the
    /// write.
    pub fn column_text_rw<G, S>(mut self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> String + 'static,
        S: Fn(&mut Row, &str) -> bool + 'static,
    {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type: ColumnType::Text,
            getter: Box::new(move |ctx, row| ctx.result_text(getter(row))),
            setter: Some(Box::new(move |row, value| {
                let text = value.as_c_str().map(CStr::to_string_lossy);
                setter(row, text.as_deref().unwrap_or(""))
            })),
            hidden: false,
        });
        self
    }

    /// Add a writable `TEXT` column whose getter may return `None` (emitting SQL
    /// NULL); the setter receives the raw argument.
    pub fn column_text_nullable_rw<G, S>(
        mut self,
        name: impl Into<String>,
        getter: G,
        setter: S,
    ) -> Self
    where
        G: Fn(&Row) -> Option<String> + 'static,
        S: for<'a> Fn(&mut Row, &FunctionArg<'a>) -> bool + 'static,
    {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type: ColumnType::Text,
            getter: Box::new(move |ctx, row| match getter(row) {
                Some(value) => ctx.result_text(value),
                None => ctx.result_null(),
            }),
            setter: Some(Box::new(setter)),
            hidden: false,
        });
        self
    }

    /// Add a writable `REAL` column; the setter returns false to reject the
    /// write.
    pub fn column_double_rw<G, S>(mut self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> f64 + 'static,
        S: Fn(&mut Row, f64) -> bool + 'static,
    {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type: ColumnType::Real,
            getter: Box::new(move |ctx, row| ctx.result_double(getter(row))),
            setter: Some(Box::new(move |row, value| setter(row, value.as_f64()))),
            hidden: false,
        });
        self
    }

    /// Add a writable `BLOB` column; the setter returns false to reject the
    /// write.
    pub fn column_blob_rw<G, S>(mut self, name: impl Into<String>, getter: G, setter: S) -> Self
    where
        G: Fn(&Row) -> Vec<u8> + 'static,
        S: Fn(&mut Row, &[u8]) -> bool + 'static,
    {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type: ColumnType::Blob,
            getter: Box::new(move |ctx, row| ctx.result_blob(&getter(row))),
            setter: Some(Box::new(move |row, value| setter(row, value.as_blob()))),
            hidden: false,
        });
        self
    }

    /// Add a hidden `INTEGER` column (usable as a constraint input but not
    /// returned by `*`).
    pub fn hidden_column_i64(self, name: impl Into<String>) -> Self {
        self.hidden_column(name, ColumnType::Integer)
    }

    /// Alias for [`hidden_column_i64`](Self::hidden_column_i64).
    pub fn hidden_column_int64(self, name: impl Into<String>) -> Self {
        self.hidden_column_i64(name)
    }

    /// Add a hidden `INTEGER` column (usable as a constraint input but not
    /// returned by `*`).
    pub fn hidden_column_int(self, name: impl Into<String>) -> Self {
        self.hidden_column(name, ColumnType::Integer)
    }

    /// Add a hidden `TEXT` column (usable as a constraint input but not returned
    /// by `*`).
    pub fn hidden_column_text(self, name: impl Into<String>) -> Self {
        self.hidden_column(name, ColumnType::Text)
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
        self.add_filter(
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
        self.add_filter(
            column_name.as_ref(),
            estimated_cost,
            estimated_rows,
            move |value| {
                let text = value.as_c_str().map(CStr::to_string_lossy);
                factory(text.as_deref().unwrap_or(""))
            },
        )
    }

    /// Register a filter requiring equality constraints on all `param_columns`,
    /// producing a [`Generator`] from the matched values.
    pub fn parametric_filter<I, S, F>(
        mut self,
        param_columns: I,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
        F: for<'a> Fn(&[FunctionArg<'a>]) -> Box<dyn Generator<Row>> + 'static,
    {
        let mut column_indices = Vec::new();
        for column_name in param_columns {
            let Some(column_index) = self.find_column(column_name.as_ref()) else {
                return self;
            };
            column_indices.push(column_index);
        }
        let filter_id = PARAMETRIC_FILTER_BASE + self.parametric_filters.len() as c_int;
        self.parametric_filters.push(ParametricFilterDef {
            column_indices,
            filter_id,
            estimated_cost,
            estimated_rows: estimated_rows.max(1.0) as i64,
            factory: Box::new(factory),
        });
        self
    }

    /// Register a filter driven by a set of required/optional
    /// [`ConstraintRequest`]s; the factory receives the matched constraint values.
    pub fn constraint_filter<I, F>(
        mut self,
        constraints: I,
        factory: F,
        estimated_cost: f64,
        estimated_rows: f64,
    ) -> Self
    where
        I: IntoIterator<Item = ConstraintRequest>,
        F: for<'a> Fn(&[GeneratorConstraintArg<'a>]) -> Box<dyn Generator<Row>> + 'static,
    {
        let mut specs = Vec::new();
        for constraint in constraints {
            let Some(column_index) = self.find_column(&constraint.column_name) else {
                return self;
            };
            specs.push(GeneratorConstraintSpec {
                column_index,
                op: constraint.op,
                required: constraint.required,
                missing_error: constraint.missing_error,
            });
        }
        let filter_id = CONSTRAINT_FILTER_BASE + self.constraint_filters.len() as c_int;
        self.constraint_filters.push(ConstraintFilterDef {
            specs,
            filter_id,
            estimated_cost,
            estimated_rows: estimated_rows.max(1.0) as i64,
            ordered_column: None,
            ordered_desc: false,
            factory: Box::new(factory),
        });
        self
    }

    /// Mark the most recently added constraint filter as satisfying an `ORDER BY`
    /// on `column_name` (in `desc` direction), so SQLite skips its own sort.
    pub fn order_by_consumed(mut self, column_name: impl AsRef<str>, desc: bool) -> Self {
        let Some(column_index) = self.find_column(column_name.as_ref()) else {
            return self;
        };
        if let Some(filter) = self.constraint_filters.last_mut() {
            filter.ordered_column = Some(column_index);
            filter.ordered_desc = desc;
        }
        self
    }

    /// Set a callback that resolves a single row by rowid, used for rowid lookups
    /// and update/delete reconstruction.
    pub fn row_lookup<F>(mut self, row_lookup: F) -> Self
    where
        F: Fn(i64) -> Option<Row> + 'static,
    {
        self.row_lookup = Some(Box::new(row_lookup));
        self
    }

    /// Make the table deletable via a callback that deletes the given row.
    pub fn deletable<F>(mut self, delete_row: F) -> Self
    where
        F: Fn(&Row) -> bool + 'static,
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

    /// Finalize the definition; errors if no generator callback was set.
    pub fn build(self) -> Result<GeneratorTableDef<Row>> {
        let Some(generator) = self.generator else {
            return Err(Error::Message(format!(
                "generator table '{}' is missing a generator callback",
                self.name
            )));
        };
        Ok(GeneratorTableDef {
            inner: Rc::new(GeneratorInner {
                name: self.name,
                estimate_rows: self.estimate_rows,
                generator,
                columns: self.columns,
                filters: self.filters,
                parametric_filters: self.parametric_filters,
                constraint_filters: self.constraint_filters,
                full_scan_error: self.full_scan_error,
                before_modify: self.before_modify,
                after_modify: self.after_modify,
                delete_row: self.delete_row,
                insert_row: self.insert_row,
                row_lookup: self.row_lookup,
            }),
        })
    }

    fn hidden_column(mut self, name: impl Into<String>, column_type: ColumnType) -> Self {
        self.columns.push(GeneratorColumnDef {
            name: name.into(),
            column_type,
            getter: Box::new(|ctx, _row| ctx.result_null()),
            setter: None,
            hidden: true,
        });
        self
    }

    fn add_filter<F>(
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
            self.filters.push(GeneratorFilterDef {
                column_index,
                filter_id,
                estimated_cost,
                estimated_rows: estimated_rows.max(1.0) as i64,
                factory: Box::new(factory),
            });
        }
        self
    }

    fn find_column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }
}

#[repr(C)]
struct GeneratorModuleState<Row> {
    module: ffi::sqlite3_module,
    def: Rc<GeneratorInner<Row>>,
}

#[repr(C)]
struct GeneratorVtab<Row> {
    base: ffi::sqlite3_vtab,
    def: Rc<GeneratorInner<Row>>,
}

#[repr(C)]
struct GeneratorCursor<Row> {
    base: ffi::sqlite3_vtab_cursor,
    vtab: *mut ffi::sqlite3_vtab,
    def: Rc<GeneratorInner<Row>>,
    generator: Option<Box<dyn Generator<Row>>>,
    iterator: Option<Box<dyn RowIterator>>,
    eof: bool,
}

pub(crate) fn register_generator_table<Row: 'static>(
    db: *mut ffi::sqlite3,
    module_name: &str,
    def: &GeneratorTableDef<Row>,
) -> Result<()> {
    if db.is_null() {
        return Err(Error::DatabaseNotOpen);
    }
    let module_name = CString::new(module_name)?;
    let state = Box::new(GeneratorModuleState {
        module: create_generator_module::<Row>(),
        def: def.inner.clone(),
    });
    let state_ptr = Box::into_raw(state);
    let module_ptr = unsafe { &(*state_ptr).module as *const ffi::sqlite3_module };
    let rc = unsafe {
        ffi::sqlite3_create_module_v2(
            db,
            module_name.as_ptr(),
            module_ptr,
            state_ptr.cast::<c_void>(),
            Some(destroy_generator_module_state::<Row>),
        )
    };
    if sqlite_ok(rc) {
        Ok(())
    } else {
        Err(Error::sqlite(rc, crate::function::sqlite_error(db)))
    }
}

fn create_generator_module<Row: 'static>() -> ffi::sqlite3_module {
    let mut module: ffi::sqlite3_module = unsafe { std::mem::zeroed() };
    module.iVersion = 3;
    module.xCreate = Some(generator_vtab_connect::<Row>);
    module.xConnect = Some(generator_vtab_connect::<Row>);
    module.xBestIndex = Some(generator_vtab_best_index::<Row>);
    module.xDisconnect = Some(generator_vtab_disconnect::<Row>);
    module.xDestroy = Some(generator_vtab_disconnect::<Row>);
    module.xOpen = Some(generator_vtab_open::<Row>);
    module.xClose = Some(generator_vtab_close::<Row>);
    module.xFilter = Some(generator_vtab_filter::<Row>);
    module.xNext = Some(generator_vtab_next::<Row>);
    module.xEof = Some(generator_vtab_eof::<Row>);
    module.xColumn = Some(generator_vtab_column::<Row>);
    module.xRowid = Some(generator_vtab_rowid::<Row>);
    module.xUpdate = Some(generator_vtab_update::<Row>);
    module
}

unsafe extern "C" fn destroy_generator_module_state<Row>(ptr: *mut c_void) {
    unsafe {
        if !ptr.is_null() {
            drop(Box::from_raw(ptr.cast::<GeneratorModuleState<Row>>()));
        }
    }
}

unsafe extern "C" fn generator_vtab_connect<Row>(
    db: *mut ffi::sqlite3,
    p_aux: *mut c_void,
    _argc: c_int,
    _argv: *const *const c_char,
    pp_vtab: *mut *mut ffi::sqlite3_vtab,
    pz_err: *mut *mut c_char,
) -> c_int {
    unsafe {
        callback_status(pz_err, || {
            let state = p_aux
                .cast::<GeneratorModuleState<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator module state is null".to_string()))?;
            let schema = CString::new(state.def.schema())?;
            let rc = ffi::sqlite3_declare_vtab(db, schema.as_ptr());
            if !sqlite_ok(rc) {
                return Err(Error::sqlite(rc, crate::function::sqlite_error(db)));
            }
            let vtab = Box::new(GeneratorVtab {
                base: std::mem::zeroed(),
                def: state.def.clone(),
            });
            let vtab_ptr = Box::into_raw(vtab);
            *pp_vtab = &mut (*vtab_ptr).base;
            Ok(ffi::SQLITE_OK)
        })
    }
}

unsafe extern "C" fn generator_vtab_disconnect<Row>(p_vtab: *mut ffi::sqlite3_vtab) -> c_int {
    unsafe {
        if !p_vtab.is_null() {
            drop(Box::from_raw(p_vtab.cast::<GeneratorVtab<Row>>()));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn generator_vtab_open<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    pp_cursor: *mut *mut ffi::sqlite3_vtab_cursor,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            let cursor = Box::new(GeneratorCursor {
                base: std::mem::zeroed(),
                vtab: p_vtab,
                def: vtab.def.clone(),
                generator: None,
                iterator: None,
                eof: false,
            });
            let cursor_ptr = Box::into_raw(cursor);
            *pp_cursor = &mut (*cursor_ptr).base;
            Ok::<_, Error>(())
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

unsafe extern "C" fn generator_vtab_close<Row>(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        if !cursor.is_null() {
            drop(Box::from_raw(cursor.cast::<GeneratorCursor<Row>>()));
        }
        ffi::SQLITE_OK
    }
}

unsafe extern "C" fn generator_vtab_filter<Row>(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    idx_num: c_int,
    idx_str: *const c_char,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<GeneratorCursor<Row>>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql generator cursor is null".to_string()))?;
            cursor.generator = None;
            cursor.iterator = None;
            cursor.eof = false;

            if idx_num == MISSING_REQUIRED_CONSTRAINT {
                let message = c_string_to_string(idx_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "required constraint missing".to_string());
                set_vtab_error(cursor.vtab, &message);
                return Err(Error::Message(message));
            }

            if idx_num >= CONSTRAINT_FILTER_BASE
                && argc > 0
                && let Some(filter) = cursor
                    .def
                    .constraint_filters
                    .iter()
                    .find(|filter| filter.filter_id == idx_num)
            {
                let spec_indices = parse_constraint_index_list(idx_str);
                let mut args = Vec::new();
                for (arg_index, spec_index) in spec_indices.iter().copied().enumerate() {
                    let Some(spec) = filter.specs.get(spec_index) else {
                        continue;
                    };
                    if arg_index >= argc as usize {
                        break;
                    }
                    args.push(GeneratorConstraintArg {
                        column_index: spec.column_index,
                        op: spec.op,
                        value: FunctionArg::new(*argv.add(arg_index)),
                    });
                }
                let mut generator = (filter.factory)(&args);
                cursor.eof = !generator.next()?;
                cursor.generator = Some(generator);
                return Ok(());
            }

            if idx_num >= PARAMETRIC_FILTER_BASE
                && argc > 0
                && let Some(filter) = cursor
                    .def
                    .parametric_filters
                    .iter()
                    .find(|filter| filter.filter_id == idx_num)
            {
                let args = build_args(argc, argv);
                let mut generator = (filter.factory)(&args);
                cursor.eof = !generator.next()?;
                cursor.generator = Some(generator);
                return Ok(());
            }

            if idx_num != FILTER_NONE
                && argc > 0
                && let Some(filter) = cursor.def.find_filter_by_id(idx_num)
            {
                let arg = FunctionArg::new(*argv);
                let mut iterator = (filter.factory)(&arg);
                cursor.eof = !iterator.next()?;
                cursor.iterator = Some(iterator);
                return Ok(());
            }

            if let Some(message) = cursor.def.full_scan_error.as_ref() {
                set_vtab_error(cursor.vtab, message);
                return Err(Error::Message(message.clone()));
            }

            let mut generator = (cursor.def.generator)();
            cursor.eof = !generator.next()?;
            cursor.generator = Some(generator);
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn generator_vtab_next<Row>(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<GeneratorCursor<Row>>()
                .as_mut()
                .ok_or_else(|| Error::Message("xsql generator cursor is null".to_string()))?;
            if let Some(iterator) = cursor.iterator.as_mut() {
                cursor.eof = !iterator.next()?;
            } else if let Some(generator) = cursor.generator.as_mut() {
                cursor.eof = !generator.next()?;
            } else {
                cursor.eof = true;
            }
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn generator_vtab_eof<Row>(cursor: *mut ffi::sqlite3_vtab_cursor) -> c_int {
    unsafe {
        let Some(cursor) = cursor.cast::<GeneratorCursor<Row>>().as_ref() else {
            return 1;
        };
        i32::from(cursor.eof || (cursor.generator.is_none() && cursor.iterator.is_none()))
    }
}

unsafe extern "C" fn generator_vtab_column<Row>(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    ctx: *mut ffi::sqlite3_context,
    col: c_int,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            // During UPDATE, SQLite asks for unchanged column values. Generator
            // updates reconstruct via row_lookup (rowid key), so reporting NOCHANGE
            // is always safe. Returning without a value marks the column
            // SQLITE_NOCHANGE in xUpdate.
            if sqlite3_vtab_nochange(ctx) != 0 {
                return Ok(());
            }
            let cursor = cursor
                .cast::<GeneratorCursor<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator cursor is null".to_string()))?;
            let mut fctx = FunctionContext::new(ctx);
            if cursor.eof || col < 0 || col as usize >= cursor.def.columns.len() {
                fctx.result_null();
                return Ok(());
            }
            if let Some(iterator) = cursor.iterator.as_ref() {
                iterator.column(&mut fctx, col)?;
                return Ok(());
            }
            let Some(generator) = cursor.generator.as_ref() else {
                fctx.result_null();
                return Ok(());
            };
            (cursor.def.columns[col as usize].getter)(&mut fctx, generator.current());
            Ok(())
        }));
        match result {
            Ok(Ok(())) => ffi::SQLITE_OK,
            Ok(Err(err)) => {
                FunctionContext::new(ctx).result_error(err.to_string());
                ffi::SQLITE_ERROR
            }
            Err(_) => {
                FunctionContext::new(ctx).result_error("xsql generator table column panicked");
                ffi::SQLITE_ERROR
            }
        }
    }
}

unsafe extern "C" fn generator_vtab_rowid<Row>(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let cursor = cursor
                .cast::<GeneratorCursor<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator cursor is null".to_string()))?;
            if let Some(iterator) = cursor.iterator.as_ref() {
                *rowid = if cursor.eof { 0 } else { iterator.rowid() };
                return Ok(());
            }
            *rowid = cursor
                .generator
                .as_ref()
                .filter(|_| !cursor.eof)
                .map(|generator| generator.rowid())
                .unwrap_or(0);
            Ok(())
        }));
        finish_cursor_unit(cursor, result)
    }
}

unsafe extern "C" fn generator_vtab_update<Row: 'static>(
    p_vtab: *mut ffi::sqlite3_vtab,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
    rowid: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<c_int> {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            if argc <= 0 || argv.is_null() {
                return Ok(ffi::SQLITE_READONLY);
            }

            let old_rowid = FunctionArg::new(*argv);
            if argc == 1 && !old_rowid.is_null() {
                let Some(delete_row) = vtab.def.delete_row.as_ref() else {
                    return Ok(ffi::SQLITE_READONLY);
                };
                let Some(row_lookup) = vtab.def.row_lookup.as_ref() else {
                    return Ok(ffi::SQLITE_READONLY);
                };
                let Some(row) = row_lookup(old_rowid.as_i64()) else {
                    return Err(Error::Message(
                        "generator table row lookup failed".to_string(),
                    ));
                };
                let operation = format!("DELETE FROM {}", vtab.def.name);
                call_cached_modify_hook(&vtab.def.before_modify, &operation);
                if delete_row(&row) {
                    call_cached_modify_hook(&vtab.def.after_modify, &operation);
                    return Ok(ffi::SQLITE_OK);
                }
                return Err(Error::Message("generator table delete failed".to_string()));
            }

            if argc > 1 && !old_rowid.is_null() {
                if !vtab
                    .def
                    .columns
                    .iter()
                    .any(|column| column.setter.is_some())
                {
                    return Ok(ffi::SQLITE_READONLY);
                }
                let Some(row_lookup) = vtab.def.row_lookup.as_ref() else {
                    return Ok(ffi::SQLITE_READONLY);
                };
                let Some(mut row) = row_lookup(old_rowid.as_i64()) else {
                    return Err(Error::Message(
                        "generator table row lookup failed".to_string(),
                    ));
                };
                let operation = format!("UPDATE {}", vtab.def.name);
                call_cached_modify_hook(&vtab.def.before_modify, &operation);
                let args = build_args(argc - 2, argv.add(2));
                apply_generator_setters(&vtab.def, &mut row, &args)?;
                call_cached_modify_hook(&vtab.def.after_modify, &operation);
                return Ok(ffi::SQLITE_OK);
            }

            if argc > 1 && old_rowid.is_null() {
                let Some(insert_row) = vtab.def.insert_row.as_ref() else {
                    return Ok(ffi::SQLITE_READONLY);
                };
                let operation = format!("INSERT INTO {}", vtab.def.name);
                call_cached_modify_hook(&vtab.def.before_modify, &operation);
                let args = build_args(argc - 2, argv.add(2));
                if insert_row(&args) {
                    if !rowid.is_null() {
                        *rowid = 0;
                    }
                    call_cached_modify_hook(&vtab.def.after_modify, &operation);
                    return Ok(ffi::SQLITE_OK);
                }
                return Err(Error::Message("generator table insert failed".to_string()));
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
                set_vtab_error(p_vtab, "xsql generator table update panicked");
                ffi::SQLITE_ERROR
            }
        }
    }
}

unsafe extern "C" fn generator_vtab_best_index<Row>(
    p_vtab: *mut ffi::sqlite3_vtab,
    index_info: *mut ffi::sqlite3_index_info,
) -> c_int {
    unsafe {
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let vtab = p_vtab
                .cast::<GeneratorVtab<Row>>()
                .as_ref()
                .ok_or_else(|| Error::Message("xsql generator vtab is null".to_string()))?;
            let info = index_info
                .as_mut()
                .ok_or_else(|| Error::Message("sqlite index_info is null".to_string()))?;
            let mut best_constraint_filter: Option<&ConstraintFilterDef<Row>> = None;
            let mut best_matched_constraints = Vec::new();
            let mut best_consumes_order = false;
            let mut best_matched_count = -1;
            let mut missing_required_error = String::new();

            for filter in &vtab.def.constraint_filters {
                let mut matched_constraints = vec![-1; filter.specs.len()];
                let mut all_required_matched = true;
                let mut matched_count = 0;

                for (spec_index, spec) in filter.specs.iter().enumerate() {
                    let mut matched = None;
                    for i in 0..info.nConstraint {
                        let constraint = &*info.aConstraint.add(i as usize);
                        if constraint.usable != 0
                            && constraint.op as c_int == spec.op.sqlite_op()
                            && constraint.iColumn >= 0
                            && constraint.iColumn as usize == spec.column_index
                        {
                            matched = Some(i);
                            break;
                        }
                    }
                    if let Some(matched) = matched {
                        matched_constraints[spec_index] = matched;
                        matched_count += 1;
                    } else if spec.required {
                        all_required_matched = false;
                        if missing_required_error.is_empty() && !spec.missing_error.is_empty() {
                            missing_required_error = spec.missing_error.clone();
                        }
                        break;
                    }
                }

                if all_required_matched && matched_count > 0 {
                    let consumes_order = match filter.ordered_column {
                        Some(ordered_column) if info.nOrderBy == 1 => {
                            let order = &*info.aOrderBy;
                            order.iColumn >= 0
                                && order.iColumn as usize == ordered_column
                                && (order.desc != 0) == filter.ordered_desc
                        }
                        _ => false,
                    };

                    let replace = best_constraint_filter
                        .map(|best| {
                            filter.estimated_cost < best.estimated_cost
                                || (filter.estimated_cost == best.estimated_cost
                                    && consumes_order
                                    && !best_consumes_order)
                                || (filter.estimated_cost == best.estimated_cost
                                    && consumes_order == best_consumes_order
                                    && matched_count > best_matched_count)
                        })
                        .unwrap_or(true);
                    if replace {
                        best_constraint_filter = Some(filter);
                        best_matched_constraints = matched_constraints;
                        best_consumes_order = consumes_order;
                        best_matched_count = matched_count;
                    }
                }
            }

            if let Some(filter) = best_constraint_filter {
                let mut argv_index = 1;
                let mut idx_parts = Vec::new();
                for (spec_index, constraint_index) in best_matched_constraints.iter().enumerate() {
                    if *constraint_index < 0 {
                        continue;
                    }
                    let usage = &mut *info.aConstraintUsage.add(*constraint_index as usize);
                    usage.argvIndex = argv_index;
                    usage.omit = 1;
                    argv_index += 1;
                    idx_parts.push(spec_index.to_string());
                }
                if best_consumes_order {
                    info.orderByConsumed = 1;
                }
                let idx_str = sqlite_mprintf_string(&idx_parts.join(","));
                if !idx_str.is_null() {
                    info.idxStr = idx_str;
                    info.needToFreeIdxStr = 1;
                }
                info.idxNum = filter.filter_id;
                info.estimatedCost = filter.estimated_cost;
                info.estimatedRows = filter.estimated_rows;
                return Ok(());
            }

            for filter in &vtab.def.parametric_filters {
                let mut matched_constraints = Vec::with_capacity(filter.column_indices.len());
                let mut all_matched = true;
                for column_index in &filter.column_indices {
                    let mut matched = None;
                    for i in 0..info.nConstraint {
                        let constraint = &*info.aConstraint.add(i as usize);
                        if constraint.usable != 0
                            && constraint.op as i32 == ffi::SQLITE_INDEX_CONSTRAINT_EQ
                            && constraint.iColumn >= 0
                            && constraint.iColumn as usize == *column_index
                        {
                            matched = Some(i);
                            break;
                        }
                    }
                    if let Some(matched) = matched {
                        matched_constraints.push(matched);
                    } else {
                        all_matched = false;
                        break;
                    }
                }
                if all_matched {
                    for (arg_index, constraint_index) in matched_constraints.iter().enumerate() {
                        let usage = &mut *info.aConstraintUsage.add(*constraint_index as usize);
                        usage.argvIndex = (arg_index + 1) as c_int;
                        usage.omit = 1;
                    }
                    info.idxNum = filter.filter_id;
                    info.estimatedCost = filter.estimated_cost;
                    info.estimatedRows = filter.estimated_rows;
                    return Ok(());
                }
            }

            let mut best_filter: Option<&GeneratorFilterDef> = None;
            let mut best_constraint_index = -1;
            for i in 0..info.nConstraint {
                let constraint = &*info.aConstraint.add(i as usize);
                if constraint.usable == 0
                    || constraint.op as i32 != ffi::SQLITE_INDEX_CONSTRAINT_EQ
                    || constraint.iColumn < 0
                {
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
                return Ok(());
            }

            if !missing_required_error.is_empty() {
                let idx_str = sqlite_mprintf_string(&missing_required_error);
                if !idx_str.is_null() {
                    info.idxStr = idx_str;
                    info.needToFreeIdxStr = 1;
                }
                info.idxNum = MISSING_REQUIRED_CONSTRAINT;
                info.estimatedCost = 1.0;
                info.estimatedRows = 0;
                return Ok(());
            }

            let estimated_rows = vtab
                .def
                .estimate_rows
                .as_ref()
                .map(|estimate| estimate())
                .unwrap_or(1_000);
            info.idxNum = FILTER_NONE;
            info.estimatedCost = estimated_rows as f64;
            info.estimatedRows = estimated_rows as i64;
            Ok(())
        }));
        finish_vtab_unit(p_vtab, result)
    }
}

fn apply_generator_setters<'a, Row>(
    def: &GeneratorInner<Row>,
    row: &mut Row,
    args: &[FunctionArg<'a>],
) -> Result<()> {
    for (column_index, value) in args.iter().enumerate() {
        if value.is_nochange() {
            continue;
        }
        let Some(column) = def.columns.get(column_index) else {
            break;
        };
        if let Some(setter) = column.setter.as_ref()
            && !setter(row, value)
        {
            return Err(Error::Message(format!(
                "generator table update failed for column '{}'",
                column.name
            )));
        }
    }
    Ok(())
}
