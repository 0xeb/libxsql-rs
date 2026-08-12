// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

//! Safe, typed Rust builders for exposing data through SQLite virtual tables.
//!
//! The crate is intentionally built around direct SQLite FFI because virtual
//! table parity needs access to planner (`xBestIndex`) and update (`xUpdate`)
//! callbacks that high-level wrappers normally hide. The unsafe boundary is
//! contained in internal modules; the public API exposes only safe Rust types.
//!
//! # Capabilities
//!
//! - Three virtual-table families, each with typed read/write columns,
//!   filters, and `INSERT`/`UPDATE`/`DELETE` support:
//!   - [`table`] — direct row-by-index access for simple in-memory data.
//!   - [`cached_table`] — cache-backed rows with projection-aware population,
//!     equality/prefix filters, hash indexes, rowid lookup, and an optional
//!     shared cache.
//!   - [`generator_table`] — streaming row sources with hidden columns,
//!     required/optional constraints, and sort elision.
//! - Scalar and aggregate SQL functions (including the built-in `blob_concat`).
//! - Prepared [`Statement`]s, script splitting/execution, SQL [`export_tables`],
//!   preparation-inclusive query timeouts, sticky cooperative cancellation,
//!   partial results for read-only statements, and rollback-safe mutation
//!   interruption via [`vtab_interrupted`].
//!
//! # Feature flags
//!
//! - `bundled-sqlite` — build SQLite through `libsqlite3-sys` (no system SQLite).
//! - `thinclient` — the blocking HTTP query server and client ([`thinclient`]).
//! - `serde` — JSON formatting helpers for script results.
//!
//! # Quickstart
//!
//! ```
//! use std::rc::Rc;
//!
//! let items = Rc::new(vec!["alpha".to_string(), "beta".to_string()]);
//! let def = libxsql::table("items")
//!     .count({
//!         let items = items.clone();
//!         move || items.len()
//!     })
//!     .column_text("name", {
//!         let items = items.clone();
//!         move |row| items[row].clone()
//!     })
//!     .build()?;
//!
//! let mut db = libxsql::Database::open_memory()?;
//! db.register_and_create_table(&def)?;
//! let rows = db.query("SELECT name FROM items ORDER BY name")?.rows;
//! assert_eq!(rows.len(), 2);
//! # Ok::<(), libxsql::Error>(())
//! ```

#![deny(missing_docs)]

mod aggregate;
mod capabilities;
mod database;
mod error;
mod function;
mod statement;
/// Shared borderless and boxed table rendering for command-line consumers.
pub mod table_printer;
mod value;

/// SQL export helpers: serialize tables to a SQL `CREATE`/`INSERT` script.
pub mod export;
/// General-purpose directed-graph algorithms (RE-agnostic): dominators,
/// post-dominators, natural loops, strongly connected components, and topological
/// order over opaque integer node ids. Port of the C++ `xsql::graph` header.
pub mod graph;
/// Shared runtime settings and the common `PRAGMA <prefix>.*` handler
/// (query/queue timeouts, admission queue bound, hints toggle, timeout stack).
pub mod runtime_settings;
/// SQL script splitting, execution, and canonical result formatting
/// (text/CSV/TSV, plus JSON with the `serde` or `thinclient` feature).
pub mod script;
/// Blocking HTTP query server and client (the `thinclient` feature): query
/// endpoint, status/cancel/shutdown routes, auth, queue mode, CLI parsing, and
/// clipboard helpers.
#[cfg(feature = "thinclient")]
pub mod thinclient;
/// Virtual-table builders and the SQLite module machinery for the `table`,
/// `cached_table`, and `generator_table` families.
pub mod vtab;

/// JSON value type re-exported from `serde_json` (available with either the
/// `serde` or `thinclient` feature, both of which pull in `serde_json`).
#[cfg(any(feature = "serde", feature = "thinclient"))]
pub type Json = serde_json::Value;
/// Insertion-order-preserving JSON value type; an alias of [`Json`] kept for
/// naming parity with the C++ library (`serde_json/preserve_order` is enabled).
#[cfg(any(feature = "serde", feature = "thinclient"))]
pub type OrderedJson = serde_json::Value;

pub use aggregate::AggregateContext;
pub use capabilities::{SqlCapability, define_sql_capabilities};
pub use database::{Database, QueryOptions, QueryOutcome, QueryResult, Row, ScriptExecutionMode};
pub use error::{
    Error, Result, Status, is_done, is_ok, is_ok_sqlite, is_ok_status, is_row, to_sqlite_status,
    to_status,
};
pub use export::export_tables;
pub use function::{FunctionArg, FunctionContext, QueryRow};
pub use script::{
    ScriptOptions, ScriptResult, ScriptStatementResult, StatementResult, collect_statements,
    execute_script, quote_identifier, run_database_script, run_script, run_script_with_executor,
    script_result_to_csv, script_result_to_text, script_result_to_tsv, split_script,
};
#[cfg(any(feature = "serde", feature = "thinclient"))]
pub use script::{
    json_to_script_result, script_result_to_json, script_result_to_json_with_sql,
    script_result_to_jsonl, stream_database_script_json, stream_database_script_ndjson,
};
pub use statement::{Statement, StepResult};
pub use table_printer::{TablePrintOptions, TableStyle, print_table};
pub use value::{OwnedValue, ValueRef, ValueType};
pub use vtab::{
    CachedTableBuilder, CachedTableDef, ColumnType, ConstraintOp, ConstraintRequest, Generator,
    GeneratorConstraintArg, GeneratorTableBuilder, GeneratorTableDef, RowIterator, TableBuilder,
    TableDef, TransactionHooks, TransactionState, cached_table, clear_vtab_interrupt_checker,
    generator_table, optional_eq, optional_ge, optional_gt, optional_le, optional_like,
    optional_lt, required_eq, required_like, set_vtab_error, set_vtab_interrupt_checker, table,
    vtab_interrupted,
};
