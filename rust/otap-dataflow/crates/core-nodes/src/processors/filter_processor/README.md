# Filter Processor

<!-- markdownlint-disable MD013 -->

## Metadata

- Type: `processor:filter` (`urn:otel:processor:filter`)
- Feature gate: Default
- Stability: Experimental

## Overview

The filter processor drops logs, metric streams, traces, or individual Profiles
samples according to signal-specific include and exclude rules.

For reference, compare the Go Collector
[filter processor](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/processor/filterprocessor/README.md).

## Getting Started

Start with a signal-specific include or exclude rule:

```yaml
type: processor:filter
config:
  logs:
    include:
      match_type: strict
      severity_texts:
        - WARN
        - ERROR
```

## Configuration

The node-level config can define independent filter rules for metrics, logs,
traces, and Profiles:

```yaml
type: processor:filter
config:
  metrics:
    include:
      match_type: strict
      metric_names:
        - http.server.request.count
        - process.cpu.utilization
    exclude:
      match_type: regexp
      metric_names:
        - ^internal\..*$
  logs:
    include:
      match_type: strict
      resource_attributes:
        - key: deployment.environment
          value: prod
      record_attributes: []
      severity_texts:
        - WARN
        - ERROR
      severity_number:
        min: 13
        match_undefined: false
      bodies:
        - checkout started
        - failed to write to socket
    exclude:
      match_type: strict
      resource_attributes:
        - key: deployment.environment
          value: staging
      record_attributes:
        - key: component
          value: db
        - key: retryable
          value: true
      severity_texts:
        - WARN
      severity_number: null
      bodies:
        - checkout started
    log_record: []
  traces:
    include:
      match_type: strict
      resource_attributes:
        - key: deployment.environment
          value: prod
      span_attributes: []
      span_names:
        - checkout-warn
        - checkout-error
      event_names:
        - checkout-event
      event_attributes: []
      link_attributes: []
    exclude:
      match_type: strict
      resource_attributes:
        - key: deployment.environment
          value: staging
      span_attributes:
        - key: component
          value: db
      span_names:
        - payment-warn
        - payment-error
      event_names:
        - payment-event
      event_attributes:
        - key: success
          value: false
      link_attributes:
        - key: correlation
          value: false
  profiles:
    include:
      match_type: strict
      sample_attributes:
        - key: process.executable.name
          value: checkout
    exclude:
      match_type: regexp
      sample_attributes:
        - key: thread.name
          value: ^internal-.*
    # Shared dimensions are retained by default.
    compact: true
    # Dense IDs are rewritten only when compaction is enabled.
    dense_ids: true
    limits:
      max_output_rows: 1000000
      max_output_bytes: 268435456
      max_cloned_rows: 100000
```

For a runnable metric-name filter pipeline, see
[`configs/trafficgen-metric-filter-debug-noop.yaml`](../../../../../configs/trafficgen-metric-filter-debug-noop.yaml).

### Metrics

To filter metrics, define `metrics.include` or `metrics.exclude` with a
`match_type` and `metric_names`. Supported `match_type` values are `strict` and
`regexp`. When both `include` and `exclude` are defined, include filtering runs
first, and exclude filtering is applied to that result.

### Logs

To filter logs you can choose to define logs to `include` or `exclude`.
You can also choose to define both, if both are defined then the result
will be the intersection of the two. Currently we allow you to filter
based on `resource_attributes` (all the attributes must match),
`record_attributes` (only one in the list has to match), `severity_texts`,
`severity_number`, and `bodies`. When defining the `severity_number` you set
the min acceptable `severity_number` you can also choose whether to match
on undefined

### Traces

To filter traces, just like logs, you define the `include` or `exclude` fields.
You can filter based on `resource_attributes` (all the attributes must match,
for each of the remaining fields only one entry has to match),
`span_attributes`, `span_names`, `event_names`, `event_attributes` and
`link_attributes`.

### Profiles

Profiles filtering selects `SAMPLES` rows by sample-owned scalar attributes.
String values support `strict` and `regexp`; integer, double, and boolean values
use exact matching. Complex array and key/value-list criteria are rejected.

Filtering does not remove shared stacks or symbols implicitly. Set
`profiles.compact: true` to remove unreachable stacks, locations, mappings,
functions, links, and their owned attributes. `dense_ids: true` additionally
rewrites all retained Profiles IDs and foreign keys into deterministic dense
ranges.

## Telemetry

These tables list telemetry emitted directly by this node. Common engine
runtime metric sets may also be attached by the pipeline telemetry policy.

### Metric Sets

#### `processor.filter.pdata`

| Metric | Unit | Description |
| --- | --- | --- |
| `processor.filter.pdata.dropped.items` | `{item}` | Number of signal items (log records, spans, or metric data points) a decision node chose to drop. |
| `processor.filter.pdata.dropped.samples` | `{sample}` | Number of Profiles samples removed without dropping their owning profile. |

### Events

| Event | Severity | Description |
| --- | --- | --- |
| *None* | N/A | No node-specific events are emitted. |

## Limits

- Include and exclude semantics depend on the signal-specific filter type in
  `otel-arrow-dfe-pdata`.
- Profiles filtering supports sample-owned attributes only. Resource, scope,
  profile, mapping, and location predicates require owner splitting or
  copy-on-write semantics and remain unsupported.
- Profiles compaction is explicit because it can traverse and rewrite the full
  BAR-scoped graph.

## Related Docs

- [Configuration model](../../../../../docs/configuration-model.md)
- [Processor taxonomy](../../../../../docs/processors.md)
- [Core node catalog](../../../README.md)
