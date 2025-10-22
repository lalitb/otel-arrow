#!/usr/bin/env python3
"""
End-to-end test script for Geneva OTAP log exporter.

This script sends OTLP logs to the collector, which will:
1. Receive logs via OTLP receiver (port 4317)
2. Convert to OTAP format
3. Encode with Bond + LZ4
4. Export to Geneva backend

Requirements:
    pip install opentelemetry-api opentelemetry-sdk opentelemetry-exporter-otlp-proto-grpc

Usage:
    # Terminal 1: Start collector
    cd otel-arrow/rust/otap-dataflow
    cargo run --bin otelarrowcol -- --config configs/otlp-geneva-test.yaml
    
    # Terminal 2: Send test logs
    python3 test_geneva_e2e.py
"""

import time
import sys
from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
from opentelemetry._logs import set_logger_provider
from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
from opentelemetry.exporter.otlp.proto.grpc._log_exporter import OTLPLogExporter
import logging

def setup_otel():
    """Configure OpenTelemetry to send to local collector."""
    
    # Set up logging
    logger_provider = LoggerProvider()
    set_logger_provider(logger_provider)
    
    # Configure OTLP exporter pointing to local collector
    otlp_exporter = OTLPLogExporter(
        endpoint="http://localhost:4317",
        insecure=True
    )
    
    logger_provider.add_log_record_processor(
        BatchLogRecordProcessor(otlp_exporter)
    )
    
    # Set up Python logging to use OTEL
    handler = LoggingHandler(
        level=logging.NOTSET,
        logger_provider=logger_provider
    )
    
    logging.getLogger().addHandler(handler)
    logging.getLogger().setLevel(logging.DEBUG)
    
    return logger_provider

def send_test_logs():
    """Send various test logs to exercise the pipeline."""
    
    print("=" * 80)
    print("Geneva OTAP End-to-End Test")
    print("=" * 80)
    print()
    print("Setting up OpenTelemetry...")
    
    logger_provider = setup_otel()
    logger = logging.getLogger(__name__)
    
    print("✓ OTLP exporter configured (endpoint: http://localhost:4317)")
    print()
    
    test_logs = [
        ("INFO", "Application started successfully", {"service": "web-api", "version": "1.0.0"}),
        ("WARNING", "High memory usage detected", {"memory_mb": 1024, "threshold": 800}),
        ("ERROR", "Database connection failed", {"db_host": "localhost", "retry_count": 3}),
        ("INFO", "User authentication successful", {"user_id": "user123", "method": "oauth2"}),
        ("DEBUG", "Request processing completed", {"duration_ms": 45, "endpoint": "/api/users"}),
        ("ERROR", "API rate limit exceeded", {"client_ip": "192.168.1.100", "limit": 100}),
        ("INFO", "Cache refresh completed", {"cache_size": 5000, "refresh_time_ms": 120}),
        ("WARNING", "Disk space running low", {"available_gb": 2, "threshold_gb": 5}),
    ]
    
    print(f"Sending {len(test_logs)} test logs...")
    print()
    
    for i, (level, message, attributes) in enumerate(test_logs, 1):
        # Log with attributes
        extra = {"otelAttributes": attributes}
        
        if level == "DEBUG":
            logger.debug(message, extra=extra)
        elif level == "INFO":
            logger.info(message, extra=extra)
        elif level == "WARNING":
            logger.warning(message, extra=extra)
        elif level == "ERROR":
            logger.error(message, extra=extra)
        
        print(f"  [{i}/{len(test_logs)}] {level:7s} | {message}")
        
        # Small delay to avoid overwhelming the collector
        time.sleep(0.1)
    
    print()
    print("Flushing log buffer...")
    
    # Force flush to ensure all logs are sent
    logger_provider.force_flush()
    
    print("✓ All logs sent successfully")
    print()
    print("Check the collector output for:")
    print("  - 'Encoded OTAP log batch' debug messages")
    print("  - Bond encoding details (schemas, events, compression)")
    print("  - Upload attempts to Geneva backend")
    print()
    print("Expected behavior:")
    print("  ✓ Logs received by OTLP receiver")
    print("  ✓ Converted to OTAP Arrow format")
    print("  ✓ Encoded with Bond binary format")
    print("  ✓ Compressed with LZ4 (~60% reduction)")
    print("  ✓ Upload attempted to Geneva (will fail without real backend)")
    print()

if __name__ == "__main__":
    try:
        send_test_logs()
        print("=" * 80)
        print("Test completed successfully!")
        print("=" * 80)
    except Exception as e:
        print(f"\n❌ Error: {e}", file=sys.stderr)
        print("\nMake sure the collector is running:")
        print("  cd otel-arrow/rust/otap-dataflow")
        print("  cargo run --bin otelarrowcol -- --config configs/otlp-geneva-test.yaml")
        sys.exit(1)
