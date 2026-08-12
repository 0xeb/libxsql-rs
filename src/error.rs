// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use libsqlite3_sys as ffi;
use std::ffi::NulError;
use thiserror::Error;

/// Convenience result type whose error variant is this crate's [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Error type spanning SQLite status failures and crate-level conditions.
#[derive(Debug, Error)]
pub enum Error {
    /// A non-OK SQLite status returned by the underlying engine.
    #[error("sqlite error {status:?}: {message}")]
    Sqlite {
        /// The SQLite status code, mapped to a [`Status`] variant.
        status: Status,
        /// Human-readable detail accompanying the status.
        message: String,
    },
    /// A Rust string contained an interior NUL byte and could not be passed to SQLite.
    #[error("interior NUL byte in string: {0}")]
    Nul(#[from] NulError),
    /// An operation required an open database, but none was available.
    #[error("database not open")]
    DatabaseNotOpen,
    /// Text returned by SQLite was not valid UTF-8.
    #[error("invalid UTF-8 from SQLite")]
    InvalidUtf8,
    /// An integer input did not fit the target type's range.
    #[error("integer input out of range")]
    IntegerOutOfRange,
    /// A free-form error message not tied to a SQLite status.
    #[error("{0}")]
    Message(String),
}

impl Error {
    pub(crate) fn sqlite(status: i32, message: impl Into<String>) -> Self {
        Self::Sqlite {
            status: Status::from_sqlite(status),
            message: message.into(),
        }
    }
}

/// A SQLite result/status code mapped to a Rust enum.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// Successful result (`SQLITE_OK`).
    Ok = ffi::SQLITE_OK,
    /// Generic error (`SQLITE_ERROR`).
    Error = ffi::SQLITE_ERROR,
    /// Internal logic error in SQLite (`SQLITE_INTERNAL`).
    Internal = ffi::SQLITE_INTERNAL,
    /// Access permission denied (`SQLITE_PERM`).
    Perm = ffi::SQLITE_PERM,
    /// Callback routine requested an abort (`SQLITE_ABORT`).
    Abort = ffi::SQLITE_ABORT,
    /// The database file is locked (`SQLITE_BUSY`).
    Busy = ffi::SQLITE_BUSY,
    /// A table in the database is locked (`SQLITE_LOCKED`).
    Locked = ffi::SQLITE_LOCKED,
    /// A `malloc()` failed (`SQLITE_NOMEM`).
    NoMemory = ffi::SQLITE_NOMEM,
    /// Attempt to write a readonly database (`SQLITE_READONLY`).
    ReadOnly = ffi::SQLITE_READONLY,
    /// Operation terminated by `sqlite3_interrupt()` (`SQLITE_INTERRUPT`).
    Interrupt = ffi::SQLITE_INTERRUPT,
    /// Some kind of disk I/O error occurred (`SQLITE_IOERR`).
    IoError = ffi::SQLITE_IOERR,
    /// The database disk image is malformed (`SQLITE_CORRUPT`).
    Corrupt = ffi::SQLITE_CORRUPT,
    /// Unknown opcode in `sqlite3_file_control()` (`SQLITE_NOTFOUND`).
    NotFound = ffi::SQLITE_NOTFOUND,
    /// Insertion failed because the database is full (`SQLITE_FULL`).
    Full = ffi::SQLITE_FULL,
    /// Unable to open the database file (`SQLITE_CANTOPEN`).
    CantOpen = ffi::SQLITE_CANTOPEN,
    /// Database lock protocol error (`SQLITE_PROTOCOL`).
    Protocol = ffi::SQLITE_PROTOCOL,
    /// Internal use only (`SQLITE_EMPTY`).
    Empty = ffi::SQLITE_EMPTY,
    /// The database schema changed (`SQLITE_SCHEMA`).
    Schema = ffi::SQLITE_SCHEMA,
    /// String or BLOB exceeds size limit (`SQLITE_TOOBIG`).
    TooBig = ffi::SQLITE_TOOBIG,
    /// Abort due to constraint violation (`SQLITE_CONSTRAINT`).
    Constraint = ffi::SQLITE_CONSTRAINT,
    /// Data type mismatch (`SQLITE_MISMATCH`).
    Mismatch = ffi::SQLITE_MISMATCH,
    /// Library used incorrectly (`SQLITE_MISUSE`).
    Misuse = ffi::SQLITE_MISUSE,
    /// Uses OS features not supported on host (`SQLITE_NOLFS`).
    NoLfs = ffi::SQLITE_NOLFS,
    /// Authorization denied (`SQLITE_AUTH`).
    Auth = ffi::SQLITE_AUTH,
    /// Not used (`SQLITE_FORMAT`).
    Format = ffi::SQLITE_FORMAT,
    /// The 2nd parameter to `sqlite3_bind` out of range (`SQLITE_RANGE`).
    Range = ffi::SQLITE_RANGE,
    /// File opened that is not a database file (`SQLITE_NOTADB`).
    NotADb = ffi::SQLITE_NOTADB,
    /// Notifications from `sqlite3_log()` (`SQLITE_NOTICE`).
    Notice = ffi::SQLITE_NOTICE,
    /// Warnings from `sqlite3_log()` (`SQLITE_WARNING`).
    Warning = ffi::SQLITE_WARNING,
    /// `sqlite3_step()` has another row ready (`SQLITE_ROW`).
    Row = ffi::SQLITE_ROW,
    /// `sqlite3_step()` has finished executing (`SQLITE_DONE`).
    Done = ffi::SQLITE_DONE,
    /// A status code not recognized by this mapping.
    Unknown = -1,
}

impl Status {
    /// Maps a raw SQLite status code to the matching [`Status`] variant, or [`Status::Unknown`].
    pub fn from_sqlite(status: i32) -> Self {
        match status {
            ffi::SQLITE_OK => Self::Ok,
            ffi::SQLITE_ERROR => Self::Error,
            ffi::SQLITE_INTERNAL => Self::Internal,
            ffi::SQLITE_PERM => Self::Perm,
            ffi::SQLITE_ABORT => Self::Abort,
            ffi::SQLITE_BUSY => Self::Busy,
            ffi::SQLITE_LOCKED => Self::Locked,
            ffi::SQLITE_NOMEM => Self::NoMemory,
            ffi::SQLITE_READONLY => Self::ReadOnly,
            ffi::SQLITE_INTERRUPT => Self::Interrupt,
            ffi::SQLITE_IOERR => Self::IoError,
            ffi::SQLITE_CORRUPT => Self::Corrupt,
            ffi::SQLITE_NOTFOUND => Self::NotFound,
            ffi::SQLITE_FULL => Self::Full,
            ffi::SQLITE_CANTOPEN => Self::CantOpen,
            ffi::SQLITE_PROTOCOL => Self::Protocol,
            ffi::SQLITE_EMPTY => Self::Empty,
            ffi::SQLITE_SCHEMA => Self::Schema,
            ffi::SQLITE_TOOBIG => Self::TooBig,
            ffi::SQLITE_CONSTRAINT => Self::Constraint,
            ffi::SQLITE_MISMATCH => Self::Mismatch,
            ffi::SQLITE_MISUSE => Self::Misuse,
            ffi::SQLITE_NOLFS => Self::NoLfs,
            ffi::SQLITE_AUTH => Self::Auth,
            ffi::SQLITE_FORMAT => Self::Format,
            ffi::SQLITE_RANGE => Self::Range,
            ffi::SQLITE_NOTADB => Self::NotADb,
            ffi::SQLITE_NOTICE => Self::Notice,
            ffi::SQLITE_WARNING => Self::Warning,
            ffi::SQLITE_ROW => Self::Row,
            ffi::SQLITE_DONE => Self::Done,
            _ => Self::Unknown,
        }
    }

    /// Returns the underlying raw SQLite status code.
    pub fn as_sqlite(self) -> i32 {
        self as i32
    }

    /// Returns `true` if this status is [`Status::Ok`].
    pub fn is_ok(self) -> bool {
        self == Self::Ok
    }

    /// Returns `true` if this status is [`Status::Row`] (another row is available).
    pub fn is_row(self) -> bool {
        self == Self::Row
    }

    /// Returns `true` if this status is [`Status::Done`] (execution finished).
    pub fn is_done(self) -> bool {
        self == Self::Done
    }
}

pub(crate) fn sqlite_ok(status: i32) -> bool {
    status == ffi::SQLITE_OK
}

/// Converts a [`Status`] into its raw SQLite status code.
pub fn to_sqlite_status(status: Status) -> i32 {
    status.as_sqlite()
}

/// Converts a raw SQLite status code into a [`Status`].
pub fn to_status(sqlite_status: i32) -> Status {
    Status::from_sqlite(sqlite_status)
}

/// Returns `true` if the given [`Status`] is [`Status::Ok`].
pub fn is_ok(status: Status) -> bool {
    status.is_ok()
}

/// Returns `true` if the given [`Status`] is [`Status::Ok`].
pub fn is_ok_status(status: Status) -> bool {
    status.is_ok()
}

/// Returns `true` if the raw SQLite status code is `SQLITE_OK`.
pub fn is_ok_sqlite(sqlite_status: i32) -> bool {
    sqlite_status == ffi::SQLITE_OK
}

/// Returns `true` if the raw SQLite status code is `SQLITE_ROW`.
pub fn is_row(sqlite_status: i32) -> bool {
    sqlite_status == ffi::SQLITE_ROW
}

/// Returns `true` if the raw SQLite status code is `SQLITE_DONE`.
pub fn is_done(sqlite_status: i32) -> bool {
    sqlite_status == ffi::SQLITE_DONE
}
