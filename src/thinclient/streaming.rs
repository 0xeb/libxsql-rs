// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

//! Chunked `/query` responses for a `streaming_executor`.
//!
//! The executor runs on a scoped producer thread and writes through a
//! [`StreamWriteCoalescer`] into a bounded channel. The connection thread hands
//! `tiny_http` a [`Read`] adapter over that channel with no content length, which
//! `tiny_http` sends with `Transfer-Encoding: chunked` to an HTTP/1.1 client (an
//! HTTP/1.0 client gets the body buffered, with a `Content-Length`). A client that
//! disconnects drops the reader, so the next channel send fails and the sink
//! reports `false` to the executor. When the client accepts gzip, the producer
//! compresses each coalesced batch before it enters the channel.

use std::io::{self, Read};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use tiny_http::{Request as TinyRequest, Response as TinyResponse, StatusCode};

use super::compress::{self, GzipStream};
use super::http_query_server::{content_type_header, respond};
use super::json::json_error;
use super::{StreamingExecutorFn, io_error};
use crate::{Result, ScriptOptions};

/// Batches a streaming executor's per-row writes into larger physical writes.
///
/// A streaming executor writes once per row. Sending each of those as its own
/// chunked-transfer frame costs several times more than the bytes themselves, so
/// this buffers them and calls the real writer once `flush_threshold_bytes` have
/// accumulated. The bytes on the wire are identical to the unbatched form.
///
/// Once the real writer returns `false` (a disconnected client), the failure
/// latches: every later [`write`](Self::write) returns `false` and
/// [`flush`](Self::flush) does nothing, without calling the real writer again.
/// Mirrors the C++ `xsql::thinclient::StreamWriteCoalescer`.
///
/// ```
/// use libxsql::thinclient::StreamWriteCoalescer;
///
/// let mut sent = Vec::new();
/// let mut coalescer = StreamWriteCoalescer::with_threshold(
///     |bytes: &[u8]| {
///         sent.extend_from_slice(bytes);
///         true
///     },
///     8,
/// );
/// assert!(coalescer.write(b"abc"));
/// assert_eq!(coalescer.real_write_count(), 0);
/// assert!(coalescer.write(b"defgh"));
/// assert_eq!(coalescer.real_write_count(), 1);
/// coalescer.flush();
/// drop(coalescer);
/// assert_eq!(sent, b"abcdefgh");
/// ```
pub struct StreamWriteCoalescer<W>
where
    W: FnMut(&[u8]) -> bool,
{
    real_write: W,
    flush_threshold_bytes: usize,
    buffer: Vec<u8>,
    failed: bool,
    real_write_count: usize,
}

impl<W> StreamWriteCoalescer<W>
where
    W: FnMut(&[u8]) -> bool,
{
    /// Flush threshold used by [`Self::new`] and by the HTTP server: 64 KiB.
    pub const DEFAULT_FLUSH_THRESHOLD_BYTES: usize = 64 * 1024;

    /// Wrap `real_write` with the default 64 KiB buffer.
    pub fn new(real_write: W) -> Self {
        Self::with_threshold(real_write, Self::DEFAULT_FLUSH_THRESHOLD_BYTES)
    }

    /// Wrap `real_write`, flushing once `flush_threshold_bytes` are buffered
    /// (`0` flushes on every write).
    pub fn with_threshold(real_write: W, flush_threshold_bytes: usize) -> Self {
        Self {
            real_write,
            flush_threshold_bytes,
            buffer: Vec::with_capacity(flush_threshold_bytes.saturating_add(4096)),
            failed: false,
            real_write_count: 0,
        }
    }

    /// Buffer `data`, flushing when the threshold is reached. Returns `false`
    /// once a real write has failed.
    pub fn write(&mut self, data: &[u8]) -> bool {
        if self.failed {
            return false;
        }
        self.buffer.extend_from_slice(data);
        if self.buffer.len() >= self.flush_threshold_bytes {
            self.flush();
        }
        !self.failed
    }

    /// Send whatever is buffered. Idempotent: a no-op when nothing is buffered
    /// or a real write already failed.
    pub fn flush(&mut self) {
        if self.buffer.is_empty() || self.failed {
            return;
        }
        self.real_write_count += 1;
        if !(self.real_write)(&self.buffer) {
            self.failed = true;
        }
        self.buffer.clear();
    }

    /// How many times the real writer was called. For tests of the batching
    /// itself; production code has no use for it.
    pub fn real_write_count(&self) -> usize {
        self.real_write_count
    }
}

/// Everything a streamed `/query` needs once admission is done.
pub(crate) struct StreamJob {
    pub(crate) executor: Arc<StreamingExecutorFn>,
    pub(crate) sql: String,
    pub(crate) opts: ScriptOptions,
    pub(crate) ndjson: bool,
}

/// Bounded so a stalled client holds at most a few coalesced batches in memory.
const STREAM_CHANNEL_DEPTH: usize = 4;

enum StreamChunk {
    Data(Vec<u8>),
    Failed(String),
}

/// Run `job` and send its output as one chunked response. Returns only after
/// the executor has finished, so a serialize guard held by the caller covers
/// the whole streamed execution.
pub(crate) fn respond_streaming(request: TinyRequest, job: &StreamJob) -> Result<()> {
    let content_type = if job.ndjson {
        "application/x-ndjson"
    } else {
        "application/json"
    };
    let gzip = compress::accepts_gzip(request.headers());
    let (tx, rx) = mpsc::sync_channel(STREAM_CHANNEL_DEPTH);
    thread::scope(|scope| {
        scope.spawn(move || produce(job, gzip, tx));
        let mut reader = ChannelReader {
            rx,
            current: Vec::new(),
            pos: 0,
        };
        // The status line is held back until the executor has produced its
        // first batch (or finished), so a failure before any output is a real
        // 500 instead of a 200 with a truncated body.
        match reader.rx.recv() {
            Ok(StreamChunk::Failed(message)) => {
                respond(request, 500, "application/json", &json_error(message))
            }
            Ok(StreamChunk::Data(first)) => {
                reader.current = first;
                send_chunked(request, content_type, gzip, reader)
            }
            Err(_) => send_chunked(request, content_type, gzip, reader),
        }
    })
}

/// Run the executor, sending its coalesced (and, for `gzip`, compressed)
/// output down `tx`. Both stream content types are compressible, so `gzip` is
/// just whether the client accepts it.
fn produce(job: &StreamJob, gzip: bool, tx: SyncSender<StreamChunk>) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let send = |bytes: Vec<u8>| bytes.is_empty() || tx.send(StreamChunk::Data(bytes)).is_ok();
        let mut encoder = gzip.then(GzipStream::new);
        let mut coalescer = StreamWriteCoalescer::new(|bytes: &[u8]| match encoder.as_mut() {
            Some(encoder) => encoder.push(bytes).is_ok_and(send),
            None => send(bytes.to_vec()),
        });
        {
            let mut sink = |chunk: &str| coalescer.write(chunk.as_bytes());
            (job.executor)(&job.sql, &job.opts, job.ndjson, &mut sink);
        }
        coalescer.flush();
        drop(coalescer);
        if let Some(encoder) = encoder
            && let Ok(tail) = encoder.finish()
        {
            send(tail);
        }
    }));
    if outcome.is_err() {
        let _ = tx.send(StreamChunk::Failed(
            "thinclient streaming executor panicked".to_string(),
        ));
    }
}

fn send_chunked(
    request: TinyRequest,
    content_type: &str,
    gzip: bool,
    reader: ChannelReader,
) -> Result<()> {
    let mut headers = vec![content_type_header(content_type)?];
    headers.extend(compress::encoding_headers(gzip));
    // No data length: tiny_http frames the body with Transfer-Encoding: chunked.
    let response = TinyResponse::new(StatusCode(200), headers, reader, None, None);
    request.respond(response).map_err(io_error)
}

/// `Read` over the producer's channel; EOF once the producer is done.
struct ChannelReader {
    rx: Receiver<StreamChunk>,
    current: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.current.len() {
            match self.rx.recv() {
                Ok(StreamChunk::Data(bytes)) => {
                    self.current = bytes;
                    self.pos = 0;
                }
                Ok(StreamChunk::Failed(message)) => return Err(io::Error::other(message)),
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len() - self.pos);
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
