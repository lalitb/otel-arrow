#!/usr/bin/env python3
"""
Stress test script for Geneva OTAP log exporter pipeline.

This script sends bursts of logs with various patterns to test:
- High throughput scenarios
- Batch processing efficiency
- Memory usage under load
- Concurrent log generation
- Different severity levels and attributes

Requirements:
    pip install opentelemetry-api opentelemetry-sdk opentelemetry-exporter-otlp-proto-grpc

Usage:
    # Terminal 1: Start collector
    cd otel-arrow/rust/otap-dataflow
    cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml --num-cores 1

    # Terminal 2: Run stress test
    python3 stress_test_geneva.py --mode burst --count 1000
    python3 stress_test_geneva.py --mode sustained --duration 60
    python3 stress_test_geneva.py --mode spike --count 5000
"""

import argparse
import time
import sys
import random
import threading
from datetime import datetime
from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
from opentelemetry._logs import set_logger_provider
from opentelemetry.sdk._logs import LoggerProvider, LoggingHandler
from opentelemetry.sdk._logs.export import BatchLogRecordProcessor
from opentelemetry.exporter.otlp.proto.grpc._log_exporter import OTLPLogExporter
import logging

# Test data templates
LOG_MESSAGES = [
    "Application started successfully",
    "User authentication completed",
    "Database query executed",
    "Cache hit for key",
    "API request received",
    "Payment processing initiated",
    "Email notification sent",
    "File upload completed",
    "Session created",
    "Configuration updated",
    "Health check passed",
    "Metrics published",
    "Backup completed",
    "Job scheduled",
    "Task completed",
    "Connection established",
    "Request processed",
    "Data synchronized",
    "Validation passed",
    "Transaction committed",
]

ERROR_MESSAGES = [
    "Database connection timeout",
    "Authentication failed",
    "API rate limit exceeded",
    "File not found",
    "Invalid input data",
    "Network error occurred",
    "Service unavailable",
    "Permission denied",
    "Resource exhausted",
    "Timeout waiting for response",
]

WARNING_MESSAGES = [
    "High memory usage detected",
    "Slow query detected",
    "Retry attempt scheduled",
    "Deprecated API endpoint used",
    "Cache miss for key",
    "Disk space running low",
    "Connection pool near limit",
    "Response time degraded",
]

SERVICES = ["web-api", "auth-service", "payment-service", "notification-service",
            "data-processor", "cache-service", "background-worker", "api-gateway"]

ENDPOINTS = ["/api/users", "/api/orders", "/api/products", "/api/payments",
             "/api/auth", "/api/notifications", "/api/analytics", "/api/reports"]


def setup_otel(batch_size=512, max_queue_size=2048):
    """Configure OpenTelemetry to send to local collector."""

    # Set up logging with configurable batch sizes for stress testing
    logger_provider = LoggerProvider()
    set_logger_provider(logger_provider)

    # Configure OTLP exporter pointing to local collector
    otlp_exporter = OTLPLogExporter(
        endpoint="http://localhost:4317",
        insecure=True
    )

    # Use batch processor with custom settings for stress testing
    logger_provider.add_log_record_processor(
        BatchLogRecordProcessor(
            otlp_exporter,
            max_export_batch_size=batch_size,
            max_queue_size=max_queue_size,
            schedule_delay_millis=1000,  # 1 second
        )
    )

    # Set up Python logging to use OTEL
    handler = LoggingHandler(
        level=logging.NOTSET,
        logger_provider=logger_provider
    )

    logging.getLogger().addHandler(handler)
    logging.getLogger().setLevel(logging.DEBUG)

    return logger_provider


def generate_log_attributes():
    """Generate random but realistic log attributes."""
    return {
        "service": random.choice(SERVICES),
        "endpoint": random.choice(ENDPOINTS),
        "user_id": f"user{random.randint(1000, 9999)}",
        "request_id": f"req-{random.randint(100000, 999999)}",
        "duration_ms": random.randint(10, 5000),
        "status_code": random.choice([200, 201, 400, 401, 403, 404, 500, 503]),
        "environment": random.choice(["production", "staging", "development"]),
        "region": random.choice(["us-east", "us-west", "eu-west", "ap-south"]),
        "version": f"v{random.randint(1, 5)}.{random.randint(0, 10)}.{random.randint(0, 20)}",
    }


def send_burst_logs(logger, count, burst_delay=0.001):
    """Send a burst of logs as fast as possible."""
    print(f"📤 Sending burst of {count} logs...")
    start_time = time.time()

    for i in range(count):
        level = random.choice(["INFO", "WARNING", "ERROR", "DEBUG"])

        if level == "ERROR":
            message = random.choice(ERROR_MESSAGES)
        elif level == "WARNING":
            message = random.choice(WARNING_MESSAGES)
        else:
            message = random.choice(LOG_MESSAGES)

        attributes = generate_log_attributes()
        attributes["burst_index"] = i

        extra = {"otelAttributes": attributes}

        if level == "DEBUG":
            logger.debug(message, extra=extra)
        elif level == "INFO":
            logger.info(message, extra=extra)
        elif level == "WARNING":
            logger.warning(message, extra=extra)
        elif level == "ERROR":
            logger.error(message, extra=extra)

        # Small delay to avoid overwhelming the client
        if burst_delay > 0:
            time.sleep(burst_delay)

        # Progress indicator every 100 logs
        if (i + 1) % 100 == 0:
            elapsed = time.time() - start_time
            rate = (i + 1) / elapsed
            print(f"  Progress: {i + 1}/{count} logs ({rate:.0f} logs/sec)")

    elapsed = time.time() - start_time
    rate = count / elapsed
    print(f"✅ Sent {count} logs in {elapsed:.2f}s ({rate:.0f} logs/sec)")

    return elapsed, rate


def send_sustained_logs(logger, duration_seconds, rate_per_second=100):
    """Send logs at a sustained rate for a specified duration."""
    print(f"📊 Sending logs at {rate_per_second} logs/sec for {duration_seconds} seconds...")

    start_time = time.time()
    total_sent = 0
    interval = 1.0 / rate_per_second

    end_time = start_time + duration_seconds

    while time.time() < end_time:
        iteration_start = time.time()

        level = random.choice(["INFO", "WARNING", "ERROR", "DEBUG"])

        if level == "ERROR":
            message = random.choice(ERROR_MESSAGES)
        elif level == "WARNING":
            message = random.choice(WARNING_MESSAGES)
        else:
            message = random.choice(LOG_MESSAGES)

        attributes = generate_log_attributes()
        attributes["test_mode"] = "sustained"

        extra = {"otelAttributes": attributes}

        if level == "DEBUG":
            logger.debug(message, extra=extra)
        elif level == "INFO":
            logger.info(message, extra=extra)
        elif level == "WARNING":
            logger.warning(message, extra=extra)
        elif level == "ERROR":
            logger.error(message, extra=extra)

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


def send_spike_logs(logger, spike_count, baseline_rate=10, spike_duration=5):
    """Send logs with periodic spikes to test batching under variable load."""
    print(f"📈 Sending logs with periodic spikes...")
    print(f"   Baseline: {baseline_rate} logs/sec, Spikes: {spike_count} logs over {spike_duration}s")

    start_time = time.time()
    total_sent = 0

    # Send baseline for 10 seconds
    print("\n🔵 Baseline phase (10s)...")
    for _ in range(10 * baseline_rate):
        level = random.choice(["INFO", "DEBUG"])
        message = random.choice(LOG_MESSAGES)
        attributes = generate_log_attributes()
        attributes["phase"] = "baseline"

        extra = {"otelAttributes": attributes}
        logger.info(message, extra=extra)
        total_sent += 1
        time.sleep(1.0 / baseline_rate)

    # Send spike
    print(f"\n🔴 Spike phase ({spike_duration}s - {spike_count} logs)...")
    spike_start = time.time()
    for i in range(spike_count):
        level = random.choice(["WARNING", "ERROR", "INFO"])
        if level == "ERROR":
            message = random.choice(ERROR_MESSAGES)
        elif level == "WARNING":
            message = random.choice(WARNING_MESSAGES)
        else:
            message = random.choice(LOG_MESSAGES)

        attributes = generate_log_attributes()
        attributes["phase"] = "spike"
        attributes["spike_index"] = i

        extra = {"otelAttributes": attributes}

        if level == "ERROR":
            logger.error(message, extra=extra)
        elif level == "WARNING":
            logger.warning(message, extra=extra)
        else:
            logger.info(message, extra=extra)

        total_sent += 1

        # Progress during spike
        if (i + 1) % 100 == 0:
            elapsed = time.time() - spike_start
            rate = (i + 1) / elapsed
            print(f"  Spike progress: {i + 1}/{spike_count} ({rate:.0f} logs/sec)")

    spike_elapsed = time.time() - spike_start
    spike_rate = spike_count / spike_elapsed
    print(f"  Spike completed: {spike_count} logs in {spike_elapsed:.2f}s ({spike_rate:.0f} logs/sec)")

    # Return to baseline for 10 seconds
    print("\n🔵 Return to baseline (10s)...")
    for _ in range(10 * baseline_rate):
        level = "INFO"
        message = random.choice(LOG_MESSAGES)
        attributes = generate_log_attributes()
        attributes["phase"] = "post-spike"

        extra = {"otelAttributes": attributes}
        logger.info(message, extra=extra)
        total_sent += 1
        time.sleep(1.0 / baseline_rate)

    elapsed = time.time() - start_time
    avg_rate = total_sent / elapsed
    print(f"\n✅ Spike test completed: {total_sent} logs in {elapsed:.2f}s ({avg_rate:.0f} logs/sec avg)")

    return elapsed, avg_rate


def send_concurrent_logs(logger, thread_count, logs_per_thread):
    """Send logs from multiple threads concurrently."""
    print(f"🔀 Sending logs from {thread_count} concurrent threads ({logs_per_thread} logs each)...")

    def worker(thread_id):
        for i in range(logs_per_thread):
            level = random.choice(["INFO", "WARNING", "ERROR", "DEBUG"])

            if level == "ERROR":
                message = random.choice(ERROR_MESSAGES)
            elif level == "WARNING":
                message = random.choice(WARNING_MESSAGES)
            else:
                message = random.choice(LOG_MESSAGES)

            attributes = generate_log_attributes()
            attributes["thread_id"] = thread_id
            attributes["thread_index"] = i

            extra = {"otelAttributes": attributes}

            if level == "DEBUG":
                logger.debug(message, extra=extra)
            elif level == "INFO":
                logger.info(message, extra=extra)
            elif level == "WARNING":
                logger.warning(message, extra=extra)
            elif level == "ERROR":
                logger.error(message, extra=extra)

            # Small random delay to simulate realistic workload
            time.sleep(random.uniform(0.001, 0.01))

    start_time = time.time()
    threads = []

    for i in range(thread_count):
        thread = threading.Thread(target=worker, args=(i,))
        threads.append(thread)
        thread.start()

    # Wait for all threads to complete
    for thread in threads:
        thread.join()

    elapsed = time.time() - start_time
    total_logs = thread_count * logs_per_thread
    rate = total_logs / elapsed

    print(f"✅ Sent {total_logs} logs from {thread_count} threads in {elapsed:.2f}s ({rate:.0f} logs/sec)")

    return elapsed, rate


def main():
    parser = argparse.ArgumentParser(
        description="Stress test for Geneva OTAP log exporter pipeline",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Send 1000 logs as fast as possible
  %(prog)s --mode burst --count 1000

  # Send logs at 100/sec for 60 seconds
  %(prog)s --mode sustained --rate 100 --duration 60

  # Test with traffic spikes
  %(prog)s --mode spike --spike-count 5000

  # Test concurrent load from 10 threads
  %(prog)s --mode concurrent --threads 10 --logs-per-thread 100

  # Run all tests sequentially
  %(prog)s --mode all
        """
    )

    parser.add_argument(
        "--mode",
        choices=["burst", "sustained", "spike", "concurrent", "all"],
        default="burst",
        help="Test mode to run"
    )
    parser.add_argument("--count", type=int, default=1000, help="Number of logs for burst mode")
    parser.add_argument("--rate", type=int, default=100, help="Logs per second for sustained mode")
    parser.add_argument("--duration", type=int, default=30, help="Duration in seconds for sustained mode")
    parser.add_argument("--spike-count", type=int, default=2000, help="Number of logs in spike")
    parser.add_argument("--threads", type=int, default=10, help="Number of concurrent threads")
    parser.add_argument("--logs-per-thread", type=int, default=100, help="Logs per thread")
    parser.add_argument("--batch-size", type=int, default=512, help="OTLP batch size")
    parser.add_argument("--queue-size", type=int, default=2048, help="OTLP max queue size")

    args = parser.parse_args()

    print("=" * 80)
    print("Geneva OTAP Pipeline Stress Test")
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

        if args.mode == "spike" or args.mode == "all":
            print("\n" + "=" * 80)
            print("TEST 3: Traffic Spike")
            print("=" * 80)
            elapsed, rate = send_spike_logs(logger, args.spike_count)
            results.append(("Spike", "varies", elapsed, rate))

            print("\nFlushing buffer...")
            logger_provider.force_flush()
            time.sleep(2)

        if args.mode == "concurrent" or args.mode == "all":
            print("\n" + "=" * 80)
            print("TEST 4: Concurrent Load")
            print("=" * 80)
            elapsed, rate = send_concurrent_logs(logger, args.threads, args.logs_per_thread)
            results.append(("Concurrent", args.threads * args.logs_per_thread, elapsed, rate))

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
        print("  - Memory usage and performance")
        print("  - Proper timestamp handling")
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
