#!/usr/bin/env python3
"""
Simplified stress test for Geneva OTAP log exporter - uses same attributes as working test.

This version uses the EXACT same attributes as test_geneva_simple.sh to ensure compatibility:
- event_name (string)
- user_id (string)
- status_code (int)
- status_code_new_test.withdot (int)
- test.key (string)

Requirements:
    pip install opentelemetry-api opentelemetry-sdk opentelemetry-exporter-otlp-proto-grpc

Usage:
    # Terminal 1: Start collector
    cd otel-arrow/rust/otap-dataflow
    cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml --num-cores 1

    # Terminal 2: Run stress test
    python3 stress_test_geneva_simple.py --mode burst --count 100
    python3 stress_test_geneva_simple.py --mode sustained --duration 30
"""

import argparse
import time
import sys
from opentelemetry._logs import set_logger_provider
from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
from opentelemetry.exporter.otlp.proto.grpc._log_exporter import OTLPLogExporter
import logging


def setup_otel(batch_size=512, max_queue_size=2048):
    """Configure OpenTelemetry to send to local collector."""
    logger_provider = LoggerProvider()
    set_logger_provider(logger_provider)

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

    handler = LoggingHandler(
        level=logging.NOTSET,
        logger_provider=logger_provider
    )

    logging.getLogger().addHandler(handler)
    logging.getLogger().setLevel(logging.INFO)

    return logger_provider


def send_burst_logs(logger, count):
    """Send a burst of logs with attributes matching test_geneva_simple.sh"""
    print(f"📤 Sending burst of {count} logs...")
    start_time = time.time()

    for i in range(count):
        # Use same attributes as working test
        attributes = {
            "event_name": "TestEvent",
            "user_id": f"user{i+1}",
            "status_code": 200,
            "status_code_new_test.withdot": 400,
            "test.key": "/Users/lalitb/work/obs/otel/rust-collector-2/otel-arrow/rust/otap-dataflow/./stress_test_geneva.py"
        }

        extra = {"otelAttributes": attributes}

        # All INFO level to match working test
        logger.info(f"Simple test message {i+1}", extra=extra)

        # Progress indicator every 10 logs
        if (i + 1) % 10 == 0:
            elapsed = time.time() - start_time
            rate = (i + 1) / elapsed
            print(f"  Progress: {i + 1}/{count} logs ({rate:.0f} logs/sec)")

    elapsed = time.time() - start_time
    rate = count / elapsed
    print(f"✅ Sent {count} logs in {elapsed:.2f}s ({rate:.0f} logs/sec)")

    return elapsed, rate


def send_sustained_logs(logger, duration_seconds, rate_per_second=100):
    """Send logs at a sustained rate with working test attributes."""
    print(f"📊 Sending logs at {rate_per_second} logs/sec for {duration_seconds} seconds...")

    start_time = time.time()
    total_sent = 0
    interval = 1.0 / rate_per_second

    end_time = start_time + duration_seconds

    while time.time() < end_time:
        iteration_start = time.time()

        # Use same attributes as working test
        attributes = {
            "event_name": "TestEvent",
            "user_id": f"user{total_sent+1}",
            "status_code": 200,
            "status_code_new_test.withdot": 400,
            "test.key": "/Users/lalitb/work/obs/otel/rust-collector-2/otel-arrow/rust/otap-dataflow/./stress_test_geneva.py"
        }

        extra = {"otelAttributes": attributes}
        logger.info(f"Simple test message {total_sent+1}", extra=extra)

        total_sent += 1

        # Progress indicator every second
        elapsed = time.time() - start_time
        if int(elapsed) > int(elapsed - interval):
            print(f"  Progress: {elapsed:.0f}s - {total_sent} logs sent ({total_sent/elapsed:.0f} logs/sec)")

        # Sleep to maintain rate
        sleep_time = interval - (time.time() - iteration_start)
        if sleep_time > 0:
            time.sleep(sleep_time)

    elapsed = time.time() - start_time
    actual_rate = total_sent / elapsed
    print(f"✅ Sent {total_sent} logs in {elapsed:.2f}s ({actual_rate:.0f} logs/sec)")

    return elapsed, actual_rate


def main():
    parser = argparse.ArgumentParser(
        description="Simplified Geneva stress test using working test attributes",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Send 100 logs as fast as possible
  %(prog)s --mode burst --count 100

  # Send logs at 50/sec for 30 seconds
  %(prog)s --mode sustained --rate 50 --duration 30

  # Run both tests
  %(prog)s --mode all
        """
    )

    parser.add_argument(
        "--mode",
        choices=["burst", "sustained", "all"],
        default="burst",
        help="Test mode to run"
    )
    parser.add_argument("--count", type=int, default=100, help="Number of logs for burst mode")
    parser.add_argument("--rate", type=int, default=50, help="Logs per second for sustained mode")
    parser.add_argument("--duration", type=int, default=30, help="Duration in seconds for sustained mode")
    parser.add_argument("--batch-size", type=int, default=512, help="OTLP batch size")
    parser.add_argument("--queue-size", type=int, default=2048, help="OTLP max queue size")

    args = parser.parse_args()

    print("=" * 80)
    print("Geneva OTAP Simplified Stress Test")
    print("Using attributes from test_geneva_simple.sh")
    print("=" * 80)
    print()
    print(f"Mode: {args.mode}")
    print(f"Batch size: {args.batch_size}")
    print(f"Queue size: {args.queue_size}")
    print()

    try:
        logger_provider = setup_otel(args.batch_size, args.queue_size)
        logger = logging.getLogger(__name__)

        results = []

        if args.mode == "burst" or args.mode == "all":
            print("\n" + "=" * 80)
            print("TEST 1: Burst Mode")
            print("=" * 80)
            elapsed, rate = send_burst_logs(logger, args.count)
            results.append(("Burst", args.count, elapsed, rate))

            print("\nFlushing buffer...")
            logger_provider.force_flush()
            time.sleep(2)

        if args.mode == "sustained" or args.mode == "all":
            print("\n" + "=" * 80)
            print("TEST 2: Sustained Load")
            print("=" * 80)
            elapsed, rate = send_sustained_logs(logger, args.duration, args.rate)
            results.append(("Sustained", args.duration * args.rate, elapsed, rate))

            print("\nFlushing buffer...")
            logger_provider.force_flush()
            time.sleep(2)

        # Final flush
        print("\nFinal flush...")
        logger_provider.force_flush()
        time.sleep(3)

        # Print summary
        print("\n" + "=" * 80)
        print("STRESS TEST SUMMARY")
        print("=" * 80)
        for test_name, count, elapsed, rate in results:
            print(f"{test_name:15s} | {str(count):10s} logs | {elapsed:6.2f}s | {rate:8.0f} logs/sec")

        print("\n" + "=" * 80)
        print("✅ All tests completed successfully!")
        print("=" * 80)
        print()
        print("Check the collector output for:")
        print("  - Batch processing efficiency")
        print("  - Geneva upload success rates")
        print("  - Logs should render in Geneva UI (same attrs as working test)")
        print()

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
