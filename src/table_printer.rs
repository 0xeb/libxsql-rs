// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

/// Table rendering style. Mirrors the C++ `xsql::cli::TableStyle`. `Borderless`
/// is the historical output; `Boxed` is the classic `+---+` / `| cell |` box that
/// pdbsql (and idasql) hand-rolled, now shareable.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TableStyle {
    /// Historical borderless columns-and-separator rendering.
    Borderless,
    /// Classic `+---+` / `| cell |` boxed rendering.
    Boxed,
}

/// Options controlling [`print_table`] output.
pub struct TablePrintOptions {
    /// Text emitted when a statement produces no columns. Mirrors the C++
    /// `xsql::cli::TablePrintOptions::no_result` field (defaults to "(no result)").
    pub no_result: String,
    /// Append a newline after [`Self::no_result`].
    pub newline_after_no_result: bool,
    /// Rendering style; defaults to `Borderless` so existing callers are byte-identical.
    pub style: TableStyle,
    /// Boxed style only: append a `N row(s)\n` footer after the box.
    pub boxed_row_count_footer: bool,
}

impl Default for TablePrintOptions {
    fn default() -> Self {
        Self {
            no_result: "(no result)".to_string(),
            newline_after_no_result: false,
            style: TableStyle::Borderless,
            boxed_row_count_footer: true,
        }
    }
}

/// Render `columns` and `rows` using `options`.
#[must_use]
pub fn print_table(columns: &[String], rows: &[Vec<String>], options: TablePrintOptions) -> String {
    if columns.is_empty() {
        // Boxed callers (pdbsql) print NOTHING for a column-less / empty result.
        if options.style == TableStyle::Boxed {
            return String::new();
        }
        return if options.newline_after_no_result {
            format!("{}\n", options.no_result)
        } else {
            options.no_result
        };
    }

    let mut widths = columns.iter().map(String::len).collect::<Vec<_>>();
    for row in rows {
        for (index, value) in row.iter().enumerate().take(widths.len()) {
            widths[index] = widths[index].max(value.len());
        }
    }

    if options.style == TableStyle::Boxed {
        return print_boxed(columns, rows, &widths, options.boxed_row_count_footer);
    }

    let mut out = String::new();
    write_table_row(&mut out, columns, &widths);
    let separator = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>();
    write_table_row(&mut out, &separator, &widths);
    for row in rows {
        write_table_row(&mut out, row, &widths);
    }
    out
}

/// Boxed `+---+` / `| cell | ` rendering (with the load-bearing trailing space
/// after each row's final `|`). Mirrors the C++ boxed branch byte-for-byte.
fn print_boxed(columns: &[String], rows: &[Vec<String>], widths: &[usize], footer: bool) -> String {
    // `+` + (width + 2) dashes per column + `+`.
    let mut rule = String::from("+");
    for width in widths {
        rule.push_str(&"-".repeat(width + 2));
        rule.push('+');
    }

    let mut out = String::new();
    out.push_str(&rule);
    out.push('\n');
    write_boxed_row(&mut out, columns, widths);
    out.push_str(&rule);
    out.push('\n');
    for row in rows {
        write_boxed_row(&mut out, row, widths);
    }
    out.push_str(&rule);
    out.push('\n');
    if footer {
        out.push_str(&format!("{} row(s)\n", rows.len()));
    }
    out
}

fn write_boxed_row(out: &mut String, cells: &[String], widths: &[usize]) {
    out.push_str("| ");
    for (index, &width) in widths.iter().enumerate() {
        let value = cells.get(index).map(String::as_str).unwrap_or("");
        out.push_str(value);
        if value.len() < width {
            out.push_str(&" ".repeat(width - value.len()));
        }
        out.push_str(" | ");
    }
    out.push('\n');
}

fn write_table_row(out: &mut String, cells: &[String], widths: &[usize]) {
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(cell);
        if index + 1 < cells.len() {
            let width = widths.get(index).copied().unwrap_or(cell.len());
            if cell.len() < width {
                out.push_str(&" ".repeat(width - cell.len()));
            }
        }
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::{TablePrintOptions, TableStyle, print_table};

    #[test]
    fn empty_table_uses_configured_newline_policy() {
        assert_eq!(
            print_table(&[], &[], TablePrintOptions::default()),
            "(no result)"
        );
        assert_eq!(
            print_table(
                &[],
                &[],
                TablePrintOptions {
                    newline_after_no_result: true,
                    ..Default::default()
                }
            ),
            "(no result)\n"
        );
    }

    #[test]
    fn empty_table_uses_custom_no_result_string() {
        assert_eq!(
            print_table(
                &[],
                &[],
                TablePrintOptions {
                    no_result: "<empty>".to_string(),
                    ..Default::default()
                }
            ),
            "<empty>"
        );
        assert_eq!(
            print_table(
                &[],
                &[],
                TablePrintOptions {
                    no_result: "<empty>".to_string(),
                    newline_after_no_result: true,
                    ..Default::default()
                }
            ),
            "<empty>\n"
        );
    }

    #[test]
    fn prints_aligned_columns() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec!["1".to_string(), "alpha".to_string()],
            vec!["200".to_string(), "b".to_string()],
        ];
        assert_eq!(
            print_table(&columns, &rows, TablePrintOptions::default()),
            "id   name\n---  -----\n1    alpha\n200  b\n"
        );
    }

    fn boxed() -> TablePrintOptions {
        TablePrintOptions {
            style: TableStyle::Boxed,
            ..Default::default()
        }
    }

    #[test]
    fn boxed_zero_columns_prints_nothing() {
        assert_eq!(print_table(&[], &[], boxed()), "");
    }

    #[test]
    fn boxed_golden_output() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec!["1".to_string(), "alpha".to_string()],
            vec!["200".to_string(), "b".to_string()],
        ];
        assert_eq!(
            print_table(&columns, &rows, boxed()),
            "+-----+-------+\n\
             | id  | name  | \n\
             +-----+-------+\n\
             | 1   | alpha | \n\
             | 200 | b     | \n\
             +-----+-------+\n\
             2 row(s)\n"
        );
    }

    #[test]
    fn boxed_zero_rows() {
        assert_eq!(
            print_table(&["x".to_string()], &[], boxed()),
            "+---+\n| x | \n+---+\n+---+\n0 row(s)\n"
        );
    }

    #[test]
    fn boxed_short_row_padded() {
        assert_eq!(
            print_table(
                &["a".to_string(), "b".to_string()],
                &[vec!["x".to_string()]],
                boxed()
            ),
            "+---+---+\n| a | b | \n+---+---+\n| x |   | \n+---+---+\n1 row(s)\n"
        );
    }

    #[test]
    fn boxed_no_footer() {
        assert_eq!(
            print_table(
                &["x".to_string()],
                &[vec!["1".to_string()]],
                TablePrintOptions {
                    style: TableStyle::Boxed,
                    boxed_row_count_footer: false,
                    ..Default::default()
                }
            ),
            "+---+\n| x | \n+---+\n| 1 | \n+---+\n"
        );
    }
}
