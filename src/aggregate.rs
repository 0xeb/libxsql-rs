// Copyright (c) 2026 Elias Bachaalany
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::error::{Error, Result};
use crate::function::FunctionContext;
use libsqlite3_sys as ffi;
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::ptr::NonNull;

/// SQLite aggregate state and result context.
pub struct AggregateContext<'a> {
    raw: *mut ffi::sqlite3_context,
    _marker: PhantomData<&'a mut ffi::sqlite3_context>,
}

impl<'a> AggregateContext<'a> {
    pub(crate) fn new(raw: *mut ffi::sqlite3_context) -> Self {
        Self {
            raw,
            _marker: PhantomData,
        }
    }

    /// Returns a mutable reference to the per-aggregation state of type `T`,
    /// allocating it via `init` on the first call within this aggregation.
    pub fn state_or_init<T>(&mut self, init: impl FnOnce() -> T) -> Result<&mut T> {
        let slot = self.state_slot()?;
        if unsafe { *slot }.is_null() {
            let boxed = Box::new(init());
            unsafe {
                *slot = Box::into_raw(boxed).cast::<c_void>();
            }
        }
        let ptr = NonNull::new(unsafe { *slot }.cast::<T>())
            .ok_or_else(|| Error::Message("aggregate state allocation failed".to_string()))?;
        Ok(unsafe { ptr.as_ptr().as_mut().expect("non-null aggregate state") })
    }

    /// Takes ownership of the per-aggregation state of type `T`, clearing the
    /// slot; returns `None` if no state was ever initialized. Typically called
    /// from the final callback.
    pub fn take_state<T>(&mut self) -> Option<Box<T>> {
        let slot = self.state_slot().ok()?;
        let raw = unsafe { *slot };
        if raw.is_null() {
            return None;
        }
        unsafe {
            *slot = std::ptr::null_mut();
            Some(Box::from_raw(raw.cast::<T>()))
        }
    }

    /// Sets the aggregate's result to the given blob.
    pub fn result_blob(&mut self, data: &[u8]) {
        self.as_function_context().result_blob(data);
    }

    /// Sets the aggregate's result to SQL NULL.
    pub fn result_null(&mut self) {
        self.as_function_context().result_null();
    }

    /// Reports an error for this aggregate with the given message.
    pub fn result_error(&mut self, message: impl AsRef<str>) {
        self.as_function_context().result_error(message);
    }

    fn state_slot(&mut self) -> Result<*mut *mut c_void> {
        if self.raw.is_null() {
            return Err(Error::DatabaseNotOpen);
        }
        let ptr = unsafe {
            ffi::sqlite3_aggregate_context(self.raw, std::mem::size_of::<*mut c_void>() as i32)
        };
        NonNull::new(ptr.cast::<*mut c_void>())
            .map(NonNull::as_ptr)
            .ok_or_else(|| Error::Message("sqlite3_aggregate_context returned NULL".to_string()))
    }

    fn as_function_context(&mut self) -> FunctionContext<'_> {
        FunctionContext::new(self.raw)
    }
}
