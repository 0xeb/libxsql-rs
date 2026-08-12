// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::error::{Error, Result, Status, sqlite_ok};
use crate::function::sqlite_error;
use crate::vtab::WriteSurfaceRegistry;
use libsqlite3_sys as ffi;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::rc::Rc;
use std::slice;

pub(crate) struct ConnectionInner {
    db: *mut ffi::sqlite3,
    // Per-connection write-surface registry read live by the prepare-time
    // authorizer (installed in `open_connection`). Behind a `Box` so its address
    // is pinned for the authorizer's `pArg` even if this struct is moved before
    // being placed in the `Rc`. Dropped with the connection, after `sqlite3_close`
    // has removed the authorizer.
    registry: Box<RefCell<WriteSurfaceRegistry>>,
}

impl ConnectionInner {
    pub(crate) fn raw(&self) -> *mut ffi::sqlite3 {
        self.db
    }

    pub(crate) fn write_surface_registry(&self) -> *const RefCell<WriteSurfaceRegistry> {
        &*self.registry
    }
}

impl Drop for ConnectionInner {
    fn drop(&mut self) {
        if !self.db.is_null() {
            unsafe {
                ffi::sqlite3_close(self.db);
            }
            self.db = std::ptr::null_mut();
        }
    }
}

/// Result of advancing a prepared statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepResult {
    /// A new result row is available for reading.
    Row,
    /// The statement finished executing with no more rows.
    Done,
    /// The database was busy or locked; the step did not complete.
    Busy,
    /// The step failed; see [`Statement::error`] for details.
    Error,
}

/// Owned prepared SQLite statement.
pub struct Statement {
    inner: Rc<ConnectionInner>,
    stmt: *mut ffi::sqlite3_stmt,
    error: String,
}

impl Statement {
    pub(crate) fn prepare(inner: Rc<ConnectionInner>, sql: &str) -> Result<Self> {
        let sql = CString::new(sql)?;
        let mut stmt = std::ptr::null_mut();
        // Clear any prior denial so the message reflects only THIS prepare; the
        // write-surface authorizer sets it during prepare_v2 on a denied write.
        crate::vtab::clear_authorizer_denial();
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(
                inner.raw(),
                sql.as_ptr(),
                -1,
                &mut stmt,
                std::ptr::null_mut(),
            )
        };
        if !sqlite_ok(rc) {
            // On an authorizer denial SQLite reports a fixed "not authorized";
            // substitute the capability-scoped message the authorizer stashed so
            // the caller sees WHY the write was refused (byte-identical to the
            // matched-row path). Mirrors the C++/C ports.
            let message = if rc == ffi::SQLITE_AUTH
                && let Some(denial) = crate::vtab::take_authorizer_denial()
            {
                denial
            } else {
                sqlite_error(inner.raw())
            };
            if !stmt.is_null() {
                unsafe { ffi::sqlite3_finalize(stmt) };
            }
            return Err(Error::sqlite(rc, message));
        }
        Ok(Self {
            inner,
            stmt,
            error: String::new(),
        })
    }

    /// Returns `true` while the statement holds a live SQLite handle.
    pub fn valid(&self) -> bool {
        !self.stmt.is_null()
    }

    /// Returns `true` if the statement makes no direct changes to the database.
    pub fn is_readonly(&self) -> bool {
        self.valid() && unsafe { ffi::sqlite3_stmt_readonly(self.stmt) != 0 }
    }

    /// Returns the message from the most recent failed operation, or empty on success.
    pub fn error(&self) -> &str {
        &self.error
    }

    /// Resets the statement back to its initial state, ready to be stepped again.
    ///
    /// Bound parameter values are preserved.
    pub fn reset(&mut self) -> Result<()> {
        self.require_valid()?;
        let rc = unsafe { ffi::sqlite3_reset(self.stmt) };
        self.set_error(rc)
    }

    /// Resets all bound parameters back to `NULL`.
    pub fn clear_bindings(&mut self) -> Result<()> {
        self.require_valid()?;
        let rc = unsafe { ffi::sqlite3_clear_bindings(self.stmt) };
        self.set_error(rc)
    }

    /// Binds `NULL` to the 1-based parameter at `index`.
    pub fn bind_null(&mut self, index: i32) -> Result<()> {
        self.require_valid()?;
        let rc = unsafe { ffi::sqlite3_bind_null(self.stmt, index) };
        self.set_error(rc)
    }

    /// Binds a 32-bit integer to the 1-based parameter at `index`.
    pub fn bind_int(&mut self, index: i32, value: i32) -> Result<()> {
        self.require_valid()?;
        let rc = unsafe { ffi::sqlite3_bind_int(self.stmt, index, value) };
        self.set_error(rc)
    }

    /// Binds a 64-bit integer to the 1-based parameter at `index`.
    ///
    /// Alias for [`Statement::bind_int64`].
    pub fn bind_i64(&mut self, index: i32, value: i64) -> Result<()> {
        self.bind_int64(index, value)
    }

    /// Binds a 64-bit integer to the 1-based parameter at `index`.
    pub fn bind_int64(&mut self, index: i32, value: i64) -> Result<()> {
        self.require_valid()?;
        let rc = unsafe { ffi::sqlite3_bind_int64(self.stmt, index, value) };
        self.set_error(rc)
    }

    /// Binds a floating-point value to the 1-based parameter at `index`.
    pub fn bind_double(&mut self, index: i32, value: f64) -> Result<()> {
        self.require_valid()?;
        let rc = unsafe { ffi::sqlite3_bind_double(self.stmt, index, value) };
        self.set_error(rc)
    }

    /// Binds a text value to the 1-based parameter at `index`.
    ///
    /// SQLite copies the string, so the source need not outlive the call.
    pub fn bind_text(&mut self, index: i32, value: impl AsRef<str>) -> Result<()> {
        self.require_valid()?;
        let value = CString::new(value.as_ref())?;
        let len = i32::try_from(value.as_bytes().len()).map_err(|_| {
            Error::sqlite(
                ffi::SQLITE_TOOBIG,
                "text too large to bind (exceeds i32::MAX bytes)",
            )
        })?;
        let rc = unsafe {
            ffi::sqlite3_bind_text(
                self.stmt,
                index,
                value.as_ptr(),
                len,
                ffi::SQLITE_TRANSIENT(),
            )
        };
        self.set_error(rc)
    }

    /// Binds a blob value to the 1-based parameter at `index`.
    ///
    /// SQLite copies the bytes, so `data` need not outlive the call.
    pub fn bind_blob(&mut self, index: i32, data: &[u8]) -> Result<()> {
        self.require_valid()?;
        let len = i32::try_from(data.len()).map_err(|_| {
            Error::sqlite(
                ffi::SQLITE_TOOBIG,
                "blob too large to bind (exceeds i32::MAX bytes)",
            )
        })?;
        let rc = unsafe {
            ffi::sqlite3_bind_blob(
                self.stmt,
                index,
                data.as_ptr().cast::<c_void>(),
                len,
                ffi::SQLITE_TRANSIENT(),
            )
        };
        self.set_error(rc)
    }

    /// Advances the statement by one step and reports the outcome.
    ///
    /// Returns [`StepResult::Error`] (and records an error message) if the
    /// statement is not valid.
    pub fn step(&mut self) -> StepResult {
        if !self.valid() {
            self.error = "Statement not valid".to_string();
            return StepResult::Error;
        }
        let rc = unsafe { ffi::sqlite3_step(self.stmt) };
        match Status::from_sqlite(rc) {
            Status::Row => {
                self.error.clear();
                StepResult::Row
            }
            Status::Done => {
                self.error.clear();
                StepResult::Done
            }
            Status::Busy | Status::Locked => {
                self.error = sqlite_error(self.inner.raw());
                StepResult::Busy
            }
            _ => {
                self.error = sqlite_error(self.inner.raw());
                StepResult::Error
            }
        }
    }

    /// Returns the number of columns in the current result row, or `0` if invalid.
    pub fn column_count(&self) -> i32 {
        if !self.valid() {
            return 0;
        }
        unsafe { ffi::sqlite3_column_count(self.stmt) }
    }

    /// Returns the name of the 0-based column `col`, or empty if unavailable.
    pub fn column_name(&self, col: i32) -> String {
        if !self.valid() {
            return String::new();
        }
        let name = unsafe { ffi::sqlite3_column_name(self.stmt, col) };
        if name.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    }

    /// Returns `true` if the 0-based column `col` holds `NULL` (or the statement is invalid).
    pub fn column_is_null(&self, col: i32) -> bool {
        !self.valid() || unsafe { ffi::sqlite3_column_type(self.stmt, col) == ffi::SQLITE_NULL }
    }

    /// Returns the SQLite datatype code of the 0-based column `col`.
    ///
    /// Returns `SQLITE_NULL` if the statement is not valid.
    pub fn column_type(&self, col: i32) -> i32 {
        if !self.valid() {
            return ffi::SQLITE_NULL;
        }
        unsafe { ffi::sqlite3_column_type(self.stmt, col) }
    }

    /// Reads the 0-based column `col` as a 32-bit integer, or `0` if invalid.
    pub fn int_value(&self, col: i32) -> i32 {
        if !self.valid() {
            return 0;
        }
        unsafe { ffi::sqlite3_column_int(self.stmt, col) }
    }

    /// Reads the 0-based column `col` as a 64-bit integer, or `0` if invalid.
    pub fn int64_value(&self, col: i32) -> i64 {
        if !self.valid() {
            return 0;
        }
        unsafe { ffi::sqlite3_column_int64(self.stmt, col) }
    }

    /// Reads the 0-based column `col` as a floating-point value, or `0.0` if invalid.
    pub fn double_value(&self, col: i32) -> f64 {
        if !self.valid() {
            return 0.0;
        }
        unsafe { ffi::sqlite3_column_double(self.stmt, col) }
    }

    /// Reads the 0-based column `col` as text (lossy UTF-8), or empty if null/invalid.
    pub fn text(&self, col: i32) -> String {
        if !self.valid() {
            return String::new();
        }
        let text = unsafe { ffi::sqlite3_column_text(self.stmt, col) };
        let len = self.bytes(col);
        if text.is_null() || len <= 0 {
            return String::new();
        }
        let bytes = unsafe { slice::from_raw_parts(text, len as usize) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Reads the 0-based column `col` as a blob, or an empty vector if null/invalid.
    pub fn blob(&self, col: i32) -> Vec<u8> {
        if !self.valid() {
            return Vec::new();
        }
        let data = unsafe { ffi::sqlite3_column_blob(self.stmt, col) };
        let len = self.bytes(col);
        if data.is_null() || len <= 0 {
            return Vec::new();
        }
        unsafe { slice::from_raw_parts(data.cast::<u8>(), len as usize) }.to_vec()
    }

    /// Returns the byte length of the text or blob in the 0-based column `col`.
    pub fn bytes(&self, col: i32) -> i32 {
        if !self.valid() {
            return 0;
        }
        unsafe { ffi::sqlite3_column_bytes(self.stmt, col) }
    }

    fn require_valid(&self) -> Result<()> {
        if self.valid() {
            Ok(())
        } else {
            Err(Error::sqlite(ffi::SQLITE_MISUSE, "Statement not valid"))
        }
    }

    fn set_error(&mut self, rc: i32) -> Result<()> {
        if sqlite_ok(rc) {
            self.error.clear();
            Ok(())
        } else {
            self.error = sqlite_error(self.inner.raw());
            Err(Error::sqlite(rc, self.error.clone()))
        }
    }
}

impl Drop for Statement {
    fn drop(&mut self) {
        if !self.stmt.is_null() {
            unsafe {
                ffi::sqlite3_finalize(self.stmt);
            }
            self.stmt = std::ptr::null_mut();
        }
    }
}

pub(crate) fn open_connection(path: &str) -> Result<Rc<ConnectionInner>> {
    let path = CString::new(path)?;
    let mut db = std::ptr::null_mut();
    let rc = unsafe { ffi::sqlite3_open(path.as_ptr(), &mut db) };
    if !sqlite_ok(rc) {
        let message = if db.is_null() {
            "Failed to allocate database".to_string()
        } else {
            sqlite_error(db)
        };
        if !db.is_null() {
            unsafe {
                ffi::sqlite3_close(db);
            }
        }
        return Err(Error::sqlite(rc, message));
    }
    // Install the write-surface authorizer. It reads the registry live at prepare
    // time (populated as tables are registered/created), so a write to an
    // unsupported surface is denied at prepare -- including the 0-row case that
    // never reaches xUpdate. The registry is pinned behind a `Box` so its address
    // stays valid for the authorizer's `pArg` for the connection's lifetime.
    let registry: Box<RefCell<WriteSurfaceRegistry>> = Box::default();
    unsafe {
        crate::vtab::install_authorizer(db, &*registry as *const RefCell<WriteSurfaceRegistry>);
    }
    Ok(Rc::new(ConnectionInner { db, registry }))
}
