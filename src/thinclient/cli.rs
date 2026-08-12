// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::{Error, Result};

use super::io_error;

/// Parsed command-line operating mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliMode {
    /// Open a database locally and run a one-shot query.
    Direct,
    /// Run as a server hosting a database.
    Serve,
    /// Connect to a running server and run a query.
    Client,
}

/// Parsed thinclient command-line arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliArgs {
    /// Operating mode inferred from the other arguments.
    pub mode: CliMode,
    /// Database source (`-s`/`--source` or a bare positional argument).
    pub source: Option<String>,
    /// Inline SQL command (`-c`/`--command`).
    pub command: Option<String>,
    /// Path to a SQL file (`-f`/`--file`).
    pub file: Option<String>,
    /// Output format (`-o`/`--output`); defaults to `csv`.
    pub output: Option<String>,
    /// Whether `--serve` was passed.
    pub serve: bool,
    /// Server/client port (`--port`, default `5555`).
    pub port: u16,
    /// Bind address (`--bind`, default `127.0.0.1`).
    pub bind: String,
    /// Whether `-h`/`--help` was passed.
    pub help: bool,
    /// Whether `--version` was passed.
    pub version: bool,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            mode: CliMode::Direct,
            source: None,
            command: None,
            file: None,
            output: None,
            serve: false,
            port: 5555,
            bind: "127.0.0.1".to_string(),
            help: false,
            version: false,
        }
    }
}

impl CliArgs {
    /// Resolve the SQL to run: the inline command, else the file contents, else
    /// an empty string.
    pub fn sql(&self) -> Result<String> {
        if let Some(command) = &self.command {
            return Ok(command.clone());
        }
        if let Some(file) = &self.file {
            return std::fs::read_to_string(file).map_err(io_error);
        }
        Ok(String::new())
    }

    /// Output format, defaulting to `csv` when unset.
    pub fn output_format(&self) -> &str {
        self.output.as_deref().unwrap_or("csv")
    }
}

/// Parse thinclient command-line arguments (skipping the program name), infer
/// the mode, and validate the combination.
pub fn parse_cli_args<I, S>(args: I) -> Result<CliArgs>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    let mut parsed = CliArgs::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => parsed.help = true,
            "--version" => parsed.version = true,
            "-s" | "--source" => parsed.source = Some(next_arg(&mut args, &arg)?),
            "-c" | "--command" => parsed.command = Some(next_arg(&mut args, &arg)?),
            "-f" | "--file" => parsed.file = Some(next_arg(&mut args, &arg)?),
            "-o" | "--output" => parsed.output = Some(next_arg(&mut args, &arg)?),
            "--serve" => parsed.serve = true,
            "--port" => {
                parsed.port = next_arg(&mut args, &arg)?
                    .parse::<u16>()
                    .map_err(|_| Error::Message("invalid port".to_string()))?;
            }
            "--bind" => parsed.bind = next_arg(&mut args, &arg)?,
            _ if arg.starts_with('-') => {
                return Err(Error::Message(format!("unknown option: {arg}")));
            }
            _ if parsed.source.is_none() => parsed.source = Some(arg),
            _ => return Err(Error::Message(format!("unexpected argument: {arg}"))),
        }
    }
    parsed.mode = detect_mode(&parsed);
    validate_cli_args(&parsed)?;
    Ok(parsed)
}

fn detect_mode(args: &CliArgs) -> CliMode {
    if args.serve {
        CliMode::Serve
    } else if args.source.is_none()
        && (args.port != CliArgs::default().port || args.command.is_some() || args.file.is_some())
    {
        CliMode::Client
    } else {
        CliMode::Direct
    }
}

fn validate_cli_args(args: &CliArgs) -> Result<()> {
    if args.help || args.version {
        return Ok(());
    }
    let has_sql = args.command.is_some() || args.file.is_some();
    match args.mode {
        CliMode::Direct => {
            if args.source.is_none() {
                return Err(Error::Message(
                    "No database specified. Use -s <database>".to_string(),
                ));
            }
            if !has_sql {
                return Err(Error::Message(
                    "No query specified. Use -c <query> or -f <file>".to_string(),
                ));
            }
        }
        CliMode::Serve => {
            if args.source.is_none() {
                return Err(Error::Message(
                    "No database specified for serve mode. Use -s <database>".to_string(),
                ));
            }
        }
        CliMode::Client => {
            if !has_sql {
                return Err(Error::Message(
                    "No query specified for client mode. Use -c <query> or -f <file>".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn next_arg(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| Error::Message(format!("missing argument for {option}")))
}
