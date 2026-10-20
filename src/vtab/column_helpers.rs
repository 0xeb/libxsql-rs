// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use super::index_key::exact_i64;
use super::set_vtab_error;
use crate::function::{FunctionArg, FunctionContext};
use crate::value::ValueType;
use std::ffi::CStr;

type RowGetter<Row> = dyn for<'a> Fn(&mut FunctionContext<'a>, &Row) + 'static;
type RowSetter<Row> = dyn for<'a> Fn(&mut Row, &FunctionArg<'a>) -> bool + 'static;
type IndexGetter = dyn for<'a> Fn(&mut FunctionContext<'a>, usize) + 'static;
type IndexSetter = dyn for<'a> Fn(usize, &FunctionArg<'a>) -> bool + 'static;

/// The integer a typed `INTEGER` (`i64`) column's setter receives: the one
/// that equals `value` under SQLite's comparison with an INTEGER column
/// ([`exact_i64`]: `16`, `16.0`, `'16'`, `' 16 '` and `'1e1'` are integers).
/// SQLite applies no column affinity to the values an UPDATE hands the vtab, so
/// this is where that spelling becomes the integer. Anything else -- NULL, a
/// blob, a non-integral or out-of-range real, non-integer text -- sets the
/// refusal reason (`column "<column>": expected an integer, got ...`) and
/// returns `None`: the setter does not run.
pub(super) fn exact_setter_i64(value: &FunctionArg<'_>, column: &str) -> Option<i64> {
    if let Some(key) = exact_i64(value) {
        return Some(key);
    }
    let got = match value.value_type() {
        ValueType::Float => "a non-integral or out-of-range real",
        ValueType::Text => "non-integer text",
        ValueType::Blob => "a blob",
        ValueType::Integer | ValueType::Null | ValueType::Other(_) => "NULL",
    };
    set_vtab_error(format!(
        "column \"{column}\": expected an integer, got {got}"
    ));
    None
}

/// [`exact_setter_i64`] for an `i32` setter, which also refuses an exact
/// integer outside `i32` (`column "<column>": <value> does not fit a 32-bit
/// integer`).
pub(super) fn exact_setter_int(value: &FunctionArg<'_>, column: &str) -> Option<i32> {
    let wide = exact_setter_i64(value, column)?;
    let narrow = i32::try_from(wide).ok();
    if narrow.is_none() {
        set_vtab_error(format!(
            "column \"{column}\": {wide} does not fit a 32-bit integer"
        ));
    }
    narrow
}

pub(super) fn row_getter_null<Row>() -> Box<RowGetter<Row>> {
    Box::new(|ctx, _row| ctx.result_null())
}

pub(super) fn row_getter_int<Row, F>(getter: F) -> Box<RowGetter<Row>>
where
    F: Fn(&Row) -> i32 + 'static,
{
    Box::new(move |ctx, row| ctx.result_int(getter(row)))
}

pub(super) fn row_getter_i64<Row, F>(getter: F) -> Box<RowGetter<Row>>
where
    F: Fn(&Row) -> i64 + 'static,
{
    Box::new(move |ctx, row| ctx.result_i64(getter(row)))
}

pub(super) fn row_getter_text<Row, F>(getter: F) -> Box<RowGetter<Row>>
where
    F: Fn(&Row) -> String + 'static,
{
    Box::new(move |ctx, row| ctx.result_text(getter(row)))
}

pub(super) fn row_getter_nullable_text<Row, F>(getter: F) -> Box<RowGetter<Row>>
where
    F: Fn(&Row) -> Option<String> + 'static,
{
    Box::new(move |ctx, row| match getter(row) {
        Some(value) => ctx.result_text(value),
        None => ctx.result_null(),
    })
}

pub(super) fn row_getter_nullable_int<Row, F>(getter: F) -> Box<RowGetter<Row>>
where
    F: Fn(&Row) -> Option<i32> + 'static,
{
    Box::new(move |ctx, row| match getter(row) {
        Some(value) => ctx.result_int(value),
        None => ctx.result_null(),
    })
}

pub(super) fn row_getter_double<Row, F>(getter: F) -> Box<RowGetter<Row>>
where
    F: Fn(&Row) -> f64 + 'static,
{
    Box::new(move |ctx, row| ctx.result_double(getter(row)))
}

pub(super) fn row_getter_blob<Row, F>(getter: F) -> Box<RowGetter<Row>>
where
    F: Fn(&Row) -> Vec<u8> + 'static,
{
    Box::new(move |ctx, row| ctx.result_blob(&getter(row)))
}

pub(super) fn row_setter_int<Row, S>(column: String, setter: S) -> Box<RowSetter<Row>>
where
    S: Fn(&mut Row, i32) -> bool + 'static,
{
    Box::new(move |row, value| exact_setter_int(value, &column).is_some_and(|v| setter(row, v)))
}

pub(super) fn row_setter_i64<Row, S>(column: String, setter: S) -> Box<RowSetter<Row>>
where
    S: Fn(&mut Row, i64) -> bool + 'static,
{
    Box::new(move |row, value| exact_setter_i64(value, &column).is_some_and(|v| setter(row, v)))
}

pub(super) fn row_setter_text<Row, S>(setter: S) -> Box<RowSetter<Row>>
where
    S: Fn(&mut Row, &str) -> bool + 'static,
{
    Box::new(move |row, value| {
        let text = value.as_c_str().map(CStr::to_string_lossy);
        setter(row, text.as_deref().unwrap_or(""))
    })
}

pub(super) fn row_setter_double<Row, S>(setter: S) -> Box<RowSetter<Row>>
where
    S: Fn(&mut Row, f64) -> bool + 'static,
{
    Box::new(move |row, value| setter(row, value.as_f64()))
}

pub(super) fn row_setter_blob<Row, S>(setter: S) -> Box<RowSetter<Row>>
where
    S: Fn(&mut Row, &[u8]) -> bool + 'static,
{
    Box::new(move |row, value| setter(row, value.as_blob()))
}

pub(super) fn index_getter_int<F>(getter: F) -> Box<IndexGetter>
where
    F: Fn(usize) -> i32 + 'static,
{
    Box::new(move |ctx, row| ctx.result_int(getter(row)))
}

pub(super) fn index_getter_i64<F>(getter: F) -> Box<IndexGetter>
where
    F: Fn(usize) -> i64 + 'static,
{
    Box::new(move |ctx, row| ctx.result_i64(getter(row)))
}

pub(super) fn index_getter_text<F>(getter: F) -> Box<IndexGetter>
where
    F: Fn(usize) -> String + 'static,
{
    Box::new(move |ctx, row| ctx.result_text(getter(row)))
}

pub(super) fn index_getter_nullable_text<F>(getter: F) -> Box<IndexGetter>
where
    F: Fn(usize) -> Option<String> + 'static,
{
    Box::new(move |ctx, row| match getter(row) {
        Some(value) => ctx.result_text(value),
        None => ctx.result_null(),
    })
}

pub(super) fn index_getter_double<F>(getter: F) -> Box<IndexGetter>
where
    F: Fn(usize) -> f64 + 'static,
{
    Box::new(move |ctx, row| ctx.result_double(getter(row)))
}

pub(super) fn index_getter_blob<F>(getter: F) -> Box<IndexGetter>
where
    F: Fn(usize) -> Vec<u8> + 'static,
{
    Box::new(move |ctx, row| ctx.result_blob(&getter(row)))
}

pub(super) fn index_setter_int<S>(column: String, setter: S) -> Box<IndexSetter>
where
    S: Fn(usize, i32) -> bool + 'static,
{
    Box::new(move |row, value| exact_setter_int(value, &column).is_some_and(|v| setter(row, v)))
}

pub(super) fn index_setter_i64<S>(column: String, setter: S) -> Box<IndexSetter>
where
    S: Fn(usize, i64) -> bool + 'static,
{
    Box::new(move |row, value| exact_setter_i64(value, &column).is_some_and(|v| setter(row, v)))
}

pub(super) fn index_setter_text<S>(setter: S) -> Box<IndexSetter>
where
    S: Fn(usize, &str) -> bool + 'static,
{
    Box::new(move |row, value| {
        let text = value.as_c_str().map(CStr::to_string_lossy);
        setter(row, text.as_deref().unwrap_or(""))
    })
}
