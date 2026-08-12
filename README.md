# libxsql-rs

`libxsql-rs` is a Rust port of [libxsql](https://github.com/0xeb/libxsql) for exposing Rust data through SQLite.
It provides safe, typed builders for SQLite virtual tables while keeping direct
access to the SQLite planner, update callbacks, scalar functions, aggregate
functions, and query execution APIs that higher-level wrappers usually hide.

The crate tracks the public libxsql capabilities with Rust-shaped APIs,
covering the same practical feature set.

## Package

The public crate is `libxsql`.

```toml
[dependencies]
libxsql = "0.0.1"
```

Development alternatives — track the git repository or a local checkout:

```toml
[dependencies]
# Latest from git:
libxsql = { git = "https://github.com/0xeb/libxsql-rs", package = "libxsql" }
# Local checkout:
libxsql = { path = "." }
```

## Feature Flags

- `bundled-sqlite`: builds SQLite through `libsqlite3-sys`.
- `serde`: enables JSON formatting for script results.
- `thinclient`: enables the blocking HTTP thinclient helpers.

## What It Provides

- SQLite database and prepared statement wrappers over `libsqlite3-sys`,
  including explicit close/reopen lifecycle helpers and callback-style exec.
- Scalar and aggregate function registration, including `blob_concat`.
- Script splitting, canonical multi-statement result aggregation, result
  formatting, O(one-row) JSON/NDJSON streaming for read-only queries, and SQL
  export helpers. Mutating `RETURNING` rows are held until the statement commits.
- A typed runtime-setting registry with a connection-isolated, transactional
  `runtime_settings(key, value, type, scope)` virtual table, plus the canonical
  `sql_capabilities(name, is_supported, notes)` discovery table.
- Preparation-inclusive timeouts and sticky cooperative cancellation through
  `vtab_interrupted`: read-only queries may return explicit partial results,
  while interrupted mutations (including `RETURNING`) roll back and fail.
- Three virtual table families:
  - `table`: direct row-by-index access for simple in-memory data.
  - `cached_table`: cache-backed rows with projection-aware population, filters,
    indexes, rowid lookup, and optional shared cache.
  - `generator_table`: streaming row sources with hidden columns, required or
    optional constraints, order consumption, row lookup, and an exact
    `row_count` fast path for bare `COUNT(*)`; rowid-only scans still enumerate
    the provider and preserve its real rowids.
- Writable virtual tables with typed setters, insert handlers, delete handlers,
  modify hooks, and complete prepare/commit/rollback/savepoint hooks with
  connection-local state.
- A blocking thinclient module with an HTTP query server, lower-level custom
  route server, client, queue mode, JSON status/error helpers, shutdown/status
  and cancellation routes, auth token handling, extra routes, clipboard payload
  helpers, and CLI argument parsing. In-flight `/cancel` is advertised only for
  a `script_executor`, the executor shape that receives a cancellation
  predicate. When a token is configured, `/status` requires it by default;
  `status_requires_auth: false` is the explicit public liveness opt-out.

## Basic Virtual Table

```rust
use std::rc::Rc;

struct Item {
    id: i32,
    name: String,
}

fn main() -> libxsql::Result<()> {
    let items = Rc::new(vec![
        Item {
            id: 1,
            name: "alpha".to_string(),
        },
        Item {
            id: 2,
            name: "beta".to_string(),
        },
    ]);

    let def = libxsql::table("items")
        .count({
            let items = items.clone();
            move || items.len()
        })
        .column_int("id", {
            let items = items.clone();
            move |row| items[row].id
        })
        .column_text("name", {
            let items = items.clone();
            move |row| items[row].name.clone()
        })
        .build()?;

    let mut db = libxsql::Database::open_memory()?;
    db.register_and_create_table(&def)?;

    for row in db.query("SELECT id, name FROM items ORDER BY id")?.rows {
        println!("{}\t{}", &row[0], &row[1]);
    }

    Ok(())
}
```

More complete examples are in `examples/`:

- `basic_table.rs`
- `writable_table.rs`
- `cached_table.rs`
- `generator_table.rs`
- `http_server.rs`

## Public Validation

```sh
cargo fmt --check
cargo clippy --all-features --all-targets
cargo build --examples --all-features
```

## License and Terms of Use

In short: you may read, build, evaluate, benchmark, package, and use unmodified libxsql-rs, including commercially, if you preserve notices and follow the license terms. You may fork or patch it to prepare bug fixes, optimizations, features, tests, or documentation improvements for contribution back within the license's contribution-purpose rules.

You may not maintain a divergent private fork, port, rebrand, clone, API-compatible replacement, competing implementation, or use libxsql-rs as AI input to recreate or improve a derivative implementation without prior written permission from Elias Bachaalany. Independent implementations that are not copied from, materially derived from, or substantially informed by libxsql-rs in the license's defined sense are not prohibited.

Permission requests: open a GitHub issue at [0xeb/libxsql-rs/issues](https://github.com/0xeb/libxsql-rs/issues).

If libxsql-rs materially informs a distributed project, preserve the human origin: credit libxsql-rs and Elias Bachaalany visibly in your README/docs and in About/credits UI when applicable. The license includes an examples/FAQ section for common allowed and permission-required uses. Third-party dependencies (libsqlite3-sys, serde, thiserror, tiny_http, ureq, and their transitive dependencies) remain under their own licenses.

See the full [Human-Origin Source License v1.0](LICENSE).
