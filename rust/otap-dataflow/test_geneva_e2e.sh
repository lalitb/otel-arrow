#!/bin/bash
# End-to-end test script for Geneva OTAP log exporter using grpcurl
#
# This script sends OTLP logs to the collector, which will:
# 1. Receive logs via OTLP receiver (port 4317)
# 2. Convert to OTAP format
# 3. Encode with Bond + LZ4
# 4. Export to Geneva backend
#
# Requirements:
#   - grpcurl: brew install grpcurl (macOS) or https://github.com/fullstorydev/grpcurl
#   - Running collector on port 4317
#
# Usage:
#   # Terminal 1: Start collector
#   cd otel-arrow/rust/otap-dataflow
#   cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml
#   
#   # Terminal 2: Run this test
#   ./test_geneva_e2e.sh

set -e

echo "================================================================================"
echo "Geneva OTAP End-to-End Test"
echo "================================================================================"
echo

# Check if grpcurl is installed
if ! command -v grpcurl &> /dev/null; then
    echo "❌ Error: grpcurl is not installed"
    echo
    echo "Install it with:"
    echo "  macOS:   brew install grpcurl"
    echo "  Linux:   https://github.com/fullstorydev/grpcurl/releases"
    echo
    exit 1
fi

# Check if collector is running
echo "Checking if collector is running on localhost:4317..."
if ! nc -z localhost 4317 2>/dev/null; then
    echo "❌ Error: Collector is not running on port 4317"
    echo
    echo "Start it with:"
    echo "  cd otel-arrow/rust/otap-dataflow"
    echo "  cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml"
    echo
    exit 1
fi
echo "✓ Collector is running"
echo

# Current timestamp in nanoseconds
TIMESTAMP=$(date +%s%N)

echo "Sending test logs via OTLP/gRPC..."
echo

# Test 1: Simple log
echo "  [1/5] INFO  | Application started"
grpcurl -plaintext \
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
      "scope": {
        "name": "test-scope"
      },
      "log_records": [{
        "time_unix_nano": "$TIMESTAMP",
        "severity_number": 9,
        "severity_text": "INFO",
        "body": {"string_value": "Application started successfully"},
        "attributes": [{
          "key": "event.name",
          "value": {"string_value": "ApplicationLog"}
        }]
      }]
    }]
  }]
}
EOF

sleep 0.5

# Test 2: Warning log
echo "  [2/5] WARN  | High memory usage"
grpcurl -plaintext \
  -d @ \
  localhost:4317 \
  opentelemetry.proto.collector.logs.v1.LogsService/Export <<EOF
{
  "resource_logs": [{
    "scope_logs": [{
      "log_records": [{
        "time_unix_nano": "$TIMESTAMP",
        "severity_number": 13,
        "severity_text": "WARN",
        "body": {"string_value": "High memory usage detected"},
        "attributes": [{
          "key": "event.name",
          "value": {"string_value": "SystemHealth"}
        }, {
          "key": "memory_mb",
          "value": {"int_value": "1024"}
        }]
      }]
    }]
  }]
}
EOF

sleep 0.5

# Test 3: Error log with trace context
echo "  [3/5] ERROR | Database connection failed"
grpcurl -plaintext \
  -d @ \
  localhost:4317 \
  opentelemetry.proto.collector.logs.v1.LogsService/Export <<EOF
{
  "resource_logs": [{
    "scope_logs": [{
      "log_records": [{
        "time_unix_nano": "$TIMESTAMP",
        "severity_number": 17,
        "severity_text": "ERROR",
        "body": {"string_value": "Database connection failed"},
        "trace_id": "0102030405060708090a0b0c0d0e0f10",
        "span_id": "0102030405060708",
        "attributes": [{
          "key": "event.name",
          "value": {"string_value": "ApplicationError"}
        }, {
          "key": "db.host",
          "value": {"string_value": "localhost"}
        }, {
          "key": "retry_count",
          "value": {"int_value": "3"}
        }]
      }]
    }]
  }]
}
EOF

sleep 0.5

# Test 4: Performance metric
echo "  [4/5] INFO  | Request completed"
grpcurl -plaintext \
  -d @ \
  localhost:4317 \
  opentelemetry.proto.collector.logs.v1.LogsService/Export <<EOF
{
  "resource_logs": [{
    "scope_logs": [{
      "log_records": [{
        "time_unix_nano": "$TIMESTAMP",
        "severity_number": 9,
        "severity_text": "INFO",
        "body": {"string_value": "Request processing completed"},
        "attributes": [{
          "key": "event.name",
          "value": {"string_value": "PerformanceMetric"}
        }, {
          "key": "duration_ms",
          "value": {"int_value": "45"}
        }, {
          "key": "endpoint",
          "value": {"string_value": "/api/users"}
        }]
      }]
    }]
  }]
}
EOF

sleep 0.5

# Test 5: Security event
echo "  [5/5] INFO  | User authenticated"
grpcurl -plaintext \
  -d @ \
  localhost:4317 \
  opentelemetry.proto.collector.logs.v1.LogsService/Export <<EOF
{
  "resource_logs": [{
    "scope_logs": [{
      "log_records": [{
        "time_unix_nano": "$TIMESTAMP",
        "severity_number": 9,
        "severity_text": "INFO",
        "body": {"string_value": "User authentication successful"},
        "attributes": [{
          "key": "event.name",
          "value": {"string_value": "SecurityEvent"}
        }, {
          "key": "user_id",
          "value": {"string_value": "user123"}
        }, {
          "key": "auth_method",
          "value": {"string_value": "oauth2"}
        }]
      }]
    }]
  }]
}
EOF

echo
echo "✓ All 5 test logs sent successfully"
echo
echo "Check the collector output for:"
echo "  - 'Encoded OTAP log batch' debug messages"
echo "  - Bond encoding details (schemas, events, compression)"
echo "  - Upload attempts to Geneva backend"
echo
echo "Expected flow:"
echo "  ✓ Logs received by OTLP receiver (port 4317)"
echo "  ✓ Converted to OTAP Arrow format"
echo "  ✓ Grouped by event name (ApplicationLog, SystemHealth, etc.)"
echo "  ✓ Encoded with Bond binary format"
echo "  ✓ Compressed with LZ4 (~60% size reduction)"
echo "  ✓ Upload attempted to Geneva backend"
echo "    (Will fail without real Geneva endpoint - this is expected)"
echo
echo "================================================================================"
echo "Test completed!"
echo "================================================================================"
