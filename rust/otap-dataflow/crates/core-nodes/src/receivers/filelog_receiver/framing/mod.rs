// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded source-byte decoding and logical-record framing.

mod decoder;
mod framer;

#[allow(unused_imports)]
pub(crate) use decoder::{DecodeError, SourceRange};
#[allow(unused_imports)]
pub(crate) use framer::{
    DecodeOutcome, FlushReason, FlushStep, FragmentMetadata, FramedBody, FramedRecord, Framer,
    FramerError, FramerStep, fragment_id,
};
