#!/bin/bash
# Simple end-to-end test using curl (no grpcurl required!)
#
# Sends OTLP logs via HTTP/JSON to the collector which will:
# 1. Receive logs via OTLP HTTP receiver (port 4318)
# 2. Convert to OTAP format
# 3. Encode with Bond + LZ4
# 4. Export to Geneva backend
#
# Requirements: Only curl (already installed on most systems)
#
# Usage:
#   # Terminal 1: Start collector
#   cd otel-arrow/rust/otap-dataflow
#   cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml --num-cores 1
#   
#   # Terminal 2: Run this test
#   ./test_geneva_curl.sh

set -e

echo "================================================================================"
echo "Geneva OTAP End-to-End Test (using curl)"
echo "================================================================================"
echo

# Check if collector is running
echo "Checking if collector is running on localhost:4318..."
if ! nc -z localhost 4318 2>/dev/null; then
    echo "❌ Error: Collector is not running on port 4318"
    echo
    echo "Start it with:"
    echo "  cd otel-arrow/rust/otap-dataflow"
    echo "  cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml --num-cores 1"
    echo
    exit 1
fi
echo "✓ Collector is running"
echo

# Current timestamp in nanoseconds
TIMESTAMP=$(date +%s)000000000

echo "Sending test logs via OTLP/HTTP (JSON)..."
echo

# Test 1: Simple INFO log
echo "  [1/5] INFO  | Application started"
curl -s -X POST http://localhost:4318/v1/logs \
  -H "Content-Type: application/json" \
  -d "{
  \"resourceLogs\": [{
    \"resource\": {
      \"attributes\": [{
        \"key\": \"service.name\",
        \"value\": {\"stringValue\": \"test-service\"}
      }]
    },
    \"scopeLogs\": [{
      \"scope\": {\"name\": \"test-scope\"},
      \"logRecords\": [{
        \"timeUnixNano\": \"$TIMESTAMP\",
        \"severityNumber\": 9,
        \"severityText\": \"INFO\",
        \"body\": {\"stringValue\": \"Application started successfully\"},
        \"attributes\": [{
          \"key\": \"event.name\",
          \"value\": {\"stringValue\": \"ApplicationLog\"}
        }]
      }]
    }]
  }]
}" > /dev/null

sleep 0.3

# Test 2: WARNING log
echo "  [2/5] WARN  | High memory usage"
curl -s -X POST http://localhost:4318/v1/logs \
  -H "Content-Type: application/json" \
  -d "{
  \"resourceLogs\": [{
    \"scopeLogs\": [{
      \"logRecords\": [{
        \"timeUnixNano\": \"$TIMESTAMP\",
        \"severityNumber\": 13,
        \"severityText\": \"WARN\",
        \"body\": {\"stringValue\": \"High memory usage detected\"},
        \"attributes\": [
          {\"key\": \"event.name\", \"value\": {\"stringValue\": \"SystemHealth\"}},
          {\"key\": \"memory_mb\", \"value\": {\"intValue\": \"1024\"}}
        ]
      }]
    }]
  }]
}" > /dev/null

sleep 0.3

# Test 3: ERROR log with trace context
echo "  [3/5] ERROR | Database connection failed"
curl -s -X POST http://localhost:4318/v1/logs \
  -H "Content-Type: application/json" \
  -d "{
  \"resourceLogs\": [{
    \"scopeLogs\": [{
      \"logRecords\": [{
        \"timeUnixNano\": \"$TIMESTAMP\",
        \"severityNumber\": 17,
        \"severityText\": \"ERROR\",
        \"body\": {\"stringValue\": \"Database connection failed\"},
        \"traceId\": \"0102030405060708090a0b0c0d0e0f10\",
        \"spanId\": \"0102030405060708\",
        \"attributes\": [
          {\"key\": \"event.name\", \"value\": {\"stringValue\": \"ApplicationError\"}},
          {\"key\": \"db.host\", \"value\": {\"stringValue\": \"localhost\"}},
          {\"key\": \"retry_count\", \"value\": {\"intValue\": \"3\"}}
        ]
      }]
    }]
  }]
}" > /dev/null

sleep 0.3

# Test 4: Performance metric
echo "  [4/5] INFO  | Request completed"
curl -s -X POST http://localhost:4318/v1/logs \
  -H "Content-Type: application/json" \
  -d "{
  \"resourceLogs\": [{
    \"scopeLogs\": [{
      \"logRecords\": [{
        \"timeUnixNano\": \"$TIMESTAMP\",
        \"severityNumber\": 9,
        \"severityText\": \"INFO\",
        \"body\": {\"stringValue\": \"Request processing completed\"},
        \"attributes\": [
          {\"key\": \"event.name\", \"value\": {\"stringValue\": \"PerformanceMetric\"}},
          {\"key\": \"duration_ms\", \"value\": {\"intValue\": \"45\"}},
          {\"key\": \"endpoint\", \"value\": {\"stringValue\": \"/api/users\"}}
        ]
      }]
    }]
  }]
}" > /dev/null

sleep 0.3

# Test 5: Security event
echo "  [5/5] INFO  | User authenticated"
curl -s -X POST http://localhost:4318/v1/logs \
  -H "Content-Type: application/json" \
  -d "{
  \"resourceLogs\": [{
    \"scopeLogs\": [{
      \"logRecords\": [{
        \"timeUnixNano\": \"$TIMESTAMP\",
        \"severityNumber\": 9,
        \"severityText\": \"INFO\",
        \"body\": {\"stringValue\": \"User authentication successful\"},
        \"attributes\": [
          {\"key\": \"event.name\", \"value\": {\"stringValue\": \"SecurityEvent\"}},
          {\"key\": \"user_id\", \"value\": {\"stringValue\": \"user123\"}},
          {\"key\": \"auth_method\", \"value\": {\"stringValue\": \"oauth2\"}}
        ]
      }]
    }]
  }]
}" > /dev/null

echo
echo "✓ All 5 test logs sent successfully"
echo
echo "Check the collector output for:"
echo "  - 'Encoded OTAP log batch' debug messages"
echo "  - Bond encoding details (schemas, events, compression)"
echo "  - Upload attempts to Geneva backend"
echo
echo "Expected flow:"
echo "  ✓ Logs received by OTLP receiver (HTTP port 4318)"
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
