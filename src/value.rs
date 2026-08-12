// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use libsqlite3_sys as ffi;
use std::ffi::CStr;
use std::marker::PhantomData;
use std::os::raw::c_int;
use std::os::raw::c_void;
use std::slice;

unsafe extern "C" {
    fn sqlite3_value_nochange(arg1: *mut ffi::sqlite3_value) -> c_int;
}

/// SQLite dynamic value kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueType {
    /// 64-bit signed integer (`SQLITE_INTEGER`).
    Integer,
    /// IEEE-754 double (`SQLITE_FLOAT`).
    Float,
    /// UTF text (`SQLITE_TEXT`).
    Text,
    /// Binary blob (`SQLITE_BLOB`).
    Blob,
    /// SQL NULL (`SQLITE_NULL`).
    Null,
    /// Any other / unrecognized SQLite type code, carried verbatim.
    Other(i32),
}

impl ValueType {
    pub(crate) fn from_sqlite(value_type: i32) -> Self {
        match value_type {
            ffi::SQLITE_INTEGER => Self::Integer,
            ffi::SQLITE_FLOAT => Self::Float,
            ffi::SQLITE_TEXT => Self::Text,
            ffi::SQLITE_BLOB => Self::Blob,
            ffi::SQLITE_NULL => Self::Null,
            other => Self::Other(other),
        }
    }
}

/// Owned SQLite value used outside callback lifetimes.
#[derive(Clone, Debug, PartialEq)]
pub enum OwnedValue {
    /// SQL NULL.
    Null,
    /// Owned 64-bit signed integer.
    Integer(i64),
    /// Owned IEEE-754 double.
    Real(f64),
    /// Owned UTF-8 text.
    Text(String),
    /// Owned binary blob.
    Blob(Vec<u8>),
}

/// Borrowed SQLite value valid for the current callback.
pub struct ValueRef<'a> {
    pub(crate) raw: *mut ffi::sqlite3_value,
    _marker: PhantomData<&'a ffi::sqlite3_value>,
}

// Keep ValueRef a pointer-sized borrowed SQLite value handle. Unlike the C++
// wrapper, Rust materializes argv into a Vec, but this invariant keeps the FFI
// boundary honest.
const _: () = {
    assert!(
        std::mem::size_of::<ValueRef<'static>>() == std::mem::size_of::<*mut ffi::sqlite3_value>()
    );
    assert!(
        std::mem::align_of::<ValueRef<'static>>()
            == std::mem::align_of::<*mut ffi::sqlite3_value>()
    );
};

impl<'a> ValueRef<'a> {
    pub(crate) fn new(raw: *mut ffi::sqlite3_value) -> Self {
        Self {
            raw,
            _marker: PhantomData,
        }
    }

    /// Returns the [`ValueType`] of this value.
    pub fn value_type(&self) -> ValueType {
        ValueType::from_sqlite(self.type_code())
    }

    /// Returns the raw SQLite type code (`sqlite3_value_type`); `SQLITE_NULL`
    /// when the underlying pointer is null.
    pub fn type_code(&self) -> i32 {
        if self.raw.is_null() {
            return ffi::SQLITE_NULL;
        }
        unsafe { ffi::sqlite3_value_type(self.raw) }
    }

    /// Reads the value as an `i64` (`sqlite3_value_int64`); `0` if null.
    pub fn as_i64(&self) -> i64 {
        if self.raw.is_null() {
            return 0;
        }
        unsafe { ffi::sqlite3_value_int64(self.raw) }
    }

    /// Reads the value as an `i32` (`sqlite3_value_int`); `0` if null.
    pub fn as_i32(&self) -> i32 {
        if self.raw.is_null() {
            return 0;
        }
        unsafe { ffi::sqlite3_value_int(self.raw) }
    }

    /// Reads the value as an `f64` (`sqlite3_value_double`); `0.0` if null.
    pub fn as_f64(&self) -> f64 {
        if self.raw.is_null() {
            return 0.0;
        }
        unsafe { ffi::sqlite3_value_double(self.raw) }
    }

    /// Text value as a Rust `String`. Matches the C++ reference's
    /// `std::string(sqlite3_value_text(..))`: truncate at the first embedded NUL
    /// (C-string semantics). Invalid UTF-8 in the prefix is lossily replaced —
    /// the one unavoidable deviation, since a Rust `String` must be valid UTF-8.
    /// Use [`as_text_bytes`](Self::as_text_bytes) / [`as_blob`](Self::as_blob) for
    /// the raw bytes.
    pub fn as_text(&self) -> String {
        let bytes = self.as_text_bytes();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }

    /// Returns the text as a borrowed [`CStr`] (`sqlite3_value_text`); `None`
    /// when the value or its text pointer is null.
    pub fn as_c_str(&self) -> Option<&'a CStr> {
        if self.raw.is_null() {
            return None;
        }
        let ptr = unsafe { ffi::sqlite3_value_text(self.raw) };
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(ptr.cast()) })
    }

    /// Returns the raw text bytes (`sqlite3_value_text` + `sqlite3_value_bytes`),
    /// without NUL truncation or UTF-8 validation; empty slice if null/empty.
    pub fn as_text_bytes(&self) -> &'a [u8] {
        if self.raw.is_null() {
            return &[];
        }
        let ptr = unsafe { ffi::sqlite3_value_text(self.raw) };
        let len = self.bytes();
        if ptr.is_null() || len <= 0 {
            return &[];
        }
        unsafe { slice::from_raw_parts(ptr, len as usize) }
    }

    /// Returns the blob bytes (`sqlite3_value_blob` + `sqlite3_value_bytes`);
    /// empty slice if null/empty.
    pub fn as_blob(&self) -> &'a [u8] {
        if self.raw.is_null() {
            return &[];
        }
        let ptr = unsafe { ffi::sqlite3_value_blob(self.raw) };
        let len = self.bytes();
        if ptr.is_null() || len <= 0 {
            return &[];
        }
        unsafe { slice::from_raw_parts(ptr.cast::<u8>(), len as usize) }
    }

    /// Returns the raw blob pointer (`sqlite3_value_blob`); null if the value
    /// is null.
    pub fn blob_ptr(&self) -> *const c_void {
        if self.raw.is_null() {
            return std::ptr::null();
        }
        unsafe { ffi::sqlite3_value_blob(self.raw) }
    }

    /// Returns the byte length of the text or blob (`sqlite3_value_bytes`);
    /// `0` if null.
    pub fn bytes(&self) -> i32 {
        if self.raw.is_null() {
            return 0;
        }
        unsafe { ffi::sqlite3_value_bytes(self.raw) }
    }

    /// Returns `true` if the value's type is [`ValueType::Null`].
    pub fn is_null(&self) -> bool {
        self.value_type() == ValueType::Null
    }

    /// Returns `true` if this is an unchanged column in an UPDATE
    /// (`sqlite3_value_nochange`); `false` if the value is null.
    pub fn is_nochange(&self) -> bool {
        if self.raw.is_null() {
            return false;
        }
        unsafe { sqlite3_value_nochange(self.raw) != 0 }
    }

    /// Copies the value into a lifetime-free [`OwnedValue`]; `Other`-typed
    /// values become [`OwnedValue::Null`].
    pub fn to_owned_value(&self) -> OwnedValue {
        match self.value_type() {
            ValueType::Integer => OwnedValue::Integer(self.as_i64()),
            ValueType::Float => OwnedValue::Real(self.as_f64()),
            ValueType::Text => OwnedValue::Text(self.as_text()),
            ValueType::Blob => OwnedValue::Blob(self.as_blob().to_vec()),
            ValueType::Null | ValueType::Other(_) => OwnedValue::Null,
        }
    }
}
