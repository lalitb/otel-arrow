#!/usr/bin/env bash
# Provision Perses with the OTAP Dataflow project, datasource, and dashboards.
# Run after `docker compose up -d`.

set -euo pipefail

PERSES_URL="${PERSES_URL:-http://localhost:8081}"
DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Waiting for Perses at ${PERSES_URL}..."
until curl -sf "${PERSES_URL}/api/v1/health" >/dev/null 2>&1; do
  sleep 1
done
echo "Perses is ready."

echo "Creating project..."
curl -sf -X POST "${PERSES_URL}/api/v1/projects" \
  -H 'Content-Type: application/json' \
  -d @"${DIR}/perses/project.json" >/dev/null 2>&1 || true

echo "Creating datasource..."
curl -sf -X POST "${PERSES_URL}/api/v1/projects/otap-dataflow/datasources" \
  -H 'Content-Type: application/json' \
  -d @"${DIR}/perses/datasource.json" >/dev/null 2>&1 || \
curl -sf -X PUT "${PERSES_URL}/api/v1/projects/otap-dataflow/datasources/prometheus" \
  -H 'Content-Type: application/json' \
  -d @"${DIR}/perses/datasource.json" >/dev/null

echo "Importing dashboards..."
for f in "${DIR}"/perses/*-dashboard.json; do
  name=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['metadata']['name'])" "$f")
  curl -sf -X POST "${PERSES_URL}/api/v1/projects/otap-dataflow/dashboards" \
    -H 'Content-Type: application/json' \
    -d @"$f" >/dev/null 2>&1 || \
  curl -sf -X PUT "${PERSES_URL}/api/v1/projects/otap-dataflow/dashboards/${name}" \
    -H 'Content-Type: application/json' \
    -d @"$f" >/dev/null
  echo "  ✓ ${name}"
done

echo ""
echo "Done! Open ${PERSES_URL} → project 'otap-dataflow'"
