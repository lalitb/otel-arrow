# Kubernetes Setup for SAT Extension

This directory contains the Kubernetes manifests required to test the Service Account Token (SAT) extension.

## Prerequisites

- A running Kubernetes cluster (Minikube, Kind, or a real cluster).
- `kubectl` configured to talk to your cluster.

## Setup

1. **Apply the manifests:**

   ```bash
   kubectl apply -f setup.yaml
   ```

   This creates:
   - Namespace: `otel-demo`
   - ServiceAccount: `telemetry-sender` (The client)
   - ServiceAccount: `otel-collector` (The collector)
   - RBAC: Permissions for `otel-collector` to verify tokens.

## Getting the Client Token

To test the extension, you need a valid JWT token for the `telemetry-sender` service account.

**For Kubernetes 1.24+:**

Service Account tokens are no longer automatically created as Secrets. You must request one via the TokenRequest API.

```bash
# Generate a token valid for 1 hour
kubectl create token telemetry-sender -n otel-demo --duration=1h
```

Copy the output string. This is your `Bearer <token>`.

## Running the Collector

When running the Collector (with this extension) inside Kubernetes, you must ensure it runs as the `otel-collector` service account so it has permission to perform reviews.

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: otel-collector
  namespace: otel-demo
spec:
  serviceAccountName: otel-collector
  containers:
  - name: collector
    image: your-collector-image
    # ...
```

## Testing Locally (Outside Cluster)

If you are running the Collector locally (e.g., `cargo run`), you need to provide it with a `kubeconfig` that has permissions to create TokenReviews.

1. Ensure your local `~/.kube/config` is pointing to the cluster.
2. The extension uses the standard Rust `kube` crate, which automatically loads `~/.kube/config`.
3. Use the token generated above in your client request:

```bash
curl -H "Authorization: Bearer <PASTE_TOKEN_HERE>" http://localhost:4318/v1/metrics ...
```
