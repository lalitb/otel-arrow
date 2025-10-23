#!/bin/bash
# Simple gRPC test with minimal attributes (no dots in attribute names)

set -e

echo "================================================================================"
echo "Simple Geneva Test - No Dots in Attribute Names"
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

echo "Sending 10 simple test logs (to trigger batch flush)..."
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
        "key": "service_name",
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
	  {"key": "test.key", "value" : {"string_value": "/Users/lalitb/work/obs/otel/rust-collector-2/otel-arrow/rust/otap-dataflow/./stress_test_geneva.py"}}
        ]
      }]
    }]
  }]
}
EOF
done

echo
echo "✓ All 10 test logs sent (should trigger batch flush)"
echo
echo "Check collector output for:"
echo "  - No [RESOURCE_ATTR] in log encoding (our fix!)"
echo "  - [LOG_ATTR] event_name, user_id, status_code"
echo "  - Geneva upload URL with schemaId"
echo
echo "================================================================================"
