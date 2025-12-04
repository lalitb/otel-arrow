# Discussion Plan: Parquet Exporter & Extension Architecture

**Goal:** Align on a decoupled architecture for Authentication and shared capabilities, guiding the design toward the OpenTelemetry "Extension" pattern.

## 1. The "Why" - Identifying Pain Points
*Start by asking questions to highlight the limitations of the current approach.*

*   **Authentication Coupling:**
    *   "Does the Parquet exporter currently handle authentication (e.g., to S3/Azure/GCS) internally?"
    *   "If we add another exporter later (e.g., Arrow, Log), do we want to copy-paste that auth logic and config parsing?"
*   **Complexity & Bloat:**
    *   "Static keys are simple, but what if a user wants AWS IAM Roles, Vault, or Kubernetes Service Accounts?"
    *   "Do we want to bloat the Parquet exporter crate with all those SDK dependencies?"
*   **Shared State:**
    *   "If multiple components need to share a connection or a heartbeat, how do they do that currently?"

## 2. The Reference - Go Collector Architecture
*Introduce the standard pattern as a solution, not just "my idea".*

*   **Decoupling:**
    *   "In the main OpenTelemetry Collector, they decouple 'Components' (Exporters) from 'Extensions' (Auth/Config)."
    *   "The Exporter doesn't know *how* to authenticate; it just asks a 'Host' for an 'Authenticator'."
*   **The Question:**
    *   "Do you think we should adopt a similar pattern in Rust so we don't reinvent the wheel?"

## 3. The Proposal - Technical Design
*Steer the design toward the traits and patterns we want.*

*   **Interfaces:**
    *   "We'd probably need a generic `Extension` trait for lifecycle management."
    *   "Specific traits like `ClientAuth` (for exporters) and `ServerAuth` (for receivers) would define the contract."
*   **Injection:**
    *   "For the Parquet exporter, we could update the `start()` method to accept a `Host` object."
    *   "This `Host` would act as a registry to look up extensions by ID."

## 4. The "Hypothetical" Workflow
*Show how clean the configuration and code would be.*

**Configuration (YAML):**
```yaml
extensions:
  # Auth logic is defined once, here
  aws_auth/s3:
    type: aws_iam
    role: "arn:aws:iam::..."

exporters:
  parquet:
    bucket: "my-data"
    # The exporter just references the auth extension
    auth_extension: "aws_auth/s3" 
```

**Code Structure:**
*   The Parquet exporter code stays clean:
    ```rust
    // Hypothetical usage inside exporter
    let auth = host.get_client_auth("aws_auth/s3")?;
    let creds = auth.get_credentials()?;
    // Use creds to write to S3...
    ```

## 5. Action Items / Next Steps
*   **Consensus:** Agree that decoupling Auth from the Exporter is the right move.
*   **The "Volunteer":**
    *   "This seems like a prerequisite for a production-ready system."
    *   "Why don't I take a stab at defining these interfaces (`Extension`, `Host`, `Auth`) based on the Go specs?"
    *   *(This allows introducing the existing `otap-df-extension` crate as the "draft" implementation later.)*

## 6. Appendix: Case Study - Kubernetes SAT Auth
*Use this as a concrete example of "Complex Auth" that shouldn't be in the exporter.*

**The Problem:**
We want to authenticate telemetry senders using Kubernetes Service Account Tokens. This requires:
1.  Connecting to the K8s API (`k8s-openapi`, `kube` crates).
2.  Calling the `TokenReview` API.
3.  Mapping Service Accounts to allowed resources (RBAC).

**The "Extension" Solution:**
Instead of adding `kube` dependencies to the Parquet exporter, we use the `sat_auth` extension.

**Setup Instructions (Reference):**
1.  **Manifests:** We have a `k8s/setup.yaml` that creates:
    *   `telemetry-sender` (Client SA)
    *   `otel-collector` (Collector SA with `system:auth-delegator` role)
2.  **Token Generation:**
    ```bash
    kubectl create token telemetry-sender -n otel-demo --duration=1h
    ```
3.  **Collector Config:**
    ```yaml
    extensions:
      k8s_sat:
        extension_urn: "urn:otel:extension:auth:sat"
        config:
          extension_auth_configs:
            - extension_rid: "parquet-writer"
              service_account_names: ["telemetry-sender"]
    ```
4.  **Result:** The Parquet exporter just calls `host.authenticate(headers)` and gets back the principal `system:serviceaccount:otel-demo:telemetry-sender`. It knows nothing about Kubernetes.
