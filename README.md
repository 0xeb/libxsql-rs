# libxsql-rs

<p align="center">
  <a href="https://paypal.me/EliasBachaalany"><img alt="Donate with PayPal" src="https://img.shields.io/badge/Donate-PayPal-0070e0?style=for-the-badge&logo=paypal&logoColor=white"></a>
  <a href="https://buymeacoffee.com/0xeb"><img alt="Buy Me a Coffee" src="https://img.shields.io/badge/Buy_Me_a_Coffee-FFDD00?style=for-the-badge&logo=buymeacoffee&logoColor=black"></a>
  <a href="https://github.com/0xeb/libxsql-rs"><img alt="Star libxsql-rs on GitHub" src="https://img.shields.io/github/stars/0xeb/libxsql-rs?style=for-the-badge&logo=github&label=Star&color=24292f"></a>
</p>

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
libxsql = "0.0.4"
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
- `thinclient`: enables the blocking HTTP thinclient helpers. Turns on `gzip`.
- `gzip`: gzip response compression for the thinclient servers, through
  `flate2`'s pure-Rust backend. A response is compressed when the client sends
  `Accept-Encoding: gzip` and its media type is compressible (decided on the
  type alone: `application/json`, `application/x-ndjson`, `text/*` other than
  `text/event-stream`, and a few more).

## What It Provides

- SQLite database and prepared statement wrappers over `libsqlite3-sys`,
  including explicit close/reopen lifecycle helpers and callback-style exec.
- Scalar and aggregate function registration, including `blob_concat`. A scalar
  function is not deterministic unless it passes `FunctionFlags::DETERMINISTIC`
  (SQLite may fold a deterministic constant-argument call into one evaluation,
  which is wrong for a function with side effects), so every call of a
  `FunctionFlags::NONE` function runs.
- `Database::query_with_params` binds `OwnedValue` parameters (`?`, `?NNN`,
  `:name`, `@name`, `$name`, all by position) under the same timeout,
  cancellation and partial-result rules as `query_with_options`; a parameter
  count that differs from the statement's is an error and nothing runs. Text
  binds by its length, so an embedded NUL is kept. Every result `Row` carries
  each cell's storage class (`Row::value_type`), read before the cell became
  text. `Database::total_changes()` counts the rows changed since the
  connection opened.
- Script splitting, canonical multi-statement result aggregation, result
  formatting, O(one-row) JSON/NDJSON streaming for read-only queries, and SQL
  export helpers. Mutating `RETURNING` rows are held until the statement commits.
  `export_tables` with no table list skips a table that requires constraints
  and names it on a `-- Skipped (requires constraints): ...` header line;
  requesting such a table by name fails the export.
- A typed runtime-setting registry with a connection-isolated, transactional
  `runtime_settings(key, value, type, scope, kind, settable)` virtual table,
  plus the canonical
  `sql_capabilities(name, is_supported, notes)` discovery table.
- Preparation-inclusive timeouts and sticky cooperative cancellation through
  `vtab_interrupted`: read-only queries may return explicit partial results,
  while interrupted mutations (including `RETURNING`) roll back and fail. A
  query a scalar function runs on the same connection
  (`FunctionContext::query_each`) stays inside the enclosing query's deadline
  and cancellation.
- Three virtual table families:
  - `table`: direct row-by-index access for simple in-memory data.
  - `cached_table`: cache-backed rows with projection-aware population, filters,
    indexes, rowid lookup, and optional shared cache. Query-scoped rows live for
    one cursor (built once per JOIN inner loop) and rebuild after a write through
    the table, `CachedTableDef::invalidate_cache()`, or a connection-wide
    `Database::invalidate_cached_tables()` /
    `FunctionContext::invalidate_cached_tables()` -- what a SQL function that
    mutates the engine behind the tables calls. Multi-column/operator pushdown
    (for example an interval-containment join) goes through `constraint_filter`
    (an exact factory) or `constraint_cache_filter` (evaluated in memory through
    `index_on` keys, compared as SQLite compares them); both return `Result` and
    reject a misconfiguration when the table is built.
  - `generator_table`: streaming row sources with hidden columns, required or
    optional constraints, order consumption, row lookup, and an exact
    `row_count` fast path for bare `COUNT(*)`; rowid-only scans still enumerate
    the provider and preserve its real rowids. A table that cannot be read
    without a `WHERE` (a missing required constraint, or `full_scan_error`)
    refuses with its own message and `SQLITE_CONSTRAINT`. Projection-aware variants
    (`projection_generator`, `filter_eq_projection`,
    `parametric_filter_projection`, `constraint_filter_projection`) hand the
    factory SQLite's `colUsed` bitmask so it can skip columns the query never
    reads, and `filter_like_text` pushes down `LIKE 'prefix%'`. When a query
    matches several filters, single-column, parametric and constraint filters
    all compete on their estimated cost and the cheapest drives the scan (a
    single-column filter wins a tie); SQLite re-checks what it leaves unconsumed.
- Writable virtual tables with typed setters, insert handlers, delete handlers,
  modify hooks, and complete prepare/commit/rollback/savepoint hooks with
  connection-local state. A `table`'s rowid is the row's position when the
  statement scanned it; a multi-row DELETE hands each delete callback the row's
  current index (the scanned position minus the rows the statement already
  deleted before it), so a callback that erases by index removes exactly the
  matched rows. The adjustment lasts one statement. Every row write a table's
  callback carries out bumps the connection's `Database::vtab_write_count()`
  (temp and ordinary SQLite tables never do, a refused write does not, and a
  later `ROLLBACK` does not lower it); a bookkeeping table opts out with
  `counts_as_write(false)`, as `runtime_settings` does.
- A blocking thinclient module with an HTTP query server, lower-level custom
  route server, client, queue mode, JSON status/error helpers, shutdown/status
  and cancellation routes, auth token handling, extra routes, clipboard payload
  helpers, and CLI argument parsing. In-flight `/cancel` is advertised only for
  a `script_executor`, the executor shape that receives a cancellation
  predicate, and `cancel_fn` forwards a `/cancel` downstream (a failure is a
  502). When a token is configured, `/status` requires it by default;
  `status_requires_auth: false` is the explicit public liveness opt-out.
  `/query` renders `?format=json|jsonl|text|csv|tsv` (anything else is a 400)
  and honors `X-XSQL-Timeout: <ms>`. A `streaming_executor` answers
  `X-XSQL-Stream: 1|ndjson` requests with a chunked response written row by row
  (batched by `StreamWriteCoalescer`), for engines that can stream a result
  with bounded memory.

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

## Support

If libxsql-rs saves you time, a donation funds the next release, and a star helps others find it.

<p>
  <a href="https://paypal.me/EliasBachaalany"><img alt="Donate with PayPal" src="https://img.shields.io/badge/Donate-PayPal-0070e0?style=for-the-badge&logo=paypal&logoColor=white"></a>
  <a href="https://buymeacoffee.com/0xeb"><img alt="Buy Me a Coffee" src="https://img.shields.io/badge/Buy_Me_a_Coffee-FFDD00?style=for-the-badge&logo=buymeacoffee&logoColor=black"></a>
  <a href="https://github.com/0xeb/libxsql-rs"><img alt="Star libxsql-rs on GitHub" src="https://img.shields.io/github/stars/0xeb/libxsql-rs?style=for-the-badge&logo=github&label=Star&color=24292f"></a>
</p>

Bug reports with a minimal reproduction are just as valuable.

## License and Terms of Use

In short: you may read, build, evaluate, benchmark, package, and use unmodified libxsql-rs, including commercially, if you preserve notices and follow the license terms. You may fork or patch it to prepare bug fixes, optimizations, features, tests, or documentation improvements for contribution back within the license's contribution-purpose rules.

You may not maintain a divergent private fork, port, rebrand, clone, API-compatible replacement, competing implementation, or use libxsql-rs as AI input to recreate or improve a derivative implementation without prior written permission from Elias Bachaalany. Independent implementations that are not copied from, materially derived from, or substantially informed by libxsql-rs in the license's defined sense are not prohibited.

Permission requests: open a GitHub issue at [0xeb/libxsql-rs/issues](https://github.com/0xeb/libxsql-rs/issues).

If libxsql-rs materially informs a distributed project, preserve the human origin: credit libxsql-rs and Elias Bachaalany visibly in your README/docs and in About/credits UI when applicable. The license includes an examples/FAQ section for common allowed and permission-required uses. Third-party dependencies (libsqlite3-sys, serde, thiserror, tiny_http, ureq, flate2, and their transitive dependencies) remain under their own licenses.

See the full [Human-Origin Source License v1.0](LICENSE).
