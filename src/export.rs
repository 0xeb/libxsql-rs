// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::{Database, Error, Result, StepResult};
use libsqlite3_sys as ffi;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

struct ColumnInfo {
    name: String,
    column_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key: bool,
}

/// Exports the given tables to a SQL script at `output_path`.
///
/// When `tables` is empty, every table in `sqlite_master` is exported (ordered
/// by name). For each table the script writes a header comment, a
/// `DROP TABLE IF EXISTS`, a `CREATE TABLE` reconstructed from
/// `PRAGMA table_info` (type, `PRIMARY KEY`, `NOT NULL`, `DEFAULT`), one
/// `INSERT INTO ... VALUES (...)` per row, and a row-count comment. Identifiers
/// are double-quoted and column values are emitted as SQL literals: `NULL`,
/// integers, `%.16g`-style floats, `X'..'` blob hex, and single-quoted escaped
/// text.
pub fn export_tables(
    db: &mut Database,
    tables: &[impl AsRef<str>],
    output_path: impl AsRef<Path>,
) -> Result<()> {
    let tables = if tables.is_empty() {
        load_table_names(db)?
    } else {
        tables
            .iter()
            .map(|table| table.as_ref().to_string())
            .collect()
    };

    let file = File::create(output_path.as_ref())
        .map_err(|error| Error::Message(format!("cannot open output file: {error}")))?;
    let mut out = BufWriter::new(file);

    writeln!(out, "-- SQL Export").map_err(io_error)?;
    writeln!(out, "-- Tables: {}", tables.len()).map_err(io_error)?;
    writeln!(out).map_err(io_error)?;

    for table in tables {
        let quoted_table = quote_identifier(&table);
        let columns = load_columns(db, &quoted_table)?;
        if columns.is_empty() {
            continue;
        }

        writeln!(out, "-- Table: {table}").map_err(io_error)?;
        writeln!(out, "DROP TABLE IF EXISTS {quoted_table};").map_err(io_error)?;
        writeln!(out, "CREATE TABLE {quoted_table} (").map_err(io_error)?;
        for (index, column) in columns.iter().enumerate() {
            write!(out, "    {}", quote_identifier(&column.name)).map_err(io_error)?;
            if !column.column_type.is_empty() {
                write!(out, " {}", column.column_type).map_err(io_error)?;
            }
            if column.primary_key {
                write!(out, " PRIMARY KEY").map_err(io_error)?;
            }
            if column.not_null {
                write!(out, " NOT NULL").map_err(io_error)?;
            }
            if let Some(default_value) = &column.default_value {
                write!(out, " DEFAULT {default_value}").map_err(io_error)?;
            }
            if index + 1 < columns.len() {
                write!(out, ",").map_err(io_error)?;
            }
            writeln!(out).map_err(io_error)?;
        }
        writeln!(out, ");").map_err(io_error)?;
        writeln!(out).map_err(io_error)?;

        let row_count = write_rows(db, &mut out, &quoted_table)?;
        writeln!(out, "-- {row_count} rows exported").map_err(io_error)?;
        writeln!(out).map_err(io_error)?;
    }

    out.flush().map_err(io_error)?;
    Ok(())
}

fn load_table_names(db: &mut Database) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let result = db.query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    for row in result.rows {
        if !row.values.is_empty() {
            names.push(row.values[0].clone());
        }
    }
    Ok(names)
}

fn load_columns(db: &mut Database, quoted_table: &str) -> Result<Vec<ColumnInfo>> {
    let mut statement = db.prepare(&format!("PRAGMA table_info({quoted_table})"))?;
    let mut columns = Vec::new();
    loop {
        match statement.step() {
            StepResult::Row => columns.push(ColumnInfo {
                name: statement.text(1),
                column_type: if statement.column_is_null(2) {
                    String::new()
                } else {
                    statement.text(2)
                },
                not_null: statement.int_value(3) != 0,
                default_value: if statement.column_is_null(4) {
                    None
                } else {
                    Some(statement.text(4))
                },
                primary_key: statement.int_value(5) != 0,
            }),
            StepResult::Done => break,
            StepResult::Busy | StepResult::Error => {
                return Err(Error::Message(statement.error().to_string()));
            }
        }
    }
    Ok(columns)
}

fn write_rows(db: &mut Database, out: &mut impl Write, quoted_table: &str) -> Result<usize> {
    let mut statement = db.prepare(&format!("SELECT * FROM {quoted_table}"))?;
    let mut row_count = 0usize;
    loop {
        match statement.step() {
            StepResult::Row => {
                write!(out, "INSERT INTO {quoted_table} VALUES (").map_err(io_error)?;
                for col in 0..statement.column_count() {
                    if col > 0 {
                        write!(out, ", ").map_err(io_error)?;
                    }
                    write!(out, "{}", literal_for_column(&statement, col)).map_err(io_error)?;
                }
                writeln!(out, ");").map_err(io_error)?;
                row_count += 1;
            }
            StepResult::Done => break,
            StepResult::Busy | StepResult::Error => {
                return Err(Error::Message(statement.error().to_string()));
            }
        }
    }
    Ok(row_count)
}

fn literal_for_column(statement: &crate::Statement, col: i32) -> String {
    match statement.column_type(col) {
        ffi::SQLITE_NULL => "NULL".to_string(),
        ffi::SQLITE_INTEGER => statement.int64_value(col).to_string(),
        ffi::SQLITE_FLOAT => float_literal(statement.double_value(col)),
        ffi::SQLITE_BLOB => blob_literal(&statement.blob(col)),
        ffi::SQLITE_TEXT => quoted_text(&statement.text(col)),
        _ => quoted_text(&statement.text(col)),
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn quoted_text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Format a float as the C++ reference does: `std::ostringstream` with
/// `setprecision(digits10 + 1)` and the default float format, i.e. `printf`'s
/// `%.16g`. Implemented in pure Rust (no libc `snprintf`, which is an inline
/// header function on Windows/MSVC and does not link) so the output is identical
/// and deterministic across platforms.
fn float_literal(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_string();
    }

    // %g with 16 significant digits: choose fixed vs scientific by the (rounded)
    // decimal exponent, then strip trailing zeros.
    const SIG: usize = 16;
    let scientific = format!("{:.*e}", SIG - 1, value); // e.g. "1.500000000000000e0"
    let (mantissa, exp_str) = scientific
        .split_once('e')
        .expect("rust scientific format always has 'e'");
    let exp: i32 = exp_str.parse().expect("valid exponent");

    if exp >= -4 && exp < SIG as i32 {
        // Fixed notation with (SIG-1-exp) fractional digits.
        let frac = (SIG as i32 - 1 - exp).max(0) as usize;
        let mut text = format!("{value:.frac$}");
        strip_trailing_zeros(&mut text);
        text
    } else {
        // Scientific notation: trimmed mantissa + C-style exponent (signed, >=2
        // digits), e.g. "1e+20", "1e-07".
        let mut mantissa = mantissa.to_string();
        strip_trailing_zeros(&mut mantissa);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    }
}

/// Drop trailing zeros (and a trailing decimal point) from a decimal string,
/// matching `%g`'s suppression of insignificant zeros.
fn strip_trailing_zeros(text: &mut String) {
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
}

fn blob_literal(bytes: &[u8]) -> String {
    let mut out = String::from("X'");
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out.push('\'');
    out
}

fn io_error(error: std::io::Error) -> Error {
    Error::Message(error.to_string())
}
