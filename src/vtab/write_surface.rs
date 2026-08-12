// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

//! Prepare-time write-surface authorizer (bnsql issues #2/#3).
//!
//! Mirrors the C++ (`xsql::write_surface_authorizer` / `WriteCaps` /
//! `record_write_surface` in `libxsql/src/database.cpp`) and the C
//! (`xsqlc_write_surface_authorizer` / `xsqlc_db_record_write_surface`) ports.
//!
//! A DML statement against a virtual table whose surface has no mutation support
//! must fail with an actionable, capability-scoped message rather than SQLite's
//! generic "attempt to write a readonly database" (matched-row path, handled in
//! each module's `xUpdate` via [`super::ffi::unsupported_insert`] et al.) or
//! silently "succeeding" for a 0-row DELETE/UPDATE (which never reaches
//! `xUpdate`). This authorizer fires once per DML action at prepare time,
//! INDEPENDENT of match count, so the 0-row case is denied consistently. The
//! message it composes is byte-identical to the matched-row path so both report
//! the same error.

use libsqlite3_sys as ffi;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

use super::ffi::unsupported_write_message;
use crate::error::{Error, Result};

/// Write capabilities of a registered virtual-table module, consulted by the
/// prepare-time write-surface authorizer.
#[derive(Clone, Debug, Default)]
pub(crate) struct WriteCaps {
    pub(crate) insertable: bool,
    pub(crate) deletable: bool,
    pub(crate) writable_columns: HashSet<String>,
}

/// Per-connection registry of concrete schema-qualified virtual-table surfaces.
#[derive(Default)]
pub(crate) struct WriteSurfaceRegistry {
    table_caps: HashMap<String, WriteCaps>,
    preparing_drop: Option<String>,
}

impl WriteSurfaceRegistry {
    /// Record a concrete table from its module's `xCreate`/`xConnect` callback.
    pub(crate) fn connect_table(&mut self, schema: &str, table: &str, caps: WriteCaps) {
        self.table_caps.insert(table_key(schema, table), caps);
    }

    /// Retire a concrete table only when its module's `xDestroy` callback runs.
    pub(crate) fn remove_table(&mut self, schema: &str, table: &str) {
        self.table_caps.remove(&table_key(schema, table));
        self.preparing_drop = None;
    }

    fn caps_for_table(&self, schema: &str, table: &str) -> Option<WriteCaps> {
        self.table_caps.get(&table_key(schema, table)).cloned()
    }
}

fn table_key(schema: &str, table: &str) -> String {
    format!(
        "{}\u{1f}{}",
        schema.to_ascii_lowercase(),
        table.to_ascii_lowercase()
    )
}

pub(crate) unsafe fn connect_write_surface(
    registry: *const RefCell<WriteSurfaceRegistry>,
    argc: c_int,
    argv: *const *const c_char,
    caps: WriteCaps,
) -> Result<(String, String)> {
    unsafe {
        if registry.is_null() || argc < 3 || argv.is_null() {
            return Err(Error::Message(
                "xsql write-surface connection metadata is unavailable".to_string(),
            ));
        }
        let schema_ptr = *argv.add(1);
        let table_ptr = *argv.add(2);
        if schema_ptr.is_null() || table_ptr.is_null() {
            return Err(Error::Message(
                "xsql write-surface schema/table name is null".to_string(),
            ));
        }
        let schema = CStr::from_ptr(schema_ptr).to_string_lossy().into_owned();
        let table = CStr::from_ptr(table_ptr).to_string_lossy().into_owned();
        (*registry)
            .borrow_mut()
            .connect_table(&schema, &table, caps);
        Ok((schema, table))
    }
}

pub(crate) unsafe fn destroy_write_surface(
    registry: *const RefCell<WriteSurfaceRegistry>,
    schema: &str,
    table: &str,
) {
    unsafe {
        if let Some(registry) = registry.as_ref() {
            registry.borrow_mut().remove_table(schema, table);
        }
    }
}

thread_local! {
    // Prepare-time authorizer-denial channel. The authorizer stashes the
    // capability-scoped message here; prepare / exec substitute it for SQLite's
    // fixed "not authorized" on an SQLITE_AUTH failure. Thread-local mirrors the
    // C++ `authorizer_denial_slot()` (thread_local std::string) and the C
    // `g_authz_denial` (XSQL_THREAD_LOCAL).
    static AUTHZ_DENIAL: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Stash the authorizer's denial message for the current prepare/exec.
pub(crate) fn set_authorizer_denial(message: String) {
    AUTHZ_DENIAL.with(|slot| *slot.borrow_mut() = Some(message));
}

/// Clear any prior denial so a message reflects only the current prepare/exec.
pub(crate) fn clear_authorizer_denial() {
    AUTHZ_DENIAL.with(|slot| *slot.borrow_mut() = None);
}

/// Take the stashed denial message, if any (clears the slot).
pub(crate) fn take_authorizer_denial() -> Option<String> {
    AUTHZ_DENIAL.with(|slot| slot.borrow_mut().take())
}

/// Install the write-surface authorizer on `db`, reading `registry` live at
/// prepare time. `registry` must outlive `db` (both are owned by
/// `ConnectionInner`, dropped together after the connection is closed).
///
/// # Safety
/// `registry` must point to a `WriteSurfaceRegistry` that stays valid and
/// pinned for as long as the authorizer is installed on `db`.
pub(crate) unsafe fn install_authorizer(
    db: *mut ffi::sqlite3,
    registry: *const RefCell<WriteSurfaceRegistry>,
) {
    unsafe {
        ffi::sqlite3_set_authorizer(db, Some(write_surface_authorizer), registry as *mut c_void);
    }
}

// The authorizer callback. Fires once per DML action at prepare time. Denies an
// unsupported INSERT/UPDATE/DELETE (stashing the capability-scoped message) and
// otherwise returns SQLITE_OK.
unsafe extern "C" fn write_surface_authorizer(
    p_arg: *mut c_void,
    action: c_int,
    a1: *const c_char,
    a2: *const c_char,
    db_name: *const c_char,
    _trigger: *const c_char,
) -> c_int {
    unsafe {
        if p_arg.is_null() || a1.is_null() {
            return ffi::SQLITE_OK;
        }
        let registry = &*(p_arg as *const RefCell<WriteSurfaceRegistry>);
        let table = match CStr::from_ptr(a1).to_str() {
            Ok(table) => table,
            Err(_) => return ffi::SQLITE_OK,
        };
        let schema = if db_name.is_null() {
            "main"
        } else {
            match CStr::from_ptr(db_name).to_str() {
                Ok(schema) => schema,
                Err(_) => return ffi::SQLITE_OK,
            }
        };
        let target = table_key(schema, table);
        let mut registry = registry.borrow_mut();
        if action == ffi::SQLITE_DROP_VTABLE || action == ffi::SQLITE_DROP_TABLE {
            registry.preparing_drop = registry.table_caps.contains_key(&target).then_some(target);
            return ffi::SQLITE_OK;
        }
        // SQLite authorizes one internal DELETE while compiling DROP TABLE.
        // Permit that companion action, but do not retire the surface until the
        // statement executes and the module's xDestroy callback runs.
        if action == ffi::SQLITE_DELETE
            && registry.preparing_drop.as_deref() == Some(target.as_str())
        {
            registry.preparing_drop = None;
            return ffi::SQLITE_OK;
        }
        registry.preparing_drop = None;
        if action != ffi::SQLITE_INSERT
            && action != ffi::SQLITE_DELETE
            && action != ffi::SQLITE_UPDATE
        {
            return ffi::SQLITE_OK;
        }
        let Some(caps) = registry.caps_for_table(schema, table) else {
            return ffi::SQLITE_OK; // not one of our vtables
        };
        let (verb, leaf) = match action {
            ffi::SQLITE_INSERT => {
                if caps.insertable {
                    return ffi::SQLITE_OK;
                }
                ("INSERT INTO", "insert")
            }
            ffi::SQLITE_DELETE => {
                if caps.deletable {
                    return ffi::SQLITE_OK;
                }
                ("DELETE FROM", "delete")
            }
            ffi::SQLITE_UPDATE => {
                let column = if a2.is_null() {
                    None
                } else {
                    CStr::from_ptr(a2).to_str().ok()
                };
                if column.is_some_and(|column| {
                    caps.writable_columns.contains(&column.to_ascii_lowercase())
                }) {
                    return ffi::SQLITE_OK;
                }
                let leaf = column.unwrap_or("*");
                set_authorizer_denial(if let Some(column) = column {
                    format!(
                        "column \"{column}\" is read-only (capability mutation.{table}.{leaf} is unavailable)"
                    )
                } else {
                    unsupported_write_message("UPDATE", table, leaf)
                });
                return ffi::SQLITE_DENY;
            }
            _ => return ffi::SQLITE_OK,
        };
        set_authorizer_denial(unsupported_write_message(verb, table, leaf));
        ffi::SQLITE_DENY
    }
}

#[cfg(test)]
mod tests {
    use super::{WriteCaps, WriteSurfaceRegistry};
    use std::collections::HashSet;

    #[test]
    fn table_capabilities_are_schema_qualified_and_removable() {
        let caps = WriteCaps {
            insertable: true,
            deletable: false,
            writable_columns: HashSet::from(["name".to_string()]),
        };
        let mut registry = WriteSurfaceRegistry::default();
        registry.connect_table("main", "items", caps);

        assert!(registry.caps_for_table("main", "items").is_some());
        assert!(registry.caps_for_table("temp", "items").is_none());
        registry.remove_table("main", "items");
        assert!(registry.caps_for_table("main", "items").is_none());
    }
}
