// Copyright (c) 2024-2026 Elias Bachaalany
// SPDX-License-Identifier: LicenseRef-Human-Origin-Source-1.0
//
// This file is licensed under the Human-Origin Source License v1.0.
// See LICENSE.

//! gzip response compression for the thinclient servers (the `gzip` feature,
//! which `thinclient` enables).
//!
//! Mirrors the C++ servers' vendored httplib: a response is compressed only
//! when the client's `Accept-Encoding` names `gzip` and the response media type
//! is compressible. The decision reads the media type alone, ignoring
//! `;parameters`, so `text/event-stream; charset=utf-8` stays uncompressed (a
//! compressed event stream stalls in the compressor). Level 1 (fastest): a
//! query result is generated fresh for every request, so compression time is
//! paid on each one.

use std::io::{self, Write};

use flate2::Compression;
use flate2::write::GzEncoder;
use tiny_http::Header as TinyHeader;

/// Non-`text/*` media types that are compressed.
const COMPRESSIBLE_TYPES: [&str; 7] = [
    "image/svg+xml",
    "application/javascript",
    "application/json",
    "application/xml",
    "application/protobuf",
    "application/x-ndjson",
    "application/xhtml+xml",
];

/// Whether a response with this `Content-Type` may be gzip'd, judged on the
/// media type alone.
pub(crate) fn can_compress_content_type(content_type: &str) -> bool {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_end_matches([' ', '\t']);
    COMPRESSIBLE_TYPES.contains(&media_type)
        || (media_type.starts_with("text/") && media_type != "text/event-stream")
}

/// Whether the request's `Accept-Encoding` names gzip (a substring match, as
/// the C++ server does).
pub(crate) fn accepts_gzip(request_headers: &[TinyHeader]) -> bool {
    request_headers
        .iter()
        .filter(|header| header.field.equiv("Accept-Encoding"))
        .any(|header| header.value.as_str().contains("gzip"))
}

/// The encoding headers for a response whose `Content-Type` is compressible:
/// `Vary: Accept-Encoding` always (the representation depends on it), plus
/// `Content-Encoding: gzip` when the body is compressed.
pub(crate) fn encoding_headers(gzip: bool) -> Vec<TinyHeader> {
    let mut headers = vec![
        TinyHeader::from_bytes(b"Vary".as_slice(), b"Accept-Encoding".as_slice())
            .expect("static header"),
    ];
    if gzip {
        headers.push(
            TinyHeader::from_bytes(b"Content-Encoding".as_slice(), b"gzip".as_slice())
                .expect("static header"),
        );
    }
    headers
}

/// gzip a whole buffered body.
pub(crate) fn gzip(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(body)?;
    encoder.finish()
}

/// Incremental gzip for a chunked stream: each [`push`](Self::push) returns the
/// compressed bytes ready so far, [`finish`](Self::finish) the rest plus the
/// gzip trailer.
pub(crate) struct GzipStream {
    encoder: GzEncoder<Vec<u8>>,
}

impl GzipStream {
    pub(crate) fn new() -> Self {
        Self {
            encoder: GzEncoder::new(Vec::new(), Compression::fast()),
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> io::Result<Vec<u8>> {
        self.encoder.write_all(data)?;
        Ok(std::mem::take(self.encoder.get_mut()))
    }

    pub(crate) fn finish(self) -> io::Result<Vec<u8>> {
        self.encoder.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    fn gunzip(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        GzDecoder::new(data)
            .read_to_end(&mut out)
            .expect("valid gzip");
        out
    }

    // Mirrors the C++ HttpCompressionTest.MediaTypeParametersDoNotChangeTheDecision.
    #[test]
    fn media_type_parameters_do_not_change_the_decision() {
        assert!(!can_compress_content_type("text/event-stream"));
        assert!(!can_compress_content_type(
            "text/event-stream; charset=utf-8"
        ));
        assert!(!can_compress_content_type(
            "text/event-stream ;charset=utf-8"
        ));
        assert!(can_compress_content_type("application/json"));
        assert!(can_compress_content_type("application/json; charset=utf-8"));
        assert!(can_compress_content_type("application/x-ndjson"));
        assert!(can_compress_content_type("text/plain; charset=utf-8"));
        assert!(!can_compress_content_type("application/octet-stream"));
    }

    #[test]
    fn buffered_and_incremental_gzip_round_trip() {
        let body = "RepeatedSymbolName\n".repeat(5000);
        let whole = gzip(body.as_bytes()).expect("gzip");
        assert_eq!(&whole[..2], &[0x1f, 0x8b], "gzip magic");
        assert!(whole.len() < body.len() / 10, "{} bytes", whole.len());
        assert_eq!(gunzip(&whole), body.as_bytes());

        let mut stream = GzipStream::new();
        let mut out = Vec::new();
        for piece in body.as_bytes().chunks(777) {
            out.extend(stream.push(piece).expect("push"));
        }
        out.extend(stream.finish().expect("finish"));
        assert_eq!(gunzip(&out), body.as_bytes());
    }

    #[test]
    fn accept_encoding_is_a_substring_match() {
        let header = |value: &str| {
            TinyHeader::from_bytes(b"Accept-Encoding".as_slice(), value.as_bytes()).unwrap()
        };
        assert!(accepts_gzip(&[header("gzip")]));
        assert!(accepts_gzip(&[header("br, gzip;q=0.8")]));
        assert!(!accepts_gzip(&[header("identity")]));
        assert!(!accepts_gzip(&[]));
    }
}
