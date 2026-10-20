// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

use crate::error::{Error, Result};
use crate::function::FunctionContext;
use libsqlite3_sys as ffi;
use std::any::Any;
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
    pub fn state_or_init<T: 'static>(&mut self, init: impl FnOnce() -> T) -> Result<&mut T> {
        let slot = self.state_slot()?;
        if unsafe { *slot }.is_null() {
            let boxed: Box<Box<dyn Any>> = Box::new(Box::new(init()));
            unsafe {
                *slot = Box::into_raw(boxed).cast::<c_void>();
            }
        }
        let state = unsafe { &mut *(*slot).cast::<Box<dyn Any>>() };
        state
            .downcast_mut::<T>()
            .ok_or_else(|| Error::Message("aggregate state type mismatch".to_string()))
    }

    /// Takes ownership of the per-aggregation state of type `T`, clearing the
    /// slot; returns `Ok(None)` if no state was ever initialized. A type
    /// mismatch returns an error and preserves the existing state. Any state
    /// left in the slot is dropped after the final callback, including errors
    /// and panics.
    pub fn take_state<T: 'static>(&mut self) -> Result<Option<Box<T>>> {
        let slot = self.state_slot()?;
        let raw = unsafe { *slot };
        if raw.is_null() {
            return Ok(None);
        }
        let state = unsafe { &*raw.cast::<Box<dyn Any>>() };
        if !state.is::<T>() {
            return Err(Error::Message("aggregate state type mismatch".to_string()));
        }
        unsafe {
            *slot = std::ptr::null_mut();
            let state = *Box::from_raw(raw.cast::<Box<dyn Any>>());
            Ok(Some(state.downcast::<T>().map_err(|_| {
                Error::Message("aggregate state type mismatch".to_string())
            })?))
        }
    }

    pub(crate) fn clear_state(&mut self) {
        if let Ok(slot) = self.state_slot() {
            let raw = unsafe { *slot };
            if !raw.is_null() {
                unsafe {
                    *slot = std::ptr::null_mut();
                    drop(Box::from_raw(raw.cast::<Box<dyn Any>>()));
                }
            }
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
