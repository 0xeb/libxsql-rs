// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::error::{Error, Result};
use crate::function::{FunctionArg, FunctionContext};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::os::raw::c_int;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

pub(crate) const FILTER_NONE: c_int = 0;

thread_local! {
    static VTAB_INTERRUPT_CHECKER: RefCell<Option<VtabInterruptChecker>> = RefCell::new(None);
    static VTAB_CALLBACK_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) type VtabInterruptChecker = Box<dyn Fn() -> bool>;

pub(crate) fn replace_vtab_interrupt_checker(
    checker: Option<VtabInterruptChecker>,
) -> Option<VtabInterruptChecker> {
    VTAB_INTERRUPT_CHECKER.with(|slot| slot.replace(checker))
}

/// Set a detailed error for the current virtual-table mutation callback.
///
/// A writable column setter, insert, or delete callback still returns `bool`;
/// returning `false` after calling this function makes SQLite surface this
/// message instead of the generic failure diagnostic.
pub fn set_vtab_error(message: impl Into<String>) {
    VTAB_CALLBACK_ERROR.with(|slot| {
        *slot.borrow_mut() = Some(message.into());
    });
}

fn take_vtab_callback_error() -> Option<String> {
    VTAB_CALLBACK_ERROR.with(|slot| slot.borrow_mut().take())
}

/// Drop any reason left by an earlier callback, before invoking an insert or
/// delete callback.
pub(crate) fn begin_vtab_callback() {
    let _ = take_vtab_callback_error();
}

/// The error for an insert or delete callback that returned `false`: the reason
/// it set with [`set_vtab_error`], else `fallback`.
pub(crate) fn vtab_callback_failure(fallback: &str) -> Error {
    Error::Message(take_vtab_callback_error().unwrap_or_else(|| fallback.to_string()))
}

/// Returns true if the thread-local interrupt checker reports that the current
/// virtual-table scan should be aborted; returns false when no checker is set.
pub fn vtab_interrupted() -> bool {
    VTAB_INTERRUPT_CHECKER.with(|checker| {
        checker
            .borrow()
            .as_ref()
            .map(|checker| catch_unwind(AssertUnwindSafe(checker)).unwrap_or(true))
            .unwrap_or(false)
    })
}

/// Install a thread-local cooperative-cancellation predicate that long-running
/// virtual-table scans poll via [`vtab_interrupted`].
///
/// Embedders can wire their own "user cancelled" signal into vtab loops.
/// [`Database::query_with_options`](crate::Database::query_with_options)
/// temporarily installs its own deadline/cancellation checker and restores this
/// outer predicate when the query finishes.
pub fn set_vtab_interrupt_checker<F>(checker: F)
where
    F: Fn() -> bool + 'static,
{
    let _ = replace_vtab_interrupt_checker(Some(Box::new(checker)));
}

/// Apply an UPDATE's column setters, shared by the cached / generator / index
/// modules. A write to a read-only column is rejected (`column "<name>" is
/// read-only`) rather than silently dropped -- SQLite marks unchanged columns
/// NOCHANGE, so a non-NOCHANGE value on a non-writable column is a real write. The
/// rejection is atomic (nothing applies if any target is read-only).
/// `nochange_eligible` must be false on reconstruct-by-argv paths, where unchanged
/// read-only identity columns carry their real value in argv and are skipped, not
/// rejected. Setters apply in order and can't be rolled back, so a later failure
/// names the columns already applied.
pub(crate) fn apply_update_columns<'a>(
    args: &[FunctionArg<'a>],
    ncols: usize,
    nochange_eligible: bool,
    is_writable: impl Fn(usize) -> bool,
    col_name: impl Fn(usize) -> String,
    mut apply: impl FnMut(usize, &FunctionArg<'a>) -> bool,
) -> Result<()> {
    let has_writable = (0..ncols).any(&is_writable);

    // Reject a read-only write before mutating anything. Skipped on reconstruct-by-
    // argv paths, where unchanged read-only columns arrive as real values.
    if nochange_eligible && has_writable {
        for (i, value) in args.iter().enumerate().take(ncols) {
            if value.is_nochange() {
                continue;
            }
            if !is_writable(i) {
                return Err(Error::Message(format!(
                    "column \"{}\" is read-only",
                    col_name(i)
                )));
            }
        }
    }

    // Apply writable setters in order, tracking which applied so a later failure can
    // name the columns that were already committed.
    let mut applied: Vec<String> = Vec::new();
    for (i, value) in args.iter().enumerate().take(ncols) {
        if value.is_nochange() {
            continue;
        }
        if !is_writable(i) {
            continue; // read-only on a non-eligible path: skip (historical behavior)
        }
        let _ = take_vtab_callback_error();
        if !apply(i, value) {
            if let Some(message) = take_vtab_callback_error() {
                return Err(Error::Message(message));
            }
            let mut msg = format!("UPDATE setter failed for column \"{}\"", col_name(i));
            if !applied.is_empty() {
                msg.push_str(&format!(
                    " (partial UPDATE: column(s) [{}] were already applied and cannot be rolled back)",
                    applied.join(", ")
                ));
            }
            return Err(Error::Message(msg));
        }
        applied.push(col_name(i));
    }
    Ok(())
}

/// Remove the thread-local interrupt checker installed by
/// [`set_vtab_interrupt_checker`]; [`vtab_interrupted`] then reports `false`.
pub fn clear_vtab_interrupt_checker() {
    let _ = replace_vtab_interrupt_checker(None);
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

/// Opaque, registration-local state passed to virtual-table transaction hooks.
///
/// A fresh value is created for every SQLite virtual-table connection, so a
/// definition can safely be registered with more than one [`Database`](crate::Database).
/// Consumers normally store a `RefCell<T>` or another interior-mutable value and
/// recover it with `Any::downcast_ref`.
pub type TransactionState = Rc<dyn Any>;

/// Factory for one connection's transaction-hook state.
pub type TransactionStateFactory = dyn Fn() -> Result<TransactionState> + 'static;
/// Fallible prepare callback, invoked from SQLite `xSync`.
pub type TransactionPrepareHook = dyn Fn(&TransactionState) -> Result<()> + 'static;
/// Infallible final callback, invoked from SQLite `xCommit` or `xRollback`.
pub type TransactionFinalHook = dyn Fn(&TransactionState) + 'static;
/// Fallible savepoint callback.
pub type TransactionSavepointHook = dyn Fn(&TransactionState, i32) -> Result<()> + 'static;

/// Complete SQLite virtual-table transaction lifecycle.
///
/// `prepare_commit` is the only fallible commit phase because SQLite propagates
/// `xSync` failures but cannot reliably recover from an `xCommit` failure.
/// `commit` and `rollback` therefore have infallible signatures; panics are
/// contained at the FFI boundary. Hooks run only for a transaction that touched
/// the table, and `prepare_commit`/`commit` require at least one successful write.
#[derive(Clone, Default)]
pub struct TransactionHooks {
    /// Create connection-local state. The default state is `()`.
    pub state_factory: Option<Rc<TransactionStateFactory>>,
    /// Validate/finalize fallible work before SQLite commits.
    pub prepare_commit: Option<Rc<TransactionPrepareHook>>,
    /// Publish already-prepared work after SQLite commits.
    pub commit: Option<Rc<TransactionFinalHook>>,
    /// Discard touched external state after SQLite rolls back.
    pub rollback: Option<Rc<TransactionFinalHook>>,
    /// Snapshot external state for a SQLite savepoint.
    pub savepoint: Option<Rc<TransactionSavepointHook>>,
    /// Release a SQLite savepoint.
    pub release: Option<Rc<TransactionSavepointHook>>,
    /// Restore external state to a SQLite savepoint.
    pub rollback_to: Option<Rc<TransactionSavepointHook>>,
}

#[derive(Clone, Copy)]
struct TransactionSnapshot {
    id: i32,
    touched: bool,
    wrote: bool,
    prepared: bool,
}

pub(crate) struct TransactionLifecycle {
    state: TransactionState,
    touched: Cell<bool>,
    wrote: Cell<bool>,
    prepared: Cell<bool>,
    savepoints: RefCell<Vec<TransactionSnapshot>>,
}

impl TransactionLifecycle {
    pub(crate) fn new(hooks: &TransactionHooks) -> Result<Self> {
        let state = match hooks.state_factory.as_deref() {
            Some(factory) => factory()?,
            None => Rc::new(()),
        };
        Ok(Self {
            state,
            touched: Cell::new(false),
            wrote: Cell::new(false),
            prepared: Cell::new(false),
            savepoints: RefCell::new(Vec::new()),
        })
    }

    pub(crate) fn begin(&self) {
        self.touched.set(false);
        self.wrote.set(false);
        self.prepared.set(false);
        self.savepoints.borrow_mut().clear();
    }

    pub(crate) fn state(&self) -> TransactionState {
        self.state.clone()
    }

    /// Mark the table touched immediately before invoking any mutation callback.
    pub(crate) fn touch(&self) {
        self.touched.set(true);
    }

    /// Mark a mutation successful. A later write after `xSync` requires prepare again.
    pub(crate) fn mark_written(&self) {
        self.wrote.set(true);
        self.prepared.set(false);
    }

    pub(crate) fn sync(&self, hooks: &TransactionHooks) -> Result<()> {
        if !self.wrote.get() || self.prepared.get() {
            return Ok(());
        }
        if let Some(prepare) = hooks.prepare_commit.as_deref() {
            prepare(&self.state)?;
        }
        self.prepared.set(true);
        Ok(())
    }

    pub(crate) fn commit(&self, hooks: &TransactionHooks) {
        let wrote = self.wrote.get();
        self.clear();
        if wrote && let Some(commit) = hooks.commit.as_deref() {
            commit(&self.state);
        }
    }

    pub(crate) fn rollback(&self, hooks: &TransactionHooks) {
        let touched = self.touched.get();
        self.clear();
        if touched && let Some(rollback) = hooks.rollback.as_deref() {
            rollback(&self.state);
        }
    }

    pub(crate) fn savepoint(&self, hooks: &TransactionHooks, id: i32) -> Result<()> {
        let snapshot = TransactionSnapshot {
            id,
            touched: self.touched.get(),
            wrote: self.wrote.get(),
            prepared: self.prepared.get(),
        };
        if let Some(savepoint) = hooks.savepoint.as_deref() {
            savepoint(&self.state, id)?;
        }
        // Publish internal state only after the fallible external hook succeeds.
        // Otherwise a returned error (or an FFI-contained panic) leaves a
        // phantom savepoint that a later rollback can incorrectly restore.
        self.savepoints.borrow_mut().push(snapshot);
        Ok(())
    }

    pub(crate) fn release(&self, hooks: &TransactionHooks, id: i32) -> Result<()> {
        if let Some(release) = hooks.release.as_deref() {
            release(&self.state, id)?;
        }
        self.savepoints
            .borrow_mut()
            .retain(|snapshot| snapshot.id < id);
        Ok(())
    }

    pub(crate) fn rollback_to(&self, hooks: &TransactionHooks, id: i32) -> Result<()> {
        if let Some(rollback_to) = hooks.rollback_to.as_deref() {
            rollback_to(&self.state, id)?;
        }
        let snapshot = self
            .savepoints
            .borrow()
            .iter()
            .rev()
            .find(|snapshot| snapshot.id == id)
            .copied()
            .unwrap_or(TransactionSnapshot {
                id,
                touched: false,
                wrote: false,
                prepared: false,
            });
        self.touched.set(snapshot.touched);
        self.wrote.set(snapshot.wrote);
        self.prepared.set(snapshot.prepared);
        self.savepoints.borrow_mut().retain(|saved| saved.id <= id);
        Ok(())
    }

    fn clear(&self) {
        self.touched.set(false);
        self.wrote.set(false);
        self.prepared.set(false);
        self.savepoints.borrow_mut().clear();
    }
}

#[cfg(test)]
mod transaction_lifecycle_tests {
    use super::{TransactionHooks, TransactionLifecycle};
    use crate::{Error, Result};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::Rc;

    #[test]
    fn failed_savepoint_hook_does_not_publish_internal_snapshot() -> Result<()> {
        let hooks = TransactionHooks {
            savepoint: Some(Rc::new(|_, _| {
                Err(Error::Message("savepoint rejected".to_string()))
            })),
            ..Default::default()
        };
        let lifecycle = TransactionLifecycle::new(&hooks)?;
        lifecycle.begin();
        lifecycle.touch();
        lifecycle.mark_written();
        assert!(lifecycle.savepoint(&hooks, 7).is_err());
        assert!(lifecycle.savepoints.borrow().is_empty());
        Ok(())
    }

    #[test]
    fn panicking_savepoint_hook_does_not_publish_internal_snapshot() -> Result<()> {
        let hooks = TransactionHooks {
            savepoint: Some(Rc::new(|_, _| panic!("savepoint panic"))),
            ..Default::default()
        };
        let lifecycle = TransactionLifecycle::new(&hooks)?;
        lifecycle.begin();
        lifecycle.touch();
        lifecycle.mark_written();
        assert!(catch_unwind(AssertUnwindSafe(|| lifecycle.savepoint(&hooks, 9))).is_err());
        assert!(lifecycle.savepoints.borrow().is_empty());
        Ok(())
    }
}

mod cached;
mod column_helpers;
mod ffi;
mod generator;
mod index;
mod write_surface;

pub use cached::{CachedTableBuilder, CachedTableDef, cached_table};
pub use generator::{GeneratorTableBuilder, GeneratorTableDef, generator_table};
pub use index::{TableBuilder, TableDef, table};

pub(crate) use cached::register_cached_table;
pub(crate) use generator::register_generator_table;
pub(crate) use index::register_index_table;
pub(crate) use write_surface::{
    WriteCaps, WriteSurfaceRegistry, clear_authorizer_denial, connect_write_surface,
    destroy_write_surface, install_authorizer, take_authorizer_denial,
};
