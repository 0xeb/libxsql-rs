// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::error::{Error, Result, Status};
use crate::statement::StepResult;
use crate::value::{ValueRef, ValueType};
use libsqlite3_sys as ffi;
use std::ffi::{CStr, CString};
use std::marker::PhantomData;
use std::os::raw::{c_char, c_void};
use std::slice;

/// SQLite function argument wrapper.
pub struct FunctionArg<'a> {
    value: ValueRef<'a>,
}

// FunctionArg intentionally remains a pointer-sized wrapper over ValueRef.
// build_args() copies SQLite argv pointers into a Vec<FunctionArg>, so this is a
// wrapper invariant rather than C++-style argv aliasing.
const _: () = {
    assert!(
        std::mem::size_of::<FunctionArg<'static>>()
            == std::mem::size_of::<*mut ffi::sqlite3_value>()
    );
    assert!(
        std::mem::align_of::<FunctionArg<'static>>()
            == std::mem::align_of::<*mut ffi::sqlite3_value>()
    );
};

impl<'a> FunctionArg<'a> {
    pub(crate) fn new(raw: *mut ffi::sqlite3_value) -> Self {
        Self {
            value: ValueRef::new(raw),
        }
    }

    /// Borrows the underlying [`ValueRef`] for direct access to the raw value.
    pub fn value(&self) -> &ValueRef<'a> {
        &self.value
    }

    /// Returns the argument as an `i64` (via `sqlite3_value_int64`); `0` if null.
    pub fn as_i64(&self) -> i64 {
        self.value.as_i64()
    }

    /// C++-named alias for [`as_i64`](Self::as_i64).
    pub fn as_int64(&self) -> i64 {
        self.as_i64()
    }

    /// Returns the argument as an `i32` (via `sqlite3_value_int`); `0` if null.
    pub fn as_i32(&self) -> i32 {
        self.value.as_i32()
    }

    /// C++-named alias for [`as_i32`](Self::as_i32).
    pub fn as_int(&self) -> i32 {
        self.as_i32()
    }

    /// Returns the argument as an `f64` (via `sqlite3_value_double`); `0.0` if null.
    pub fn as_f64(&self) -> f64 {
        self.value.as_f64()
    }

    /// C++-named alias for [`as_f64`](Self::as_f64).
    pub fn as_double(&self) -> f64 {
        self.as_f64()
    }

    /// Returns the TEXT argument as a `String`, truncated at the first embedded
    /// NUL (C-string semantics) with invalid UTF-8 lossily replaced. Use
    /// [`as_text_bytes`](Self::as_text_bytes) for the raw bytes.
    pub fn as_text(&self) -> String {
        self.value.as_text()
    }

    /// Raw TEXT bytes (full `sqlite3_value_bytes` length, including any bytes past
    /// an embedded NUL). Use this when [`as_text`](Self::as_text)'s C-string
    /// truncation / lossy UTF-8 is not acceptable.
    pub fn as_text_bytes(&self) -> &'a [u8] {
        self.value.as_text_bytes()
    }

    /// Returns the TEXT argument as a borrowed `CStr`, or `None` if null.
    pub fn as_c_str(&self) -> Option<&'a CStr> {
        self.value.as_c_str()
    }

    /// Returns the BLOB argument as a borrowed byte slice; empty if null.
    pub fn as_blob(&self) -> &'a [u8] {
        self.value.as_blob()
    }

    /// Returns the byte length of the value (via `sqlite3_value_bytes`).
    pub fn bytes(&self) -> i32 {
        self.value.bytes()
    }

    /// Returns the SQLite storage class of the argument as a [`ValueType`].
    pub fn value_type(&self) -> ValueType {
        self.value.value_type()
    }

    /// C++-named alias for [`value_type`](Self::value_type).
    pub fn sqlite_type(&self) -> ValueType {
        self.value_type()
    }

    /// Returns the raw SQLite type code (`SQLITE_INTEGER`, `SQLITE_TEXT`, ...).
    pub fn r#type(&self) -> i32 {
        self.value.type_code()
    }

    /// Returns `true` if the argument is SQL NULL.
    pub fn is_null(&self) -> bool {
        self.value.is_null()
    }

    /// Returns `true` if the column is unchanged in an UPDATE (`sqlite3_value_nochange`).
    pub fn is_nochange(&self) -> bool {
        self.value.is_nochange()
    }
}

/// SQLite scalar function result context.
pub struct FunctionContext<'a> {
    pub(crate) raw: *mut ffi::sqlite3_context,
    _marker: PhantomData<&'a mut ffi::sqlite3_context>,
}

impl<'a> FunctionContext<'a> {
    pub(crate) fn new(raw: *mut ffi::sqlite3_context) -> Self {
        Self {
            raw,
            _marker: PhantomData,
        }
    }

    /// Sets the function result to an integer (via `sqlite3_result_int`).
    pub fn result_i32(&mut self, value: i32) {
        if self.raw.is_null() {
            return;
        }
        unsafe { ffi::sqlite3_result_int(self.raw, value) };
    }

    /// C++-named alias for [`result_i32`](Self::result_i32).
    pub fn result_int(&mut self, value: i32) {
        self.result_i32(value);
    }

    /// Sets the function result to a 64-bit integer (via `sqlite3_result_int64`).
    pub fn result_i64(&mut self, value: i64) {
        if self.raw.is_null() {
            return;
        }
        unsafe { ffi::sqlite3_result_int64(self.raw, value) };
    }

    /// C++-named alias for [`result_i64`](Self::result_i64).
    pub fn result_int64(&mut self, value: i64) {
        self.result_i64(value);
    }

    /// Sets the function result to a floating-point value (via `sqlite3_result_double`).
    pub fn result_f64(&mut self, value: f64) {
        if self.raw.is_null() {
            return;
        }
        unsafe { ffi::sqlite3_result_double(self.raw, value) };
    }

    /// C++-named alias for [`result_f64`](Self::result_f64).
    pub fn result_double(&mut self, value: f64) {
        self.result_f64(value);
    }

    /// Set a TEXT result. Matches the C++ reference's
    /// `sqlite3_result_text(ctx, val.c_str(), -1, ..)`: an embedded NUL truncates
    /// the value (C-string semantics) rather than being an error.
    pub fn result_text(&mut self, value: impl AsRef<str>) {
        if self.raw.is_null() {
            return;
        }
        let bytes = value.as_ref().as_bytes();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let Ok(len) = i32::try_from(end) else {
            unsafe { ffi::sqlite3_result_error_toobig(self.raw) };
            return;
        };
        unsafe {
            ffi::sqlite3_result_text(
                self.raw,
                bytes.as_ptr().cast::<c_char>(),
                len,
                ffi::SQLITE_TRANSIENT(),
            )
        };
    }

    /// Sets a TEXT result from a `'static` C string, passed without copying
    /// (a `NULL` destructor tells SQLite the bytes outlive the call).
    pub fn result_text_static(&mut self, value: &'static CStr) {
        if self.raw.is_null() {
            return;
        }
        let Ok(len) = i32::try_from(value.to_bytes().len()) else {
            unsafe { ffi::sqlite3_result_error_toobig(self.raw) };
            return;
        };
        unsafe { ffi::sqlite3_result_text(self.raw, value.as_ptr(), len, None) };
    }

    /// Sets the function result to a BLOB, copying `data` (via `sqlite3_result_blob`
    /// with `SQLITE_TRANSIENT`).
    pub fn result_blob(&mut self, data: &[u8]) {
        if self.raw.is_null() {
            return;
        }
        let Ok(len) = i32::try_from(data.len()) else {
            unsafe { ffi::sqlite3_result_error_toobig(self.raw) };
            return;
        };
        unsafe {
            ffi::sqlite3_result_blob(
                self.raw,
                data.as_ptr().cast::<c_void>(),
                len,
                ffi::SQLITE_TRANSIENT(),
            )
        };
    }

    /// Sets the function result to SQL NULL (via `sqlite3_result_null`).
    pub fn result_null(&mut self) {
        if self.raw.is_null() {
            return;
        }
        unsafe { ffi::sqlite3_result_null(self.raw) };
    }

    /// Reports a function error message (via `sqlite3_result_error`). Any embedded
    /// NUL in `message` is escaped to the literal `\0` so the full text survives.
    pub fn result_error(&mut self, message: impl AsRef<str>) {
        if self.raw.is_null() {
            return;
        }
        let sanitized = message.as_ref().replace('\0', "\\0");
        let c_message = CString::new(sanitized).expect("sanitized SQLite error contains no NUL");
        let Ok(len) = i32::try_from(c_message.as_bytes().len()) else {
            unsafe { ffi::sqlite3_result_error_toobig(self.raw) };
            return;
        };
        unsafe { ffi::sqlite3_result_error(self.raw, c_message.as_ptr(), len) };
    }

    /// Returns the most recent error message from this context's database handle
    /// (via `sqlite3_errmsg`); empty if unavailable.
    pub fn db_error(&self) -> String {
        if self.raw.is_null() {
            return String::new();
        }
        let db = unsafe { ffi::sqlite3_context_db_handle(self.raw) };
        if db.is_null() {
            return String::new();
        }
        let msg = unsafe { ffi::sqlite3_errmsg(db) };
        if msg.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    }

    /// Prepares and runs `sql` on this context's database, invoking `f` for each
    /// result row. The statement is finalized when the call returns. Returns an
    /// error if the database is not open, the SQL fails to prepare/step, or `f`
    /// returns an error.
    pub fn query_each<F>(&mut self, sql: &str, mut f: F) -> Result<()>
    where
        F: FnMut(QueryRow<'_>) -> Result<()>,
    {
        if self.raw.is_null() {
            return Err(Error::DatabaseNotOpen);
        }
        let db = unsafe { ffi::sqlite3_context_db_handle(self.raw) };
        if db.is_null() {
            return Err(Error::DatabaseNotOpen);
        }
        let sql = CString::new(sql)?;
        let mut stmt = std::ptr::null_mut();
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, std::ptr::null_mut())
        };
        if rc != ffi::SQLITE_OK {
            return Err(Error::sqlite(rc, sqlite_error(db)));
        }
        let _guard = StatementFinalizer(stmt);
        loop {
            let rc = unsafe { ffi::sqlite3_step(stmt) };
            match Status::from_sqlite(rc) {
                Status::Row => f(QueryRow::new(stmt))?,
                Status::Done => break,
                _ => return Err(Error::sqlite(rc, sqlite_error(db))),
            }
        }
        Ok(())
    }
}

/// Row view exposed while a scalar function queries the same database.
pub struct QueryRow<'a> {
    stmt: *mut ffi::sqlite3_stmt,
    _marker: PhantomData<&'a ffi::sqlite3_stmt>,
}

impl<'a> QueryRow<'a> {
    fn new(stmt: *mut ffi::sqlite3_stmt) -> Self {
        Self {
            stmt,
            _marker: PhantomData,
        }
    }

    /// Returns column `col` as a `String` (via `sqlite3_column_text`), with invalid
    /// UTF-8 lossily replaced; empty if NULL or out of range.
    pub fn text(&self, col: i32) -> String {
        if self.stmt.is_null() {
            return String::new();
        }
        let ptr = unsafe { ffi::sqlite3_column_text(self.stmt, col) };
        let len = self.bytes(col);
        if ptr.is_null() || len <= 0 {
            return String::new();
        }
        let bytes = unsafe { slice::from_raw_parts(ptr, len as usize) };
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Returns column `col` as an `i32` (via `sqlite3_column_int`); `0` if unavailable.
    pub fn int_value(&self, col: i32) -> i32 {
        if self.stmt.is_null() {
            return 0;
        }
        unsafe { ffi::sqlite3_column_int(self.stmt, col) }
    }

    /// Returns column `col` as an `i64` (via `sqlite3_column_int64`); `0` if unavailable.
    pub fn int64_value(&self, col: i32) -> i64 {
        if self.stmt.is_null() {
            return 0;
        }
        unsafe { ffi::sqlite3_column_int64(self.stmt, col) }
    }

    /// Returns column `col` as an `f64` (via `sqlite3_column_double`); `0.0` if unavailable.
    pub fn double_value(&self, col: i32) -> f64 {
        if self.stmt.is_null() {
            return 0.0;
        }
        unsafe { ffi::sqlite3_column_double(self.stmt, col) }
    }

    /// Returns `true` if column `col` is SQL NULL (or the row is unavailable).
    pub fn is_null(&self, col: i32) -> bool {
        self.stmt.is_null()
            || unsafe { ffi::sqlite3_column_type(self.stmt, col) == ffi::SQLITE_NULL }
    }

    /// Returns the byte length of column `col` (via `sqlite3_column_bytes`); `0` if unavailable.
    pub fn bytes(&self, col: i32) -> i32 {
        if self.stmt.is_null() {
            return 0;
        }
        unsafe { ffi::sqlite3_column_bytes(self.stmt, col) }
    }
}

pub(crate) fn build_args<'a>(
    argc: i32,
    argv: *mut *mut ffi::sqlite3_value,
) -> Vec<FunctionArg<'a>> {
    if argc <= 0 || argv.is_null() {
        return Vec::new();
    }
    let raw_args = unsafe { slice::from_raw_parts(argv, argc as usize) };
    raw_args.iter().map(|raw| FunctionArg::new(*raw)).collect()
}

pub(crate) fn sqlite_error(db: *mut ffi::sqlite3) -> String {
    if db.is_null() {
        return "Database not open".to_string();
    }
    let msg = unsafe { ffi::sqlite3_errmsg(db) };
    if msg.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(msg) }
        .to_string_lossy()
        .into_owned()
}

struct StatementFinalizer(*mut ffi::sqlite3_stmt);

impl Drop for StatementFinalizer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::sqlite3_finalize(self.0) };
        }
    }
}

impl From<StepResult> for Status {
    fn from(value: StepResult) -> Self {
        match value {
            StepResult::Row => Status::Row,
            StepResult::Done => Status::Done,
            StepResult::Busy => Status::Busy,
            StepResult::Error => Status::Error,
        }
    }
}
