# libxsql-rs

`libxsql-rs` is a Rust port of libxsql for exposing Rust data through SQLite.
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
  formatting, and SQL export helpers.
- Cooperative timeout polling for virtual table callbacks through
  `vtab_interrupted`.
- Three virtual table families:
  - `table`: direct row-by-index access for simple in-memory data.
  - `cached_table`: cache-backed rows with projection-aware population, filters,
    indexes, rowid lookup, and optional shared cache.
  - `generator_table`: streaming row sources with hidden columns, required or
    optional constraints, order consumption, and row lookup.
- Writable virtual tables with typed setters, insert handlers, delete handlers,
  and modify hooks.
- A blocking thinclient module with an HTTP query server, lower-level custom
  route server, client, queue mode, JSON status/error helpers, shutdown/status
  routes, auth token handling, extra routes, clipboard payload helpers, and CLI
  argument parsing.

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

## License

Copyright (c) 2026 Elias Bachaalany.

Licensed under the Mozilla Public License, v. 2.0 ([MPL-2.0](LICENSE)).
