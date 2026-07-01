// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::error::Result;
use crate::function::{FunctionArg, FunctionContext};
use std::cell::RefCell;
use std::os::raw::c_int;

pub(crate) const FILTER_NONE: c_int = 0;

thread_local! {
    static VTAB_INTERRUPT_CHECKER: RefCell<Option<Box<dyn Fn() -> bool>>> = RefCell::new(None);
}

/// Returns true if the thread-local interrupt checker reports that the current
/// virtual-table scan should be aborted; returns false when no checker is set.
pub fn vtab_interrupted() -> bool {
    VTAB_INTERRUPT_CHECKER.with(|checker| {
        checker
            .borrow()
            .as_ref()
            .map(|checker| checker())
            .unwrap_or(false)
    })
}

/// Install a thread-local cooperative-cancellation predicate that long-running
/// virtual-table scans poll via [`vtab_interrupted`].
///
/// Embedders can wire their own "user cancelled" signal into vtab loops. Note
/// that [`Database::query`](crate::Database::query) with a timeout temporarily
/// installs its own deadline-based checker for the duration of the query and
/// clears it afterward, so set a custom checker for use outside such a query.
pub fn set_vtab_interrupt_checker<F>(checker: F)
where
    F: Fn() -> bool + 'static,
{
    VTAB_INTERRUPT_CHECKER.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(checker));
    });
}

/// Remove the thread-local interrupt checker installed by
/// [`set_vtab_interrupt_checker`]; [`vtab_interrupted`] then reports `false`.
pub fn clear_vtab_interrupt_checker() {
    VTAB_INTERRUPT_CHECKER.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

/// SQLite column affinity used by virtual table builders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColumnType {
    /// SQLite `INTEGER` affinity.
    Integer,
    /// SQLite `TEXT` affinity.
    Text,
    /// SQLite `REAL` affinity.
    Real,
    /// SQLite `BLOB` affinity.
    Blob,
}

impl ColumnType {
    fn sql(self) -> &'static str {
        match self {
            Self::Integer => "INTEGER",
            Self::Text => "TEXT",
            Self::Real => "REAL",
            Self::Blob => "BLOB",
        }
    }
}

/// Optimized row source used for virtual table constraint pushdown.
pub trait RowIterator {
    /// Advance to the next row; returns `Ok(false)` when the iterator is exhausted.
    fn next(&mut self) -> Result<bool>;
    /// Returns true when the iterator has no current row.
    fn eof(&self) -> bool;
    /// Emit the value of column `col` for the current row into `ctx`.
    fn column(&self, ctx: &mut FunctionContext<'_>, col: i32) -> Result<()>;
    /// Returns the rowid of the current row.
    fn rowid(&self) -> i64;
}

/// Streaming row source used by generator virtual tables.
pub trait Generator<Row> {
    /// Advance to the next row; returns `Ok(false)` when the stream is exhausted.
    fn next(&mut self) -> Result<bool>;
    /// Returns a reference to the current row.
    fn current(&self) -> &Row;
    /// Returns the rowid of the current row.
    fn rowid(&self) -> i64;
}

/// SQLite constraint operation requested from a generator filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConstraintOp {
    /// Equality (`=`) constraint.
    Eq,
    /// Greater-than (`>`) constraint.
    Gt,
    /// Less-than-or-equal (`<=`) constraint.
    Le,
    /// Less-than (`<`) constraint.
    Lt,
    /// Greater-than-or-equal (`>=`) constraint.
    Ge,
    /// `LIKE` pattern constraint.
    Like,
}

impl ConstraintOp {
    fn sqlite_op(self) -> c_int {
        match self {
            Self::Eq => ffi::SQLITE_INDEX_CONSTRAINT_EQ,
            Self::Gt => ffi::SQLITE_INDEX_CONSTRAINT_GT,
            Self::Le => ffi::SQLITE_INDEX_CONSTRAINT_LE,
            Self::Lt => ffi::SQLITE_INDEX_CONSTRAINT_LT,
            Self::Ge => ffi::SQLITE_INDEX_CONSTRAINT_GE,
            Self::Like => ffi::SQLITE_INDEX_CONSTRAINT_LIKE,
        }
    }
}

/// Required or optional constraint specification for generator filters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConstraintRequest {
    column_name: String,
    op: ConstraintOp,
    required: bool,
    missing_error: String,
}

impl ConstraintRequest {
    /// Build a required constraint on `column_name` for `op`; when the planner
    /// cannot satisfy it the query fails with `missing_error`.
    pub fn required(
        column_name: impl Into<String>,
        op: ConstraintOp,
        missing_error: impl Into<String>,
    ) -> Self {
        Self {
            column_name: column_name.into(),
            op,
            required: true,
            missing_error: missing_error.into(),
        }
    }

    /// Build an optional constraint on `column_name` for `op` that the planner
    /// uses when present.
    pub fn optional(column_name: impl Into<String>, op: ConstraintOp) -> Self {
        Self {
            column_name: column_name.into(),
            op,
            required: false,
            missing_error: String::new(),
        }
    }
}

/// Shorthand for a required equality constraint on `column_name`.
pub fn required_eq(
    column_name: impl Into<String>,
    missing_error: impl Into<String>,
) -> ConstraintRequest {
    ConstraintRequest::required(column_name, ConstraintOp::Eq, missing_error)
}

/// Shorthand for a required `LIKE` constraint on `column_name`.
pub fn required_like(
    column_name: impl Into<String>,
    missing_error: impl Into<String>,
) -> ConstraintRequest {
    ConstraintRequest::required(column_name, ConstraintOp::Like, missing_error)
}

/// Shorthand for an optional equality constraint on `column_name`.
pub fn optional_eq(column_name: impl Into<String>) -> ConstraintRequest {
    ConstraintRequest::optional(column_name, ConstraintOp::Eq)
}

/// Shorthand for an optional `LIKE` constraint on `column_name`.
pub fn optional_like(column_name: impl Into<String>) -> ConstraintRequest {
    ConstraintRequest::optional(column_name, ConstraintOp::Like)
}

/// Shorthand for an optional greater-than constraint on `column_name`.
pub fn optional_gt(column_name: impl Into<String>) -> ConstraintRequest {
    ConstraintRequest::optional(column_name, ConstraintOp::Gt)
}

/// Shorthand for an optional greater-than-or-equal constraint on `column_name`.
pub fn optional_ge(column_name: impl Into<String>) -> ConstraintRequest {
    ConstraintRequest::optional(column_name, ConstraintOp::Ge)
}

/// Shorthand for an optional less-than constraint on `column_name`.
pub fn optional_lt(column_name: impl Into<String>) -> ConstraintRequest {
    ConstraintRequest::optional(column_name, ConstraintOp::Lt)
}

/// Shorthand for an optional less-than-or-equal constraint on `column_name`.
pub fn optional_le(column_name: impl Into<String>) -> ConstraintRequest {
    ConstraintRequest::optional(column_name, ConstraintOp::Le)
}

/// Constraint value passed to a generator constraint-filter callback.
pub struct GeneratorConstraintArg<'a> {
    /// Index of the constrained column.
    pub column_index: usize,
    /// The constraint operation that matched.
    pub op: ConstraintOp,
    /// The right-hand-side value of the constraint.
    pub value: FunctionArg<'a>,
}

pub(crate) type ModifyHook = dyn Fn(&str) + 'static;

mod cached;
mod ffi;
mod generator;
mod index;

pub use cached::{CachedTableBuilder, CachedTableDef, cached_table};
pub use generator::{GeneratorTableBuilder, GeneratorTableDef, generator_table};
pub use index::{TableBuilder, TableDef, table};

pub(crate) use cached::register_cached_table;
pub(crate) use generator::register_generator_table;
pub(crate) use index::register_index_table;
