// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

//! Comparing a cached table's integer `index_on` keys against constraint values
//! the way SQLite compares an INTEGER column against them.
//!
//! SQLite hands xFilter the constraint's right-hand side untouched: no column
//! affinity is applied (measured: `end >= '15'` arrives as the text `'15'`), yet a
//! plan that sets `omit=1` must answer exactly what SQLite would. So the value
//! gets the column's INTEGER affinity here (well-formed numeric text becomes a
//! number) and is then ordered by SQLite's rules: NULL matches nothing, a real
//! compares exactly against the integer key, and any other text or a blob sorts
//! after every number.

use std::cmp::Ordering;

use std::borrow::Cow;

use super::{ConstraintOp, RowIterator};
use crate::function::{FunctionArg, FunctionContext};
use crate::value::ValueType;
use crate::{Error, Result};

/// 2^63: every i64 is below it and at or above its negation.
const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

/// A constraint value as an integer key compares against it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum KeyOperand {
    Int(i64),
    Real(f64),
    /// Non-numeric text or a blob: greater than every number.
    AboveNumbers,
    /// NULL (or an unknown storage class): no comparison is true.
    Null,
}

/// Classify `value`, applying INTEGER affinity to text.
pub(crate) fn key_operand(value: &FunctionArg<'_>) -> KeyOperand {
    match value.value_type() {
        ValueType::Integer => KeyOperand::Int(value.as_i64()),
        ValueType::Float => KeyOperand::Real(value.as_f64()),
        ValueType::Text => {
            // Apply comparison affinity to a copy: neither C-string parsing nor
            // storing into a NUMERIC column has the same full-length semantics.
            unsafe {
                let copy = libsqlite3_sys::sqlite3_value_dup(value.value().raw);
                assert!(!copy.is_null(), "cannot allocate numeric affinity value");
                let operand = match libsqlite3_sys::sqlite3_value_numeric_type(copy) {
                    libsqlite3_sys::SQLITE_INTEGER => {
                        KeyOperand::Int(libsqlite3_sys::sqlite3_value_int64(copy))
                    }
                    libsqlite3_sys::SQLITE_FLOAT => {
                        KeyOperand::Real(libsqlite3_sys::sqlite3_value_double(copy))
                    }
                    _ => KeyOperand::AboveNumbers,
                };
                libsqlite3_sys::sqlite3_value_free(copy);
                operand
            }
        }
        ValueType::Blob => KeyOperand::AboveNumbers,
        ValueType::Null | ValueType::Other(_) => KeyOperand::Null,
    }
}

/// Exact integer-versus-real comparison, as SQLite's `sqlite3IntFloatCompare`
/// (no rounding of large integers). `None` for NaN.
fn compare_int_to_real(key: i64, real: f64) -> Option<Ordering> {
    if real.is_nan() {
        return None;
    }
    if real >= TWO_POW_63 {
        return Some(Ordering::Less);
    }
    if real < -TWO_POW_63 {
        return Some(Ordering::Greater);
    }
    let whole = real.trunc();
    Some(key.cmp(&(whole as i64)).then(if real > whole {
        Ordering::Less
    } else if real < whole {
        Ordering::Greater
    } else {
        Ordering::Equal
    }))
}

/// How `key` orders against `operand`; `None` when no comparison can be true.
pub(crate) fn compare_key(key: i64, operand: KeyOperand) -> Option<Ordering> {
    match operand {
        KeyOperand::Int(value) => Some(key.cmp(&value)),
        KeyOperand::Real(value) => compare_int_to_real(key, value),
        KeyOperand::AboveNumbers => Some(Ordering::Less),
        KeyOperand::Null => None,
    }
}

/// Whether `key <op> operand` holds.
pub(crate) fn key_satisfies(key: i64, op: ConstraintOp, operand: KeyOperand) -> bool {
    let Some(ordering) = compare_key(key, operand) else {
        return false;
    };
    match op {
        ConstraintOp::Eq => ordering == Ordering::Equal,
        ConstraintOp::Gt => ordering == Ordering::Greater,
        ConstraintOp::Ge => ordering != Ordering::Less,
        ConstraintOp::Lt => ordering == Ordering::Less,
        ConstraintOp::Le => ordering != Ordering::Greater,
        // Not evaluable against an integer key (rejected when the filter is built).
        ConstraintOp::Like => false,
    }
}

/// The one integer key equal to `operand`, if any (an equality lookup).
pub(crate) fn exact_key(operand: KeyOperand) -> Option<i64> {
    match operand {
        KeyOperand::Int(value) => Some(value),
        KeyOperand::Real(value) => integral_key(value),
        KeyOperand::AboveNumbers | KeyOperand::Null => None,
    }
}

/// The `i64` that equals `value` under SQLite's comparison with an INTEGER
/// column, or `None` when no integer does.
///
/// Call this in a `constraint_filter` / `parametric_filter` factory instead of
/// [`FunctionArg::as_i64`]: SQLite hands the factory the constraint value with
/// no column affinity (`k = '10'` is text, `k = 10.5` a real), and those plans
/// are omit=1, so `as_i64` answers `k = 10.5` with key 10 and `k = NULL` with
/// key 0. `None` means the factory must yield no rows.
///
/// An integer is itself; a real only when it is integral and in `i64` range
/// (`10.0` is 10, `10.5` is `None`, compared exactly past 2^53); text only when
/// SQLite's INTEGER affinity makes it such a number (`'10'`, `' 10 '` and
/// `'1e1'` are 10; `'0x10'` and `'a'` are `None`); NULL, NaN and blobs never.
/// The built-in `filter_eq` plans use the same comparison.
pub fn exact_i64(value: &FunctionArg<'_>) -> Option<i64> {
    exact_key(key_operand(value))
}

/// The text that equals `value` under SQLite's comparison with a TEXT column,
/// or `None` when no text does (the factory then yields no rows).
///
/// Text is itself; a number gets TEXT affinity, which is its rendering (`10`
/// is `"10"`, `10.5` is `"10.5"`); NULL and blobs never equal text. The
/// built-in `filter_eq_text` plans use the same comparison.
pub fn exact_text<'a>(value: &FunctionArg<'a>) -> Option<Cow<'a, str>> {
    match value.value_type() {
        ValueType::Null | ValueType::Blob | ValueType::Other(_) => None,
        _ => Some(String::from_utf8_lossy(value.as_text_bytes())),
    }
}

/// An inclusive span of unsigned 64-bit keys, `lo <= hi`. Produced by
/// [`I64KeyRange::unsigned_spans`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct U64Span {
    /// The smallest key in the span.
    pub lo: u64,
    /// The largest key in the span.
    pub hi: u64,
}

/// The inclusive window of `i64` keys satisfying every comparison applied to
/// it, each compared the way SQLite compares an INTEGER column.
///
/// A `constraint_filter` factory over one key column narrows a window with each
/// argument and scans only [`lower`](Self::lower)..=[`upper`](Self::upper):
/// `constrain(Ge, 14.5)` keeps keys `>= 15`; `constrain(Lt, 'a')` keeps every
/// key (text sorts after all numbers); a NULL operand, or an `Eq` value no
/// integer can equal, empties the window. A new window holds every `i64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I64KeyRange {
    lower: i64,
    upper: i64,
    empty: bool,
}

impl Default for I64KeyRange {
    fn default() -> Self {
        Self::new()
    }
}

impl I64KeyRange {
    /// A window holding every `i64`.
    pub const fn new() -> Self {
        Self {
            lower: i64::MIN,
            upper: i64::MAX,
            empty: false,
        }
    }

    /// Narrow the window to keys satisfying `key <op> value`. Returns
    /// `Ok(false)` once the window is empty.
    ///
    /// # Errors
    ///
    /// LIKE is not a key comparison: it empties the window and returns an
    /// error, so a mis-routed constraint can fail the query (for instance from
    /// the generator's first `next`) instead of matching.
    pub fn constrain(&mut self, op: ConstraintOp, value: &FunctionArg<'_>) -> Result<bool> {
        self.constrain_operand(op, key_operand(value))
    }

    /// Narrow the window to keys satisfying `key <op> value` for an integer the
    /// caller resolved itself (a symbol name parsed to an address, say).
    ///
    /// # Errors
    ///
    /// As [`constrain`](Self::constrain): LIKE empties the window and errors.
    pub fn constrain_i64(&mut self, op: ConstraintOp, value: i64) -> Result<bool> {
        self.constrain_operand(op, KeyOperand::Int(value))
    }

    /// Whether no key satisfies the comparisons applied so far.
    pub const fn is_empty(&self) -> bool {
        self.empty
    }

    /// The smallest key in the window (`i64::MIN` when unbounded below).
    /// Meaningless when [`is_empty`](Self::is_empty).
    pub const fn lower(&self) -> i64 {
        self.lower
    }

    /// The largest key in the window (`i64::MAX` when unbounded above).
    /// Meaningless when [`is_empty`](Self::is_empty).
    pub const fn upper(&self) -> i64 {
        self.upper
    }

    /// Whether `key` satisfies every comparison applied so far.
    pub const fn contains(&self, key: i64) -> bool {
        !self.empty && key >= self.lower && key <= self.upper
    }

    /// The window as unsigned 64-bit spans, in the order SQL sorts the keys.
    ///
    /// For tables whose key is really an unsigned 64-bit value (an address)
    /// stored as a bit-cast `i64`. Keys `>= 2^63` reach SQL as negative
    /// integers, so SQL order is the high half first (most negative first),
    /// then 0, then the low half, while the engine walks unsigned. A walker
    /// that claims `ORDER BY key`, or omits the constraint, must visit the
    /// spans in the order returned here:
    ///
    /// | window | spans |
    /// |---|---|
    /// | empty | none |
    /// | `lo, hi >= 0` | `[lo, hi]` |
    /// | `lo, hi < 0` | `[u(lo), u(hi)]` |
    /// | `lo < 0 <= hi` | `[u(lo), u64::MAX]`, then `[0, hi]` |
    ///
    /// where `u()` is the bit-cast to `u64`. `descending` reverses the order
    /// of the spans; each span stays `lo <= hi`, and the walker decides how to
    /// walk it (downward, for a descending walk). A span only says what SQL
    /// order requires: it does not clamp to anything the engine can address.
    ///
    /// Gotcha for anyone writing such queries: SQLite reads a decimal literal
    /// `>= 2^63` (`18446744073709551613`) as a REAL, which no INTEGER key
    /// equals and which bounds the window past every key. Write a hex literal
    /// (`0xFFFFFFFFFFFFFFFD`, read as 64-bit two's complement) or the negative
    /// decimal (`-3`). And `key >= 0xFFFFF80000000000` also includes every
    /// low-half key; `key BETWEEN 0xFFFFF80000000000 AND -1` is the high half.
    pub fn unsigned_spans(&self, descending: bool) -> Vec<U64Span> {
        if self.empty || self.lower > self.upper {
            return Vec::new();
        }
        let lo = self.lower.cast_unsigned();
        let hi = self.upper.cast_unsigned();
        let mut spans = if self.lower < 0 && self.upper >= 0 {
            vec![U64Span { lo, hi: u64::MAX }, U64Span { lo: 0, hi }]
        } else {
            vec![U64Span { lo, hi }]
        };
        if descending {
            spans.reverse();
        }
        spans
    }

    fn constrain_operand(&mut self, op: ConstraintOp, operand: KeyOperand) -> Result<bool> {
        match op {
            ConstraintOp::Eq => match exact_key(operand) {
                Some(key) => {
                    self.apply_lower(KeyBound::Inclusive(key));
                    self.apply_upper(KeyBound::Inclusive(key));
                }
                None => self.empty = true,
            },
            ConstraintOp::Gt | ConstraintOp::Ge => {
                self.apply_lower(lower_bound(operand, op == ConstraintOp::Gt));
            }
            ConstraintOp::Lt | ConstraintOp::Le => {
                self.apply_upper(upper_bound(operand, op == ConstraintOp::Lt));
            }
            ConstraintOp::Like => {
                self.empty = true;
                return Err(Error::Message(
                    "LIKE is not an integer key comparison".to_string(),
                ));
            }
        }
        if self.lower > self.upper {
            self.empty = true;
        }
        Ok(!self.empty)
    }

    fn apply_lower(&mut self, bound: KeyBound) {
        match bound {
            KeyBound::Empty => self.empty = true,
            KeyBound::Unbounded => {}
            KeyBound::Inclusive(value) => self.lower = self.lower.max(value),
        }
    }

    fn apply_upper(&mut self, bound: KeyBound) {
        match bound {
            KeyBound::Empty => self.empty = true,
            KeyBound::Unbounded => {}
            KeyBound::Inclusive(value) => self.upper = self.upper.min(value),
        }
    }
}

/// The row source of an equality plan whose value no key can equal.
pub(crate) struct NoRows;

impl RowIterator for NoRows {
    fn next(&mut self) -> Result<bool> {
        Ok(false)
    }

    fn eof(&self) -> bool {
        true
    }

    fn column(&self, _ctx: &mut FunctionContext<'_>, _col: i32) -> Result<()> {
        Ok(())
    }

    fn rowid(&self) -> i64 {
        0
    }
}

/// An integral real in i64 range, as that integer.
fn integral_key(value: f64) -> Option<i64> {
    (value.fract() == 0.0 && (-TWO_POW_63..TWO_POW_63).contains(&value)).then_some(value as i64)
}

/// One side of a range over integer keys, after SQLite's comparison rules.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum KeyBound {
    /// No key satisfies the bound.
    Empty,
    /// Every key satisfies the bound.
    Unbounded,
    /// Keys at or beyond this value (inclusive) satisfy it.
    Inclusive(i64),
}

/// The inclusive lower key bound for `key > operand` (`strict`) or
/// `key >= operand`.
pub(crate) fn lower_bound(operand: KeyOperand, strict: bool) -> KeyBound {
    match operand {
        KeyOperand::Null | KeyOperand::AboveNumbers => KeyBound::Empty,
        KeyOperand::Int(value) if strict => value
            .checked_add(1)
            .map_or(KeyBound::Empty, KeyBound::Inclusive),
        KeyOperand::Int(value) => KeyBound::Inclusive(value),
        KeyOperand::Real(value) if value.is_nan() => KeyBound::Empty,
        KeyOperand::Real(value) => {
            // key > r  <=>  key >= floor(r) + 1;  key >= r  <=>  key >= ceil(r).
            let whole = if strict { value.floor() } else { value.ceil() };
            if whole >= TWO_POW_63 {
                KeyBound::Empty
            } else if whole < -TWO_POW_63 {
                KeyBound::Unbounded
            } else if strict {
                (whole as i64)
                    .checked_add(1)
                    .map_or(KeyBound::Empty, KeyBound::Inclusive)
            } else {
                KeyBound::Inclusive(whole as i64)
            }
        }
    }
}

/// The inclusive upper key bound for `key < operand` (`strict`) or
/// `key <= operand`.
pub(crate) fn upper_bound(operand: KeyOperand, strict: bool) -> KeyBound {
    match operand {
        KeyOperand::Null => KeyBound::Empty,
        // Every number sorts before text and blobs.
        KeyOperand::AboveNumbers => KeyBound::Unbounded,
        KeyOperand::Int(value) if strict => value
            .checked_sub(1)
            .map_or(KeyBound::Empty, KeyBound::Inclusive),
        KeyOperand::Int(value) => KeyBound::Inclusive(value),
        KeyOperand::Real(value) if value.is_nan() => KeyBound::Empty,
        KeyOperand::Real(value) => {
            // key < r  <=>  key <= ceil(r) - 1;  key <= r  <=>  key <= floor(r).
            let whole = if strict { value.ceil() } else { value.floor() };
            if whole < -TWO_POW_63 {
                KeyBound::Empty
            } else if whole >= TWO_POW_63 {
                KeyBound::Unbounded
            } else if strict {
                (whole as i64)
                    .checked_sub(1)
                    .map_or(KeyBound::Empty, KeyBound::Inclusive)
            } else {
                KeyBound::Inclusive(whole as i64)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equality_lookups_need_an_exactly_equal_integer() {
        assert_eq!(exact_key(KeyOperand::Int(7)), Some(7));
        assert_eq!(exact_key(KeyOperand::Real(7.0)), Some(7));
        assert_eq!(exact_key(KeyOperand::Real(7.5)), None);
        assert_eq!(exact_key(KeyOperand::Real(TWO_POW_63)), None);
        assert_eq!(exact_key(KeyOperand::Real(-TWO_POW_63)), Some(i64::MIN));
        assert_eq!(exact_key(KeyOperand::AboveNumbers), None);
        assert_eq!(exact_key(KeyOperand::Null), None);
    }

    #[test]
    fn range_bounds_match_the_comparison_they_stand_for() {
        use KeyBound::{Empty, Inclusive, Unbounded};
        assert_eq!(lower_bound(KeyOperand::Real(14.5), false), Inclusive(15));
        assert_eq!(lower_bound(KeyOperand::Real(14.5), true), Inclusive(15));
        assert_eq!(lower_bound(KeyOperand::Real(15.0), true), Inclusive(16));
        assert_eq!(lower_bound(KeyOperand::Int(i64::MAX), true), Empty);
        assert_eq!(lower_bound(KeyOperand::Real(1e300), false), Empty);
        assert_eq!(lower_bound(KeyOperand::Real(-1e300), true), Unbounded);
        assert_eq!(lower_bound(KeyOperand::AboveNumbers, false), Empty);
        assert_eq!(lower_bound(KeyOperand::Null, false), Empty);
        assert_eq!(upper_bound(KeyOperand::Real(14.5), false), Inclusive(14));
        assert_eq!(upper_bound(KeyOperand::Real(14.5), true), Inclusive(14));
        assert_eq!(upper_bound(KeyOperand::Real(15.0), true), Inclusive(14));
        assert_eq!(upper_bound(KeyOperand::Int(i64::MIN), true), Empty);
        assert_eq!(upper_bound(KeyOperand::Real(1e300), true), Unbounded);
        assert_eq!(upper_bound(KeyOperand::Real(-1e300), false), Empty);
        assert_eq!(upper_bound(KeyOperand::AboveNumbers, true), Unbounded);
        assert_eq!(upper_bound(KeyOperand::Real(f64::NAN), false), Empty);
        // Each bound agrees with key_satisfies on keys around it.
        for operand in [
            KeyOperand::Int(-3),
            KeyOperand::Real(2.5),
            KeyOperand::Real(-2.5),
            KeyOperand::Real(4.0),
        ] {
            for strict in [false, true] {
                for key in -8..8 {
                    let (low_op, high_op) = if strict {
                        (ConstraintOp::Gt, ConstraintOp::Lt)
                    } else {
                        (ConstraintOp::Ge, ConstraintOp::Le)
                    };
                    let within = |bound: KeyBound, above: bool| match bound {
                        Empty => false,
                        Unbounded => true,
                        Inclusive(limit) => (key >= limit) == above || key == limit,
                    };
                    assert_eq!(
                        within(lower_bound(operand, strict), true),
                        key_satisfies(key, low_op, operand),
                        "{operand:?} strict={strict} key={key} (lower)"
                    );
                    assert_eq!(
                        within(upper_bound(operand, strict), false),
                        key_satisfies(key, high_op, operand),
                        "{operand:?} strict={strict} key={key} (upper)"
                    );
                }
            }
        }
    }

    #[test]
    fn comparisons_follow_sqlite_ordering() {
        assert!(key_satisfies(10, ConstraintOp::Eq, KeyOperand::Int(10)));
        assert!(!key_satisfies(10, ConstraintOp::Eq, KeyOperand::Real(10.5)));
        assert!(key_satisfies(10, ConstraintOp::Eq, KeyOperand::Real(10.0)));
        // i64::MAX is below 2^63, which is exactly representable as a real.
        assert!(!key_satisfies(
            i64::MAX,
            ConstraintOp::Eq,
            KeyOperand::Real(9_223_372_036_854_775_808.0)
        ));
        assert!(key_satisfies(
            i64::MAX,
            ConstraintOp::Lt,
            KeyOperand::Real(9_223_372_036_854_775_808.0)
        ));
        assert!(key_satisfies(15, ConstraintOp::Ge, KeyOperand::Real(14.5)));
        assert!(!key_satisfies(14, ConstraintOp::Ge, KeyOperand::Real(14.5)));
        assert!(key_satisfies(
            i64::MAX,
            ConstraintOp::Lt,
            KeyOperand::AboveNumbers
        ));
        assert!(!key_satisfies(
            i64::MAX,
            ConstraintOp::Ge,
            KeyOperand::AboveNumbers
        ));
        for op in [
            ConstraintOp::Eq,
            ConstraintOp::Gt,
            ConstraintOp::Ge,
            ConstraintOp::Lt,
            ConstraintOp::Le,
        ] {
            assert!(!key_satisfies(0, op, KeyOperand::Null), "{op:?}");
            assert!(!key_satisfies(0, op, KeyOperand::Real(f64::NAN)), "{op:?}");
        }
        assert!(!key_satisfies(1, ConstraintOp::Like, KeyOperand::Int(1)));
    }
}
