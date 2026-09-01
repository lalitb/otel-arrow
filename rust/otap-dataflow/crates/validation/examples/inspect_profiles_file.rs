// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Inspect length-prefixed OTLP Profiles protobuf frames written by the file exporter.

#![allow(clippy::print_stdout)]

use otel_arrow_dfe_core_nodes::exporters::file_exporter::framing::{
    OTLP_PROTO_FRAME_HEADER_BYTES, decode_otlp_proto_frame_header,
    validate_otlp_proto_frame_payload,
};
use otel_arrow_dfe_pdata::OtlpProtoBytes;
use otel_arrow_dfe_pdata::proto::OtlpProtoMessage;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::PathBuf;

const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Profiles file path required")
        })?;
    let mut reader = BufReader::new(File::open(&path)?);
    let mut frame_index = 0_usize;

    loop {
        let mut raw_header = [0_u8; OTLP_PROTO_FRAME_HEADER_BYTES];
        let first = reader.read(&mut raw_header[..1])?;
        if first == 0 {
            break;
        }
        reader.read_exact(&mut raw_header[1..])?;
        let header = decode_otlp_proto_frame_header(&raw_header)?;
        let frame_len = header.payload_len;
        if frame_len > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame {frame_index} exceeds {MAX_FRAME_BYTES} bytes"),
            )
            .into());
        }
        let mut frame = vec![0_u8; frame_len];
        reader.read_exact(&mut frame)?;
        validate_otlp_proto_frame_payload(header, &frame)?;
        let message: OtlpProtoMessage =
            OtlpProtoBytes::new_from_bytes(header.signal, frame).try_into()?;
        match message {
            OtlpProtoMessage::Profiles(profiles) => {
                let scope_profiles = profiles
                    .resource_profiles
                    .iter()
                    .map(|resource| resource.scope_profiles.len())
                    .sum::<usize>();
                let profile_count = profiles
                    .resource_profiles
                    .iter()
                    .flat_map(|resource| &resource.scope_profiles)
                    .map(|scope| scope.profiles.len())
                    .sum::<usize>();
                let sample_count = profiles
                    .resource_profiles
                    .iter()
                    .flat_map(|resource| &resource.scope_profiles)
                    .flat_map(|scope| &scope.profiles)
                    .map(|profile| profile.samples.len())
                    .sum::<usize>();
                let dictionary = profiles.dictionary.as_ref();
                println!(
                    "frame={frame_index} signal=profiles bytes={frame_len} resources={} scopes={scope_profiles} profiles={profile_count} samples={sample_count} mappings={} locations={} functions={} stacks={} attributes={}",
                    profiles.resource_profiles.len(),
                    dictionary.map_or(0, |value| value.mapping_table.len().saturating_sub(1)),
                    dictionary.map_or(0, |value| value.location_table.len().saturating_sub(1)),
                    dictionary.map_or(0, |value| value.function_table.len().saturating_sub(1)),
                    dictionary.map_or(0, |value| value.stack_table.len().saturating_sub(1)),
                    dictionary.map_or(0, |value| value.attribute_table.len().saturating_sub(1)),
                );
            }
            message => {
                println!(
                    "frame={frame_index} signal={:?} bytes={frame_len} items={}",
                    header.signal,
                    message.num_items()
                );
            }
        }
        frame_index += 1;
    }

    if frame_index == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Profiles file is empty").into());
    }
    Ok(())
}
