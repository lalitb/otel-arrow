#!/usr/bin/env python3
"""
FIXED stress test - properly passes custom attributes through OTel SDK.

The previous version used extra={"otelAttributes": {...}} which doesn't work.
This version uses the OTel Logging API directly.

Requirements:
    pip install opentelemetry-api opentelemetry-sdk opentelemetry-exporter-otlp-proto-grpc

Usage:
    python3 stress_test_geneva_fixed.py --mode burst --count 10
"""

import argparse
import time
import sys
from opentelemetry import _logs
from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
from opentelemetry.exporter.otlp.proto.grpc._log_exporter import OTLPLogExporter
from opentelemetry.sdk.resources import Resource


def setup_otel(batch_size=512, max_queue_size=2048):
    """Configure OpenTelemetry properly."""
    resource = Resource.create({"service.name": "test-service"})

    logger_provider = LoggerProvider(resource=resource)
    _logs.set_logger_provider(logger_provider)

    otlp_exporter = OTLPLogExporter(
        endpoint="http://localhost:4317",
        insecure=True
    )

    logger_provider.add_log_record_processor(
        BatchLogRecordProcessor(
            otlp_exporter,
            max_export_batch_size=batch_size,
            max_queue_size=max_queue_size,
            schedule_delay_millis=1000,
        )
    )

    return logger_provider


def send_burst_logs(logger_provider, count):
    """Send logs using OTel Logging API directly (not Python logging module)."""
    print(f"📤 Sending burst of {count} logs...")
    start_time = time.time()

    # Get OTel logger (not Python logger!)
    otel_logger = logger_provider.get_logger("test-logger", "1.0.0")

    for i in range(count):
        # Attributes as dict - this is the correct way!
        attributes = {
            "event_name": "TestEvent",
            "user_id": f"user{i+1}",
            "status_code": 200,
            "status_code_new_test.withdot": 400,
            "test.key": "/Users/lalitb/work/obs/otel/rust-collector-2/otel-arrow/rust/otap-dataflow/./stress_test_geneva.py"
        }

        # Emit log record using OTel API
        otel_logger.emit(
            _logs.LogRecord(
                timestamp=time.time_ns(),
                body=f"Simple test message {i+1}",
                severity_number=_logs.SeverityNumber.INFO,
                severity_text="INFO",
                attributes=attributes
            )
        )

        if (i + 1) % 10 == 0:
            elapsed = time.time() - start_time
            rate = (i + 1) / elapsed
            print(f"  Progress: {i + 1}/{count} logs ({rate:.0f} logs/sec)")

    elapsed = time.time() - start_time
    rate = count / elapsed
    print(f"✅ Sent {count} logs in {elapsed:.2f}s ({rate:.0f} logs/sec)")
    return elapsed, rate


def send_sustained_logs(logger_provider, duration_seconds, rate_per_second=100):
    """Send logs at sustained rate using OTel API."""
    print(f"📊 Sending logs at {rate_per_second} logs/sec for {duration_seconds} seconds...")

    start_time = time.time()
    total_sent = 0
    interval = 1.0 / rate_per_second
    end_time = start_time + duration_seconds

    otel_logger = logger_provider.get_logger("test-logger", "1.0.0")

    while time.time() < end_time:
        iteration_start = time.time()

        attributes = {
            "event_name": "TestEvent",
            "user_id": f"user{total_sent+1}",
            "status_code": 200,
            "status_code_new_test.withdot": 400,
            "test.key": "/Users/lalitb/work/obs/otel/rust-collector-2/otel-arrow/rust/otap-dataflow/./stress_test_geneva.py"
        }

        otel_logger.emit(
            _logs.LogRecord(
                timestamp=time.time_ns(),
                body=f"Simple test message {total_sent+1}",
                severity_number=_logs.SeverityNumber.INFO,
                severity_text="INFO",
                attributes=attributes
            )
        )

        total_sent += 1

        elapsed = time.time() - start_time
        if int(elapsed) > int(elapsed - interval):
            print(f"  Progress: {elapsed:.0f}s - {total_sent} logs sent ({total_sent/elapsed:.0f} logs/sec)")

        sleep_time = interval - (time.time() - iteration_start)
        if sleep_time > 0:
            time.sleep(sleep_time)

    elapsed = time.time() - start_time
    actual_rate = total_sent / elapsed
    print(f"✅ Sent {total_sent} logs in {elapsed:.2f}s ({actual_rate:.0f} logs/sec)")
    return elapsed, actual_rate


def main():
    parser = argparse.ArgumentParser(
        description="FIXED Geneva stress test with proper OTel attribute passing",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s --mode burst --count 10
  %(prog)s --mode sustained --rate 50 --duration 30
  %(prog)s --mode all
        """
    )

    parser.add_argument("--mode", choices=["burst", "sustained", "all"], default="burst")
    parser.add_argument("--count", type=int, default=10)
    parser.add_argument("--rate", type=int, default=50)
    parser.add_argument("--duration", type=int, default=30)
    parser.add_argument("--batch-size", type=int, default=512)
    parser.add_argument("--queue-size", type=int, default=2048)

    args = parser.parse_args()

    print("=" * 80)
    print("FIXED Geneva Stress Test - Proper OTel Attribute Passing")
    print("=" * 80)
    print(f"\nMode: {args.mode}")
    print(f"Batch size: {args.batch_size}")
    print(f"Queue size: {args.queue_size}\n")

    try:
        logger_provider = setup_otel(args.batch_size, args.queue_size)
        results = []

        if args.mode in ["burst", "all"]:
            print("\n" + "=" * 80)
            print("TEST 1: Burst Mode")
            print("=" * 80)
            elapsed, rate = send_burst_logs(logger_provider, args.count)
            results.append(("Burst", args.count, elapsed, rate))
            print("\nFlushing buffer...")
            logger_provider.force_flush()
            time.sleep(2)

        if args.mode in ["sustained", "all"]:
            print("\n" + "=" * 80)
            print("TEST 2: Sustained Load")
            print("=" * 80)
            elapsed, rate = send_sustained_logs(logger_provider, args.duration, args.rate)
            results.append(("Sustained", args.duration * args.rate, elapsed, rate))
            print("\nFlushing buffer...")
            logger_provider.force_flush()
            time.sleep(2)

        print("\nFinal flush...")
        logger_provider.force_flush()
        time.sleep(3)

        print("\n" + "=" * 80)
        print("STRESS TEST SUMMARY")
        print("=" * 80)
        for test_name, count, elapsed, rate in results:
            print(f"{test_name:15s} | {str(count):10s} logs | {elapsed:6.2f}s | {rate:8.0f} logs/sec")

        print("\n" + "=" * 80)
        print("✅ All tests completed!")
        print("=" * 80)
        print("\nCheck collector output - should now see ALL custom attributes:")
        print("  - event_name")
        print("  - user_id")
        print("  - status_code")
        print("  - status_code_new_test.withdot")
        print("  - test.key")
        print("\nLogs should now render in Geneva UI! 🎉\n")

    except KeyboardInterrupt:
        print("\n\n⚠️  Test interrupted by user")
        logger_provider.force_flush()
        sys.exit(1)
    except Exception as e:
        print(f"\n❌ Error: {e}", file=sys.stderr)
        import traceback
        traceback.print_exc()
        sys.exit(1)


if __name__ == "__main__":
    main()
