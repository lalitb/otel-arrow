// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Module contains utilities for concatenating OTAP batches.
//!
//! This is used by pipeline stages that split/duplicate the batch, evaluate nested pipelines
//! on it, and concatenate the results to produce an output.

use arrow::array::RecordBatch;
use otel_arrow_dfe_pdata::OtapArrowRecords;
use otel_arrow_dfe_pdata::otap::raw_batch_store::{
    LOGS_LAYOUT, METRICS_LAYOUT, RawBatchStore, TRACES_LAYOUT, payload_position,
};
use otel_arrow_dfe_pdata::otap::transform::concatenate::concatenate;
use otel_arrow_dfe_pdata::otap::transform::reindex::reindex;
use otel_arrow_dfe_pdata::otap::{Logs, Metrics, OtapBatchStore, Profiles, Traces};
use otel_arrow_dfe_pdata::proto::opentelemetry::arrow::v1::ArrowPayloadType;

use crate::error::{Error, Result};

pub(crate) fn concat_generic<T, const LAYOUT: u8, const COUNT: usize>(
    branch_results: &mut Vec<OtapArrowRecords>,
) -> Result<OtapArrowRecords>
where
    T: OtapBatchStore<BatchArray = [Option<RecordBatch>; COUNT]>
        + TryFrom<RawBatchStore<LAYOUT, COUNT>, Error = otel_arrow_dfe_pdata::error::Error>
        + TryFrom<OtapArrowRecords, Error = otel_arrow_dfe_pdata::error::Error>,
    OtapArrowRecords: From<T>,
{
    let mut batches = Vec::new();
    for branch_result in branch_results.drain(..) {
        let batch_store: T = branch_result.try_into()?;
        batches.push(batch_store.into_batches())
    }

    let concatenated_batches = concatenate(&mut batches)?;
    let raw_store = RawBatchStore::<LAYOUT, COUNT>::from_batches(concatenated_batches);
    let result_store = T::try_from(raw_store)?;

    Ok(OtapArrowRecords::from(result_store))
}

pub(crate) fn concatenate_logs(
    branch_results: &mut Vec<OtapArrowRecords>,
) -> Result<OtapArrowRecords> {
    concat_generic::<Logs, LOGS_LAYOUT, { Logs::COUNT }>(branch_results)
}

pub(crate) fn concatenate_metrics(
    branch_results: &mut Vec<OtapArrowRecords>,
) -> Result<OtapArrowRecords> {
    concat_generic::<Metrics, METRICS_LAYOUT, { Metrics::COUNT }>(branch_results)
}

pub(crate) fn concatenate_traces(
    branch_results: &mut Vec<OtapArrowRecords>,
) -> Result<OtapArrowRecords> {
    concat_generic::<Traces, TRACES_LAYOUT, { Traces::COUNT }>(branch_results)
}

pub(crate) fn concatenate_profiles(
    branch_results: &mut Vec<OtapArrowRecords>,
) -> Result<OtapArrowRecords> {
    match branch_results.len() {
        0 => Ok(OtapArrowRecords::Profiles(Profiles::default())),
        1 => Ok(branch_results.pop().expect("one Profiles branch result")),
        _ => Err(Error::NotYetSupportedError {
            message: "concatenating Profiles requires profile graph reindexing".into(),
        }),
    }
}

pub(crate) fn concatenate_attrs_record_batches(
    branch_results: &mut Vec<RecordBatch>,
) -> Result<RecordBatch> {
    // to call schema aware `concatenate` on just the attributes record batch, we stick it in
    // a Logs OTAP batch and treat it as log attributes. This is a bit of a hack until we have
    // a better top-level interface for calling concatenate.

    let mut otap_batches = branch_results
        .drain(..)
        .map(|attrs_record_batch| {
            let mut logs_record_batches = Logs::default();
            logs_record_batches.set(ArrowPayloadType::LogAttrs, attrs_record_batch)?;
            Ok(OtapArrowRecords::from(logs_record_batches))
        })
        .collect::<Result<Vec<_>>>()?;
    let concatenated_logs = concatenate_logs(&mut otap_batches)?;
    let mut concatenated_logs_batches = Logs::try_from(concatenated_logs)?.into_batches();
    let concatenated_attrs_batch = concatenated_logs_batches
        [payload_position(ArrowPayloadType::LogAttrs).expect("log attributes position")]
    .take()
    .ok_or_else(|| Error::ExecutionError {
        cause: "expected concatenate to produce non 'None' batch".into(),
    })?;

    Ok(concatenated_attrs_batch)
}

pub(crate) fn reindex_generic<T, const LAYOUT: u8, const COUNT: usize>(
    branch_results: &mut Vec<OtapArrowRecords>,
) -> Result<()>
where
    T: OtapBatchStore<BatchArray = [Option<RecordBatch>; COUNT]>
        + TryFrom<RawBatchStore<LAYOUT, COUNT>, Error = otel_arrow_dfe_pdata::error::Error>
        + TryFrom<OtapArrowRecords, Error = otel_arrow_dfe_pdata::error::Error>,
    OtapArrowRecords: From<T>,
{
    let mut batches = Vec::new();
    for branch_result in branch_results.drain(..) {
        let batch_store: T = branch_result.try_into()?;
        batches.push(batch_store.into_batches())
    }

    reindex(&mut batches)?;
    for batch in batches {
        let raw_store = RawBatchStore::<LAYOUT, COUNT>::from_batches(batch);
        let result_store = T::try_from(raw_store)?;
        branch_results.push(OtapArrowRecords::from(result_store))
    }

    Ok(())
}

pub(crate) fn reindex_logs(branch_results: &mut Vec<OtapArrowRecords>) -> Result<()> {
    reindex_generic::<Logs, LOGS_LAYOUT, { Logs::COUNT }>(branch_results)
}

pub(crate) fn reindex_metrics(branch_results: &mut Vec<OtapArrowRecords>) -> Result<()> {
    reindex_generic::<Metrics, METRICS_LAYOUT, { Metrics::COUNT }>(branch_results)
}

pub(crate) fn reindex_traces(branch_results: &mut Vec<OtapArrowRecords>) -> Result<()> {
    reindex_generic::<Traces, TRACES_LAYOUT, { Traces::COUNT }>(branch_results)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: A Profiles conditional produces exactly one non-empty branch result.
    /// Guarantees: The result is returned unchanged without graph reindexing.
    #[test]
    fn concatenate_profiles_preserves_single_result() {
        let expected = OtapArrowRecords::Profiles(Profiles::default());
        let mut results = vec![expected.clone()];

        assert_eq!(concatenate_profiles(&mut results).unwrap(), expected);
        assert!(results.is_empty());
    }

    /// Scenario: A Profiles conditional produces multiple non-empty branch results.
    /// Guarantees: The query engine rejects unsafe graph concatenation with a typed error.
    #[test]
    fn concatenate_profiles_rejects_multiple_results() {
        let mut results = vec![
            OtapArrowRecords::Profiles(Profiles::default()),
            OtapArrowRecords::Profiles(Profiles::default()),
        ];

        assert!(matches!(
            concatenate_profiles(&mut results),
            Err(Error::NotYetSupportedError { .. })
        ));
        assert_eq!(results.len(), 2);
    }
}
