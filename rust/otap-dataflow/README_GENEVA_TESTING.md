# Geneva OTAP End-to-End Testing

Quick guide for testing the complete Geneva OTAP log pipeline.

## Quick Start

### 1. Start the Collector

```bash
cd otel-arrow/rust/otap-dataflow

# First time setup:
# 1. Install protobuf compiler
brew install protobuf  # macOS
# or: apt-get install protobuf-compiler  # Linux

# 2. Initialize git submodules (required for proto files)
git submodule update --init --recursive

# 3. Build and run

# Option A: Single core (recommended for macOS with certificate auth)
cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml --num-cores 1

# Option B: All cores (default, works well on Linux or with MSI auth)
cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml

# Option C: Specific number of cores
cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml --num-cores 4
```

**Note for macOS Users**: If using certificate authentication, use `--num-cores 1` to avoid 
concurrent certificate loading issues with the macOS Security Framework. This is not needed 
on Linux or when using MSI authentication.

The collector will listen on:
- **Port 4317**: gRPC (OTLP receiver - gRPC only)

**Important**: The OTLP receiver in this OTAP dataflow only supports gRPC, not HTTP/JSON.

### 2. Send Test Logs

In a new terminal:

```bash
cd otel-arrow/rust/otap-dataflow

# Option A: Using Python gRPC client (Recommended)
python3 test_geneva_e2e.py

# Option B: Using grpcurl (requires grpcurl to be installed)
./test_geneva_e2e.sh

# Note: test_geneva_curl.sh sends HTTP/JSON which is NOT supported
# by this gRPC-only receiver and will not work
```

## What Happens

```
curl/grpcurl → OTLP Receiver (port 4318/4317)
                     ↓
              OTAP Arrow Format
                     ↓
              Geneva Exporter
                     ↓
         Bond Encoding + LZ4 Compression
                     ↓
            Geneva Backend Upload
```

## Expected Output

### Test Script Output:
```
================================================================================
Geneva OTAP End-to-End Test (using curl)
================================================================================

✓ Collector is running

Sending test logs via OTLP/HTTP (JSON)...
  [1/5] INFO  | Application started
  [2/5] WARN  | High memory usage
  [3/5] ERROR | Database connection failed
  [4/5] INFO  | Request completed
  [5/5] INFO  | User authenticated

✓ All 5 test logs sent successfully
```

### Collector Output:
```
DEBUG geneva-uploader: Encoding and compressing OTAP logs log_count=5
DEBUG geneva-uploader: Encoded OTAP log batch event_name=ApplicationLog 
    schemas=1 events=1 uncompressed_size=1750 compressed_size=765
DEBUG geneva-uploader: Uploading batch event_name=ApplicationLog size=765
```

**Note**: Upload will fail without a real Geneva backend - this is expected! The test verifies that logs are:
1. ✅ Received by OTLP
2. ✅ Converted to OTAP Arrow
3. ✅ Encoded with Bond
4. ✅ Compressed with LZ4 (~60% reduction)
5. ✅ Attempted upload to Geneva

## Configuration

Test configuration: `configs/otlp-geneva-test.yaml`
- Small buffers for quick testing
- Mock authentication
- Logs to console with `RUST_LOG=debug`

Production template: `configs/geneva-config-example.yaml`
- Real Geneva endpoint
- Certificate authentication  
- Larger buffers (1000 records)
- More concurrent uploads (4)

## Troubleshooting

**Error: `protoc` not found**
```bash
brew install protobuf
```

**Error: Port already in use**
```bash
lsof -ti:4318 | xargs kill -9
```

**No debug output**
```bash
RUST_LOG=debug cargo run --bin df_engine -- --pipeline configs/otlp-geneva-test.yaml
```

## Next Steps

1. Verify logs are encoded correctly (check `compressed_size`)
2. Deploy with real Geneva endpoint
3. Monitor Geneva portal for incoming logs
4. Tune buffer sizes for your workload

For more details, see the configuration examples in the `configs/` directory.
