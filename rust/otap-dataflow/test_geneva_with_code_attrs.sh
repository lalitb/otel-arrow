#!/bin/bash
# Test based on test_geneva_simple.sh but WITH code.* attributes
# This isolates whether code.file.path, code.function.name, code.line.number cause UI rendering issues

set -e

echo "================================================================================"
echo "Geneva Test - WITH code.* Attributes (like Python SDK adds)"
echo "================================================================================"
echo

# Check if grpcurl is installed
if ! command -v grpcurl &> /dev/null; then
    echo "❌ Error: grpcurl is not installed"
    echo "Install it with: brew install grpcurl"
    exit 1
fi

# Check if collector is running
if ! nc -z localhost 4317 2>/dev/null; then
    echo "❌ Error: Collector is not running on port 4317"
    exit 1
fi
echo "✓ Collector is running"
echo

TIMESTAMP=$(date +%s%N)

echo "Sending 10 test logs WITH code.* attributes..."
echo

# Path to proto files
PROTO_PATH="/Users/lalitb/work/obs/otel/rust-collector-2/otel-arrow/proto/opentelemetry-proto"

for i in {1..10}; do
  echo "  Sending log $i/10..."
  grpcurl -plaintext \
    -proto "$PROTO_PATH/opentelemetry/proto/collector/logs/v1/logs_service.proto" \
    -import-path "$PROTO_PATH" \
    -d @ \
    localhost:4317 \
    opentelemetry.proto.collector.logs.v1.LogsService/Export <<EOF
{
  "resource_logs": [{
    "resource": {
      "attributes": [{
        "key": "service.name",
        "value": {"string_value": "test-service"}
      }]
    },
    "scope_logs": [{
      "scope": {"name": "test-scope"},
      "log_records": [{
        "time_unix_nano": "$TIMESTAMP",
        "severity_number": 9,
        "severity_text": "INFO",
        "body": {"string_value": "Simple test message $i"},
        "attributes": [
          {"key": "event_name", "value": {"string_value": "TestEvent"}},
          {"key": "user_id", "value": {"string_value": "user$i"}},
          {"key": "status_code", "value": {"int_value": "200"}},
          {"key": "status_code_new_test.withdot", "value": {"int_value": "400"}},
          {"key": "test.key", "value": {"string_value": "/Users/lalitb/work/obs/otel/rust-collector-2/otel-arrow/rust/otap-dataflow/./stress_test_geneva.py"}},
          {"key": "code.file.path", "value": {"string_value": "/Users/lalitb/work/obs/otel/rust-collector-2/otel-arrow/rust/otap-dataflow/./stress_test_geneva_simple.py"}},
          {"key": "code.function.name", "value": {"string_value": "send_burst_logs"}},
          {"key": "code.line.number", "value": {"int_value": "83"}}
        ]
      }]
    }]
  }]
}
EOF
done

echo
echo "✓ All 10 test logs sent WITH code.* attributes"
echo
echo "Check collector output for:"
echo "  - [LOG_ATTR] code.file.path"
echo "  - [LOG_ATTR] code.function.name"
echo "  - [LOG_ATTR] code.line.number"
echo "  - Schema ID (compare with test_geneva_simple.sh)"
echo
echo "Then check Geneva UI:"
echo "  - Do these logs render correctly?"
echo "  - If NO: code.* attributes are the problem"
echo "  - If YES: something else in Python SDK is the issue"
echo
echo "================================================================================"
