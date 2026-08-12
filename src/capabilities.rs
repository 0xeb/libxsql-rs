// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

//! Canonical SQL capability discovery table.

use crate::{CachedTableDef, Error, Result, cached_table};
use std::rc::Rc;

/// One stable capability declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlCapability {
    /// Stable dotted capability name.
    pub name: String,
    /// Whether this build supports the capability.
    pub is_supported: bool,
    /// Concise consumer-facing behavior or limitation.
    pub notes: String,
}

/// Build `sql_capabilities(name TEXT, is_supported INTEGER, notes TEXT)`.
///
/// Rows are sorted by name. Empty and duplicate names fail at definition time,
/// so consumers never receive ambiguous answers.
pub fn define_sql_capabilities(
    mut capabilities: Vec<SqlCapability>,
) -> Result<CachedTableDef<SqlCapability>> {
    capabilities.sort_by(|left, right| left.name.cmp(&right.name));
    for (index, capability) in capabilities.iter().enumerate() {
        if capability.name.is_empty() {
            return Err(Error::Message(
                "sql_capabilities contains an empty capability name".into(),
            ));
        }
        if index != 0 && capabilities[index - 1].name == capability.name {
            return Err(Error::Message(format!(
                "sql_capabilities contains duplicate capability '{}'",
                capability.name
            )));
        }
    }
    let rows = Rc::new(capabilities);
    let count_rows = rows.clone();
    let estimate_rows = rows.clone();
    let build_rows = rows.clone();
    cached_table("sql_capabilities")
        .count(move || count_rows.len())
        .estimate_rows(move || estimate_rows.len())
        .cache_builder(move |output| output.clone_from(&build_rows))
        .column_text("name", |row| row.name.clone())
        .column_int("is_supported", |row| i32::from(row.is_supported))
        .column_text("notes", |row| row.notes.clone())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    #[test]
    fn canonical_table_is_sorted_and_read_only() -> Result<()> {
        let definition = define_sql_capabilities(vec![
            SqlCapability {
                name: "mutation.settings".into(),
                is_supported: true,
                notes: "Transactional UPDATE.".into(),
            },
            SqlCapability {
                name: "feature.runtime_settings".into(),
                is_supported: true,
                notes: "Canonical settings table.".into(),
            },
        ])?;
        let mut db = Database::open_memory()?;
        db.register_and_create_cached_table(&definition)?;
        let rows =
            db.query("SELECT name, is_supported, notes FROM sql_capabilities ORDER BY rowid")?;
        assert_eq!(rows.rows[0].values[0], "feature.runtime_settings");
        assert_eq!(rows.rows[1].values[0], "mutation.settings");
        assert!(
            db.exec("UPDATE sql_capabilities SET is_supported=0")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn duplicate_and_empty_names_fail() {
        assert!(
            define_sql_capabilities(vec![SqlCapability {
                name: String::new(),
                is_supported: false,
                notes: String::new(),
            }])
            .is_err()
        );
        assert!(
            define_sql_capabilities(vec![
                SqlCapability {
                    name: "x".into(),
                    is_supported: false,
                    notes: String::new(),
                },
                SqlCapability {
                    name: "x".into(),
                    is_supported: true,
                    notes: String::new(),
                },
            ])
            .is_err()
        );
    }
}
