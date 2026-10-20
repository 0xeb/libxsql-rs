// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::{Database, Error, Result, Statement, StepResult};
use libsqlite3_sys as ffi;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct ColumnInfo {
    name: String,
    column_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key: bool,
}

/// SQLite's planner text when every plan a virtual table offered was refused
/// (its `xBestIndex` returned `SQLITE_CONSTRAINT` for each one).
const NO_QUERY_SOLUTION: &str = "no query solution";

/// What happened to one table.
enum TableOutcome {
    /// Its section was written.
    Exported,
    /// It has no columns; nothing was written.
    Empty,
    /// It cannot be read without constraints; nothing was written. Carries the
    /// table's own reason.
    Refused(String),
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct AdjacentTempFile {
    path: PathBuf,
    file: Option<File>,
    owned: bool,
}

impl AdjacentTempFile {
    fn create(output_path: &Path, role: &str) -> io::Result<Self> {
        for _ in 0..1024 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let mut name = OsString::from(output_path.as_os_str());
            name.push(format!(".{role}.{}.{id}.tmp", std::process::id()));
            let path = PathBuf::from(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        owned: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot allocate an adjacent export temporary file",
        ))
    }

    fn take_file(&mut self) -> File {
        self.file.take().expect("temporary file already consumed")
    }

    fn rename_into(mut self, output_path: &Path) -> io::Result<()> {
        self.file.take();
        fs::rename(&self.path, output_path)?;
        self.owned = false;
        Ok(())
    }
}

impl Drop for AdjacentTempFile {
    fn drop(&mut self) {
        self.file.take();
        if self.owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Exports the given tables to a SQL script at `output_path`.
///
/// When `tables` is empty, every table in `sqlite_master` is exported (ordered
/// by name). For each table the script writes a header comment, a
/// `DROP TABLE IF EXISTS`, a `CREATE TABLE` reconstructed from
/// `PRAGMA table_info` (type, `PRIMARY KEY`, `NOT NULL`, `DEFAULT`), one
/// `INSERT INTO ... VALUES (...)` per row, and a row-count comment. Identifiers
/// are double-quoted and column values are emitted as SQL literals: `NULL`,
/// integers, round-trip-safe REAL literals, `X'..'` blob hex, and single-quoted escaped
/// text.
///
/// The script opens with `-- SQL Export` and `-- Tables: N`, `N` being the
/// tables actually written. A table that cannot be read without constraints (a
/// virtual table needing a `WHERE`: a required constraint, or a forbidden full
/// scan) is skipped when exporting every table and named on a
/// `-- Skipped (requires constraints): a, b` header line. Requested by name, it
/// fails the export with `Table 'a' cannot be exported: it requires constraints
/// (WHERE) to be read (<the table's reason>)`. Any other error fails the export.
/// A failed export leaves `output_path` untouched.
pub fn export_tables(
    db: &mut Database,
    tables: &[impl AsRef<str>],
    output_path: impl AsRef<Path>,
) -> Result<()> {
    let explicit_request = !tables.is_empty();
    let tables = if explicit_request {
        tables
            .iter()
            .map(|table| table.as_ref().to_string())
            .collect()
    } else {
        load_table_names(db)?
    };
    let output_path = output_path.as_ref();

    // The header reports how many tables were written and which were skipped,
    // known only at the end. Sections stream to a sibling body file (memory
    // stays at one row); header and body are then joined into `output_path`.
    // A failed export leaves `output_path` untouched.
    let mut body = AdjacentTempFile::create(output_path, "body")
        .map_err(|error| Error::Message(format!("cannot open output file: {error}")))?;
    let body_file = body.take_file();
    let result = export_body(db, &tables, explicit_request, body_file)
        .and_then(|(exported, skipped)| write_output(output_path, &body.path, exported, &skipped));
    drop(body);
    result
}

fn export_body(
    db: &mut Database,
    tables: &[String],
    explicit_request: bool,
    file: File,
) -> Result<(usize, Vec<String>)> {
    let mut out = BufWriter::new(file);
    let mut exported = 0usize;
    let mut skipped = Vec::new();
    for table in tables {
        match export_one_table(db, &mut out, table)? {
            TableOutcome::Exported => exported += 1,
            TableOutcome::Empty => {}
            TableOutcome::Refused(reason) if explicit_request => {
                return Err(Error::Message(refusal_error(table, &reason)));
            }
            TableOutcome::Refused(_) => skipped.push(table.clone()),
        }
    }
    out.flush().map_err(io_error)?;
    Ok((exported, skipped))
}

fn write_output(
    output_path: &Path,
    body_path: &Path,
    exported: usize,
    skipped: &[String],
) -> Result<()> {
    // Assembled beside the target and renamed over it, so a failure while
    // writing never leaves a partial `output_path`.
    let mut staged = AdjacentTempFile::create(output_path, "stage")
        .map_err(|error| Error::Message(format!("cannot open output file: {error}")))?;
    let file = staged.take_file();
    let written = (|| -> io::Result<()> {
        let mut out = BufWriter::new(file);
        writeln!(out, "-- SQL Export")?;
        writeln!(out, "-- Tables: {exported}")?;
        if !skipped.is_empty() {
            writeln!(
                out,
                "-- Skipped (requires constraints): {}",
                skipped.join(", ")
            )?;
        }
        writeln!(out)?;
        io::copy(&mut File::open(body_path)?, &mut out)?;
        out.flush()?;
        drop(out);
        staged.rename_into(output_path)
    })();
    if let Err(error) = written {
        return Err(io_error(error));
    }
    Ok(())
}

fn refusal_error(table: &str, reason: &str) -> String {
    let mut message =
        format!("Table '{table}' cannot be exported: it requires constraints (WHERE) to be read");
    if !reason.is_empty() {
        message.push_str(&format!(" ({reason})"));
    }
    message
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

/// Write one table's section. Whether the table can be read is settled by the
/// first step of its `SELECT`, before any of its text is written, so a refused
/// table leaves nothing behind.
fn export_one_table(db: &mut Database, out: &mut impl Write, table: &str) -> Result<TableOutcome> {
    let quoted_table = quote_identifier(table);
    let columns = load_columns(db, &quoted_table)?;
    if columns.is_empty() {
        return Ok(TableOutcome::Empty);
    }

    let mut statement = match db.prepare(&format!("SELECT * FROM {quoted_table}")) {
        Ok(statement) => statement,
        // A table whose xBestIndex rejects every plan fails to prepare with
        // SQLite's own "no query solution".
        Err(Error::Sqlite { status, message })
            if status.as_sqlite() == ffi::SQLITE_ERROR && message == NO_QUERY_SOLUTION =>
        {
            return Ok(TableOutcome::Refused(message));
        }
        Err(error) => return Err(error),
    };
    let mut step = statement.step();
    match step {
        StepResult::Row | StepResult::Done => {}
        // A libxsql table refuses an unconstrained scan with SQLITE_CONSTRAINT_VTAB,
        // with the explicit virtual-table refusal code. Backend constraints remain errors.
        StepResult::Error if statement.last_result_code() == ffi::SQLITE_CONSTRAINT_VTAB => {
            return Ok(TableOutcome::Refused(statement.error().to_string()));
        }
        StepResult::Busy | StepResult::Error => {
            return Err(Error::Message(statement.error().to_string()));
        }
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

    let mut row_count = 0usize;
    while matches!(step, StepResult::Row) {
        write_row(out, &statement, &quoted_table)?;
        row_count += 1;
        step = statement.step();
    }
    if !matches!(step, StepResult::Done) {
        // A failure after the table was opened is a real error, whatever its code.
        return Err(Error::Message(statement.error().to_string()));
    }

    writeln!(out, "-- {row_count} rows exported").map_err(io_error)?;
    writeln!(out).map_err(io_error)?;
    Ok(TableOutcome::Exported)
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

fn write_row(out: &mut impl Write, statement: &Statement, quoted_table: &str) -> Result<()> {
    write!(out, "INSERT INTO {quoted_table} VALUES (").map_err(io_error)?;
    for col in 0..statement.column_count() {
        if col > 0 {
            write!(out, ", ").map_err(io_error)?;
        }
        write!(out, "{}", literal_for_column(statement, col)).map_err(io_error)?;
    }
    writeln!(out, ");").map_err(io_error)
}

fn literal_for_column(statement: &Statement, col: i32) -> String {
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
    if value.contains('\0') {
        format!("CAST({} AS TEXT)", blob_literal(value.as_bytes()))
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

/// Locale-independent SQL REAL literal with seventeen significant digits.
fn float_literal(value: f64) -> String {
    if value.is_nan() {
        return "NULL".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-9.0e999"
        } else {
            "9.0e999"
        }
        .to_owned();
    }
    let scientific = format!("{value:.16e}");
    let (mantissa, exponent) = scientific.split_once('e').expect("scientific exponent");
    let exponent: i32 = exponent.parse().expect("decimal exponent");
    format!("{mantissa}e{exponent:+03}")
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
