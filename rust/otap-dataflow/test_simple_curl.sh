#!/bin/bash
# Simple test with minimal attributes (no dots) for debugging

set -e

echo "================================================================================"
echo "Simple Geneva Test - Minimal Attributes"
echo "================================================================================"
echo

# Check if collector is running
if ! nc -z localhost 4318 2>/dev/null; then
    echo "❌ Error: Collector is not running on port 4318"
    exit 1
fi
echo "✓ Collector is running"
echo

TIMESTAMP=$(date +%s)000000000

echo "Sending test log with simple attributes (no dots)..."
echo

curl -X POST http://localhost:4318/v1/logs \
  -H "Content-Type: application/json" \
  -d "{
  \"resourceLogs\": [{
    \"resource\": {
      \"attributes\": [{
        \"key\": \"service_name\",
        \"value\": {\"stringValue\": \"test-service\"}
      }]
    },
    \"scopeLogs\": [{
      \"scope\": {\"name\": \"test-scope\"},
      \"logRecords\": [{
        \"timeUnixNano\": \"$TIMESTAMP\",
        \"severityNumber\": 9,
        \"severityText\": \"INFO\",
        \"body\": {\"stringValue\": \"Test message with simple attributes\"},
        \"attributes\": [
          {\"key\": \"event_name\", \"value\": {\"stringValue\": \"TestEvent\"}},
          {\"key\": \"user_id\", \"value\": {\"stringValue\": \"user123\"}},
          {\"key\": \"request_id\", \"value\": {\"stringValue\": \"req456\"}}
        ]
      }]
    }]
  }]
}"

echo
echo "✓ Test log sent"
echo
echo "Check collector output for:"
echo "  - No [RESOURCE_ATTR] entries (only [LOG_ATTR])"
echo "  - Attributes: event_name, user_id, request_id"
echo "  - Geneva upload URL with schemaId"
echo
echo "================================================================================"
