// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::error::Result;
use libsqlite3_sys as ffi;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::ModifyHook;

// Re-exported so `super` (the `vtab` root) can name these as `ffi::SQLITE_...`
// from `ConstraintOp::sqlite_op` without aliasing the crate to the `ffi` name
// (which would collide with this private `ffi` submodule).
pub(crate) use libsqlite3_sys::{
    SQLITE_INDEX_CONSTRAINT_EQ, SQLITE_INDEX_CONSTRAINT_GE, SQLITE_INDEX_CONSTRAINT_GT,
    SQLITE_INDEX_CONSTRAINT_LE, SQLITE_INDEX_CONSTRAINT_LIKE, SQLITE_INDEX_CONSTRAINT_LT,
};

unsafe extern "C" {
    pub(crate) fn sqlite3_vtab_nochange(arg1: *mut ffi::sqlite3_context) -> c_int;
    fn sqlite3_vtab_rhs_value(
        info: *mut ffi::sqlite3_index_info,
        constraint: c_int,
        value: *mut *mut ffi::sqlite3_value,
    ) -> c_int;
}

/// Decide whether a LIKE/GLOB constraint is worth routing to a prefix filter:
/// true iff the pattern has a usable literal prefix (its first character is not
/// a wildcard/escape). If the right-hand side is not available at plan time
/// (e.g. a bound parameter), return true and let the iterator extract the prefix
/// at filter time -- always a correct superset. Mirrors the C++
/// `like_constraint_has_usable_prefix`.
pub(crate) unsafe fn like_constraint_has_usable_prefix(
    info: *mut ffi::sqlite3_index_info,
    constraint: c_int,
) -> bool {
    unsafe {
        let mut rhs: *mut ffi::sqlite3_value = std::ptr::null_mut();
        let rc = sqlite3_vtab_rhs_value(info, constraint, &mut rhs);
        if rc != ffi::SQLITE_OK || rhs.is_null() {
            return true;
        }
        let text = ffi::sqlite3_value_text(rhs);
        if text.is_null() {
            return false;
        }
        let c0 = *text;
        c0 != 0 && c0 != b'%' && c0 != b'_' && c0 != b'\\'
    }
}

pub(crate) fn callback_status(
    pz_err: *mut *mut c_char,
    f: impl FnOnce() -> Result<c_int>,
) -> c_int {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(status)) => status,
        Ok(Err(err)) => {
            set_error_message(pz_err, &err.to_string());
            ffi::SQLITE_ERROR
        }
        Err(_) => {
            set_error_message(pz_err, "xsql virtual table callback panicked");
            ffi::SQLITE_ERROR
        }
    }
}

pub(crate) fn call_cached_modify_hook(hook: &Option<Box<ModifyHook>>, operation: &str) {
    if let Some(hook) = hook.as_ref() {
        hook(operation);
    }
}

pub(crate) fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn set_error_message(target: *mut *mut c_char, message: &str) {
    if target.is_null() {
        return;
    }
    let format = CString::new("%s").expect("literal has no NUL");
    let sanitized = message.replace('\0', "\\0");
    let message = CString::new(sanitized).expect("sanitized message has no NUL");
    unsafe {
        *target = ffi::sqlite3_mprintf(format.as_ptr(), message.as_ptr());
    }
}

pub(crate) fn sqlite_mprintf_string(message: &str) -> *mut c_char {
    let format = CString::new("%s").expect("literal has no NUL");
    let sanitized = message.replace('\0', "\\0");
    let message = CString::new(sanitized).expect("sanitized message has no NUL");
    unsafe { ffi::sqlite3_mprintf(format.as_ptr(), message.as_ptr()) }
}

pub(crate) fn parse_col_used(idx_str: *const c_char) -> u64 {
    if idx_str.is_null() {
        return u64::MAX;
    }
    unsafe { CStr::from_ptr(idx_str) }
        .to_string_lossy()
        .parse::<u64>()
        .unwrap_or(u64::MAX)
}

pub(crate) fn parse_constraint_index_list(idx_str: *const c_char) -> Vec<usize> {
    c_string_to_string(idx_str)
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.parse::<usize>().ok())
        .collect()
}

pub(crate) fn c_string_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

pub(crate) unsafe fn set_vtab_error(p_vtab: *mut ffi::sqlite3_vtab, message: &str) {
    unsafe {
        if p_vtab.is_null() {
            return;
        }
        set_error_message(&mut (*p_vtab).zErrMsg, message);
    }
}

// --- Capability-scoped errors for unsupported write surfaces -----------------
//
// A write to a surface/column that has no mutation support must NOT surface as
// SQLite's generic "attempt to write a readonly database" -- that implies the
// whole database is read-only when in fact it is writable and only THIS surface
// is not. Compose an actionable message naming the surface and the missing
// capability (`mutation.<table>.<leaf>`, the convention every tool uses in its
// `capabilities` table) and return `SQLITE_ERROR` (not `SQLITE_READONLY`) with
// the message set on the vtab. This is the matched-row half of the fix; the
// prepare-time authorizer (see [`super::write_surface`]) produces the identical
// message for the 0-row case so both report consistently.
//
// The composed strings MUST stay byte-identical to the C++ (`detail::unsupported_*`)
// and C (`xsqlc_unsupported_write`) ports and to the authorizer below.
pub(crate) fn unsupported_write_message(verb: &str, table: &str, leaf: &str) -> String {
    format!("{verb} {table} is not supported (capability mutation.{table}.{leaf} is unavailable)")
}

pub(crate) unsafe fn unsupported_insert(p_vtab: *mut ffi::sqlite3_vtab, table: &str) -> c_int {
    unsafe {
        set_vtab_error(
            p_vtab,
            &unsupported_write_message("INSERT INTO", table, "insert"),
        );
    }
    ffi::SQLITE_ERROR
}

pub(crate) unsafe fn unsupported_delete(p_vtab: *mut ffi::sqlite3_vtab, table: &str) -> c_int {
    unsafe {
        set_vtab_error(
            p_vtab,
            &unsupported_write_message("DELETE FROM", table, "delete"),
        );
    }
    ffi::SQLITE_ERROR
}

pub(crate) unsafe fn unsupported_update(p_vtab: *mut ffi::sqlite3_vtab, table: &str) -> c_int {
    unsafe {
        set_vtab_error(p_vtab, &unsupported_write_message("UPDATE", table, "*"));
    }
    ffi::SQLITE_ERROR
}

/// Finish a vtab callback returning `Result<()>`: on a returned error or a caught
/// panic, copy the message into the vtab's `zErrMsg` so SQLite surfaces it via
/// `sqlite3_errmsg` (matching the C++ reference), then return `SQLITE_ERROR`.
pub(crate) unsafe fn finish_vtab_unit(
    p_vtab: *mut ffi::sqlite3_vtab,
    result: std::thread::Result<Result<()>>,
) -> c_int {
    unsafe {
        match result {
            Ok(Ok(())) => ffi::SQLITE_OK,
            Ok(Err(err)) => {
                set_vtab_error(p_vtab, &err.to_string());
                ffi::SQLITE_ERROR
            }
            Err(_) => {
                set_vtab_error(p_vtab, "xsql virtual table callback panicked");
                ffi::SQLITE_ERROR
            }
        }
    }
}

/// As [`finish_vtab_unit`], for cursor callbacks: resolve the owning vtab from the
/// cursor so the message lands on `pCursor->pVtab->zErrMsg`.
pub(crate) unsafe fn finish_cursor_unit(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    result: std::thread::Result<Result<()>>,
) -> c_int {
    unsafe {
        let p_vtab = if cursor.is_null() {
            std::ptr::null_mut()
        } else {
            (*cursor).pVtab
        };
        finish_vtab_unit(p_vtab, result)
    }
}
