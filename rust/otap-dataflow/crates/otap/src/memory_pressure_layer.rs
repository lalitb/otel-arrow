// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Tower middleware that rejects requests while hard memory pressure is active.

use futures::future::BoxFuture;
use http::{Request, Response};
use otap_df_engine::memory_budget::SharedRuntimeBudgetPressure;
use otap_df_engine::memory_limiter::SharedReceiverAdmissionState;
use otap_df_telemetry::metrics::MetricSet;
use parking_lot::Mutex;
use std::sync::Arc;
use std::task::{Context, Poll};
use tonic::{Code, Status, body::Body, metadata::MetadataMap};
use tower::{Layer, Service};

use crate::otlp_metrics::OtlpReceiverMetrics;

/// Records request rejections before they enter the pipeline.
pub trait ReceiverRejectionMetrics: Send + Sync {
    /// Records one request rejected before entering the pipeline.
    fn record_rejection(&self);

    /// Records one request rejected before entering the pipeline due to hard memory pressure.
    fn record_memory_pressure_rejection(&self) {
        self.record_rejection();
    }
}

/// Builds a gRPC `resource_exhausted` status with retry pushback metadata.
#[must_use]
pub fn grpc_memory_pressure_status(state: &SharedReceiverAdmissionState) -> Status {
    let mut metadata = MetadataMap::new();
    let retry_pushback_ms = u64::from(state.retry_after_secs().max(1)) * 1_000;
    let _ = metadata.insert(
        "grpc-retry-pushback-ms",
        retry_pushback_ms
            .to_string()
            .parse()
            .expect("retry pushback metadata should be valid ASCII"),
    );
    Status::with_metadata(Code::ResourceExhausted, "memory pressure", metadata)
}

impl ReceiverRejectionMetrics for Mutex<MetricSet<OtlpReceiverMetrics>> {
    fn record_rejection(&self) {
        self.lock().rejected_requests.inc();
    }

    fn record_memory_pressure_rejection(&self) {
        let mut metrics = self.lock();
        metrics.rejected_requests.inc();
        metrics.refused_memory_pressure.inc();
    }
}

/// Layer that fails fast with `resource_exhausted` before tonic decodes request bodies.
///
/// This is only enforced at `Hard` pressure. `Soft` remains advisory in the
/// process-wide state machine for this Phase 1 implementation.
///
/// In addition to process-wide pressure, the layer can optionally consult this
/// runtime's memory-budget pressure through a sendable
/// [`SharedRuntimeBudgetPressure`] view, so the shared (tonic) handler can shed
/// on runtime-budget `Hard` without touching the `!Send` runtime account. That
/// runtime-budget shedding only takes effect when the runtime is in enforce mode
/// with `enforcement.receiver_admission` enabled; otherwise it is shadow-only and
/// never rejects (process-`Hard` behavior is unchanged either way).
#[derive(Clone)]
pub struct MemoryPressureLayer {
    state: SharedReceiverAdmissionState,
    runtime_budget_pressure: Option<SharedRuntimeBudgetPressure>,
    metrics: Option<Arc<dyn ReceiverRejectionMetrics>>,
}

impl MemoryPressureLayer {
    /// Creates a new layer backed by the shared process-wide memory pressure state.
    #[must_use]
    pub const fn new(state: SharedReceiverAdmissionState) -> Self {
        Self {
            state,
            runtime_budget_pressure: None,
            metrics: None,
        }
    }

    /// Creates a new layer that also records dedicated rejection metrics.
    #[must_use]
    pub fn with_metrics<M>(state: SharedReceiverAdmissionState, metrics: Arc<M>) -> Self
    where
        M: ReceiverRejectionMetrics + 'static,
    {
        Self {
            state,
            runtime_budget_pressure: None,
            metrics: Some(metrics),
        }
    }

    /// Creates a new layer that also records dedicated OTLP rejection metrics.
    #[must_use]
    pub fn with_otlp_metrics(
        state: SharedReceiverAdmissionState,
        metrics: Arc<Mutex<MetricSet<OtlpReceiverMetrics>>>,
    ) -> Self {
        Self::with_metrics(state, metrics)
    }

    /// Attaches this runtime's sendable memory-budget pressure view so the layer
    /// also sheds on runtime-budget `Hard` (gated by enforce mode + the
    /// `receiver_admission` gate). `None` keeps the layer process-pressure only.
    #[must_use]
    pub fn with_runtime_budget_pressure(
        mut self,
        runtime_budget_pressure: Option<SharedRuntimeBudgetPressure>,
    ) -> Self {
        self.runtime_budget_pressure = runtime_budget_pressure;
        self
    }
}

impl<S> Layer<S> for MemoryPressureLayer {
    type Service = MemoryPressureService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MemoryPressureService {
            inner,
            state: self.state.clone(),
            runtime_budget_pressure: self.runtime_budget_pressure.clone(),
            metrics: self.metrics.clone(),
            reject_next_call: false,
        }
    }
}

/// Service implementation for [`MemoryPressureLayer`].
#[derive(Clone)]
pub struct MemoryPressureService<S> {
    inner: S,
    state: SharedReceiverAdmissionState,
    runtime_budget_pressure: Option<SharedRuntimeBudgetPressure>,
    metrics: Option<Arc<dyn ReceiverRejectionMetrics>>,
    reject_next_call: bool,
}

impl<S> MemoryPressureService<S> {
    /// Returns whether the current request must be shed, combining process
    /// pressure with this runtime's memory-budget pressure (when installed).
    ///
    /// A few relaxed atomic loads; no allocation, no budget acquisition. With no
    /// runtime-budget pressure installed this is exactly the Phase 1 process-only
    /// `should_shed_ingress()` check, so existing behavior is preserved.
    fn should_reject(&self) -> bool {
        let (runtime_budget_level, runtime_budget_enforce) = match &self.runtime_budget_pressure {
            Some(pressure) => (
                Some(pressure.budget_level()),
                pressure.receiver_admission_enforce(),
            ),
            None => (None, false),
        };
        self.state
            .evaluate(runtime_budget_level, runtime_budget_enforce)
            .is_reject()
    }
}

impl<S, ReqBody> Service<Request<ReqBody>> for MemoryPressureService<S>
where
    S: Service<Request<ReqBody>, Response = Response<Body>> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.should_reject() {
            self.reject_next_call = true;
            return Poll::Ready(Ok(()));
        }
        self.reject_next_call = false;
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        if self.reject_next_call || self.should_reject() {
            self.reject_next_call = false;
            if let Some(metrics) = &self.metrics {
                metrics.record_memory_pressure_rejection();
            }
            let response = grpc_memory_pressure_status(&self.state).into_http();
            return Box::pin(async move { Ok(response) });
        }

        let future = self.inner.call(request);
        Box::pin(future)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Response, StatusCode};
    use otap_df_config::policy::MemoryLimiterMode;
    use otap_df_engine::memory_limiter::{MemoryPressureBehaviorConfig, MemoryPressureState};
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    #[derive(Clone)]
    struct CountingService {
        poll_ready_calls: Arc<AtomicUsize>,
        call_count: Arc<AtomicUsize>,
    }

    impl CountingService {
        fn new() -> Self {
            Self {
                poll_ready_calls: Arc::new(AtomicUsize::new(0)),
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Service<Request<Body>> for CountingService {
        type Response = Response<Body>;
        type Error = Infallible;
        type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            let _ = self.poll_ready_calls.fetch_add(1, Ordering::Relaxed);
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: Request<Body>) -> Self::Future {
            let _ = self.call_count.fetch_add(1, Ordering::Relaxed);
            futures::future::ready(Ok(Response::new(Body::empty())))
        }
    }

    #[test]
    fn hard_pressure_short_circuits_before_inner_readiness_and_call() {
        let state = MemoryPressureState::default();
        state.set_level_for_tests(otap_df_engine::memory_limiter::MemoryPressureLevel::Hard);
        state.configure(MemoryPressureBehaviorConfig {
            retry_after_secs: 3,
            fail_readiness_on_hard: true,
            mode: MemoryLimiterMode::Enforce,
        });

        let inner = CountingService::new();
        let poll_ready_calls = inner.poll_ready_calls.clone();
        let call_count = inner.call_count.clone();

        let mut service =
            MemoryPressureLayer::new(SharedReceiverAdmissionState::from_process_state(&state))
                .layer(inner);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        assert!(matches!(service.poll_ready(&mut cx), Poll::Ready(Ok(()))));

        let response = futures::executor::block_on(service.call(Request::new(Body::empty())))
            .expect("memory pressure rejection should not error");

        assert_eq!(poll_ready_calls.load(Ordering::Relaxed), 0);
        assert_eq!(call_count.load(Ordering::Relaxed), 0);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok()),
            Some("8")
        );
        assert_eq!(
            response
                .headers()
                .get("grpc-retry-pushback-ms")
                .and_then(|v| v.to_str().ok()),
            Some("3000")
        );
    }

    #[test]
    fn hard_rejection_decision_from_poll_ready_is_sticky_for_the_following_call() {
        let state = MemoryPressureState::default();
        state.set_level_for_tests(otap_df_engine::memory_limiter::MemoryPressureLevel::Hard);

        let inner = CountingService::new();
        let poll_ready_calls = inner.poll_ready_calls.clone();
        let call_count = inner.call_count.clone();

        let mut service =
            MemoryPressureLayer::new(SharedReceiverAdmissionState::from_process_state(&state))
                .layer(inner);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        assert!(matches!(service.poll_ready(&mut cx), Poll::Ready(Ok(()))));
        state.set_level_for_tests(otap_df_engine::memory_limiter::MemoryPressureLevel::Normal);

        let response = futures::executor::block_on(service.call(Request::new(Body::empty())))
            .expect("memory pressure rejection should not error");

        assert_eq!(poll_ready_calls.load(Ordering::Relaxed), 0);
        assert_eq!(call_count.load(Ordering::Relaxed), 0);
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn soft_pressure_remains_advisory() {
        let state = MemoryPressureState::default();
        state.set_level_for_tests(otap_df_engine::memory_limiter::MemoryPressureLevel::Soft);

        let inner = CountingService::new();
        let poll_ready_calls = inner.poll_ready_calls.clone();
        let call_count = inner.call_count.clone();

        let mut service =
            MemoryPressureLayer::new(SharedReceiverAdmissionState::from_process_state(&state))
                .layer(inner);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        assert!(matches!(service.poll_ready(&mut cx), Poll::Ready(Ok(()))));

        let response = futures::executor::block_on(service.call(Request::new(Body::empty())))
            .expect("soft pressure should not error");

        assert_eq!(poll_ready_calls.load(Ordering::Relaxed), 1);
        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ----- Runtime-budget admission (Phase 2) -----

    use otap_df_engine::memory_budget::{BudgetLevel, SharedRuntimeBudgetPressure};

    /// Builds a sendable runtime-budget pressure view pinned at `target`, with
    /// the given budget mode and `receiver_admission` gate. Returns the view plus
    /// the keep-alive state/account/ticket the caller must hold for the test
    /// (dropping the account resets the view to a safe `Normal` default).
    fn runtime_pressure(
        mode: otap_df_engine::memory_budget::BudgetMode,
        receiver_admission: bool,
        target: BudgetLevel,
    ) -> (
        SharedRuntimeBudgetPressure,
        otap_df_engine::memory_budget::MemoryBudgetState,
        std::rc::Rc<otap_df_engine::memory_budget::RuntimeMemoryAccount>,
        Option<otap_df_engine::memory_budget::LocalMemoryTicket>,
    ) {
        use otap_df_engine::memory_budget::{
            BudgetScopeId, MemoryBudgetEnforcement, MemoryBudgetSizing, MemoryBudgetState,
            RetainedSiteKind, RuntimeMemoryBudgetConfig,
        };
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 1,
                    lease_step_bytes: 1,
                    max_overshoot_per_runtime_bytes: 10,
                    overshoot_debt_limit_bytes: 10,
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement {
                    receiver_admission,
                    queue_publish: false,
                    reclaim_hooks: false,
                },
            },
            None,
        );
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let pressure = handle.shared_budget_pressure();
        let account = handle.local_account().expect("account");
        let ticket = match target {
            BudgetLevel::Normal => None,
            BudgetLevel::Soft => Some(
                account
                    .charge_at(RetainedSiteKind::Unknown, 5_u64)
                    .expect("soft charge fits within overshoot"),
            ),
            BudgetLevel::Hard => {
                let mut ticket = account
                    .charge_at(RetainedSiteKind::Unknown, 1_u64)
                    .expect("initial charge fits");
                // A plain charge cannot exceed the authorized ceiling under
                // enforce; reconcile a post-hoc overdraw to pin `Hard`.
                ticket.reconcile_size(10_000_000);
                Some(ticket)
            }
        };
        assert_eq!(account.level(), target);
        (pressure, state, account, ticket)
    }

    /// Runs the layer once against a `CountingService` with the given process
    /// pressure state and runtime-budget pressure. Returns
    /// `(inner_call_count, response)`: a rejected request never reaches the inner
    /// service (count 0) and returns a gRPC `resource_exhausted` response.
    fn run_layer_once(
        process_state: &MemoryPressureState,
        runtime_budget_pressure: Option<SharedRuntimeBudgetPressure>,
    ) -> (usize, Response<Body>) {
        let inner = CountingService::new();
        let call_count = inner.call_count.clone();
        let mut service = MemoryPressureLayer::new(
            SharedReceiverAdmissionState::from_process_state(process_state),
        )
        .with_runtime_budget_pressure(runtime_budget_pressure)
        .layer(inner);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let _ = service.poll_ready(&mut cx);
        let response = futures::executor::block_on(service.call(Request::new(Body::empty())))
            .expect("call should not error");
        (call_count.load(Ordering::Relaxed), response)
    }

    fn grpc_status(response: &Response<Body>) -> Option<String> {
        response
            .headers()
            .get("grpc-status")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    #[test]
    fn runtime_budget_hard_enforced_rejects_grpc_request() {
        // Process pressure Normal (default); only the enforced runtime budget
        // at Hard drives the rejection.
        let (pressure, _state, _account, _ticket) = runtime_pressure(
            otap_df_engine::memory_budget::BudgetMode::Enforce,
            true,
            BudgetLevel::Hard,
        );
        let (call_count, response) =
            run_layer_once(&MemoryPressureState::default(), Some(pressure));
        assert_eq!(
            call_count, 0,
            "enforced runtime-budget Hard must reject before inner service"
        );
        assert_eq!(
            grpc_status(&response).as_deref(),
            Some("8"),
            "resource_exhausted"
        );
        assert!(
            response.headers().get("grpc-retry-pushback-ms").is_some(),
            "retry pushback metadata must be preserved"
        );
    }

    #[test]
    fn runtime_budget_hard_observe_only_admits_grpc_request() {
        let (pressure, _state, _account, _ticket) = runtime_pressure(
            otap_df_engine::memory_budget::BudgetMode::ObserveOnly,
            true,
            BudgetLevel::Hard,
        );
        let (call_count, response) =
            run_layer_once(&MemoryPressureState::default(), Some(pressure));
        assert_eq!(call_count, 1, "observe-only runtime budget must not reject");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn runtime_budget_hard_gate_disabled_admits_grpc_request() {
        let (pressure, _state, _account, _ticket) = runtime_pressure(
            otap_df_engine::memory_budget::BudgetMode::Enforce,
            false,
            BudgetLevel::Hard,
        );
        let (call_count, _response) =
            run_layer_once(&MemoryPressureState::default(), Some(pressure));
        assert_eq!(
            call_count, 1,
            "disabled receiver_admission gate must not reject"
        );
    }

    #[test]
    fn runtime_budget_soft_admits_grpc_request() {
        let (pressure, _state, _account, _ticket) = runtime_pressure(
            otap_df_engine::memory_budget::BudgetMode::Enforce,
            true,
            BudgetLevel::Soft,
        );
        let (call_count, _response) =
            run_layer_once(&MemoryPressureState::default(), Some(pressure));
        assert_eq!(
            call_count, 1,
            "Soft runtime-budget pressure is advisory and admits"
        );
    }

    #[test]
    fn no_runtime_pressure_is_process_only() {
        // No runtime pressure installed and process Normal: admit.
        let (call_count, _response) = run_layer_once(&MemoryPressureState::default(), None);
        assert_eq!(
            call_count, 1,
            "no runtime pressure + process Normal => admit"
        );
    }

    #[test]
    fn process_hard_precedence_over_runtime_hard() {
        // Both process and runtime are Hard + enforced: still rejected, with the
        // existing process-pressure response (precedence to process).
        let process = MemoryPressureState::default();
        process.set_level_for_tests(otap_df_engine::memory_limiter::MemoryPressureLevel::Hard);
        let (pressure, _state, _account, _ticket) = runtime_pressure(
            otap_df_engine::memory_budget::BudgetMode::Enforce,
            true,
            BudgetLevel::Hard,
        );
        let (call_count, response) = run_layer_once(&process, Some(pressure));
        assert_eq!(
            call_count, 0,
            "process Hard rejects regardless of runtime budget"
        );
        assert_eq!(grpc_status(&response).as_deref(), Some("8"));
    }
}
