//! Rate-limiting and deduplicating filter for `tracing` events (`no_std` over `ekostd`).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use eko::time::Instant;
use parking_lot::Mutex;
use tracing::Event;

const DEFAULT_BURST_CAPACITY: f64 = 50.0;
const DEFAULT_REFILL_PER_SEC: f64 = 1.0;
const DEFAULT_MAX_SIGNATURES: usize = 10_000;

/// Point-in-time snapshot of rate limiting metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MetricsSnapshot {
    pub events_allowed: u64,
    pub events_suppressed: u64,
    pub signatures_evicted: u64,
}

impl MetricsSnapshot {
    pub fn suppression_rate(&self) -> f64 {
        let total = self.events_allowed + self.events_suppressed;
        if total == 0 {
            0.0
        } else {
            (self.events_suppressed as f64) / (total as f64)
        }
    }
}

/// Shared atomic counters tracking allowed, suppressed, and evicted events.
#[derive(Debug, Default)]
pub struct RateLimitMetrics {
    allowed: AtomicU64,
    suppressed: AtomicU64,
    evicted: AtomicU64,
}

impl RateLimitMetrics {
    pub fn events_allowed(&self) -> u64 {
        self.allowed.load(Ordering::Relaxed)
    }

    pub fn events_suppressed(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }

    pub fn signatures_evicted(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            events_allowed: self.events_allowed(),
            events_suppressed: self.events_suppressed(),
            signatures_evicted: self.signatures_evicted(),
        }
    }

    fn record_allowed(&self) {
        self.allowed.fetch_add(1, Ordering::Relaxed);
    }

    fn record_suppressed(&self) {
        self.suppressed.fetch_add(1, Ordering::Relaxed);
    }

    fn record_evicted(&self) {
        self.evicted.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
    last_seen_seq: u64,
    suppressed_since_summary: u64,
    sample_message: String,
    target: String,
}

#[derive(Debug)]
struct ThrottlerState {
    buckets: BTreeMap<String, BucketState>,
    seq: u64,
    last_summary: Instant,
    shutdown: bool,
}

#[derive(Debug)]
struct Inner {
    excluded_fields: BTreeSet<String>,
    exempt_targets: Vec<String>,
    active_emission: bool,
    summary_interval: Duration,
    burst_capacity: f64,
    refill_per_sec: f64,
    max_signatures: usize,
    metrics: Arc<RateLimitMetrics>,
    state: Mutex<ThrottlerState>,
}

/// Builder for [`TracingRateLimitLayer`].
#[derive(Debug, Clone)]
pub struct TracingRateLimitLayerBuilder {
    excluded_fields: Vec<String>,
    exempt_targets: Vec<String>,
    active_emission: bool,
    summary_interval: Duration,
    burst_capacity: f64,
    refill_per_sec: f64,
    max_signatures: usize,
}

impl Default for TracingRateLimitLayerBuilder {
    fn default() -> Self {
        Self {
            excluded_fields: Vec::new(),
            exempt_targets: Vec::new(),
            active_emission: false,
            summary_interval: Duration::from_secs(5 * 60),
            burst_capacity: DEFAULT_BURST_CAPACITY,
            refill_per_sec: DEFAULT_REFILL_PER_SEC,
            max_signatures: DEFAULT_MAX_SIGNATURES,
        }
    }
}

impl TracingRateLimitLayerBuilder {
    pub fn with_excluded_fields(mut self, fields: impl IntoIterator<Item = String>) -> Self {
        self.excluded_fields = fields.into_iter().collect();
        self
    }

    pub fn with_exempt_targets(mut self, targets: impl IntoIterator<Item = String>) -> Self {
        self.exempt_targets = targets.into_iter().collect();
        self
    }

    pub fn with_active_emission(mut self, enabled: bool) -> Self {
        self.active_emission = enabled;
        self
    }

    pub fn with_summary_interval(mut self, interval: Duration) -> Self {
        self.summary_interval = interval;
        self
    }

    pub fn with_max_signatures(mut self, max_signatures: usize) -> Self {
        self.max_signatures = max_signatures;
        self
    }

    pub fn build(self) -> crate::eyre::Result<TracingRateLimitLayer> {
        let now = Instant::now();
        Ok(TracingRateLimitLayer {
            inner: Arc::new(Inner {
                excluded_fields: self.excluded_fields.into_iter().collect(),
                exempt_targets: self.exempt_targets,
                active_emission: self.active_emission,
                summary_interval: self.summary_interval,
                burst_capacity: self.burst_capacity,
                refill_per_sec: self.refill_per_sec,
                max_signatures: self.max_signatures.max(1),
                metrics: Arc::new(RateLimitMetrics::default()),
                state: Mutex::new(ThrottlerState {
                    buckets: BTreeMap::new(),
                    seq: 0,
                    last_summary: now,
                    shutdown: false,
                }),
            }),
        })
    }
}

/// Token-bucket log rate-limiting filter and handle.
#[derive(Debug, Clone)]
pub struct TracingRateLimitLayer {
    inner: Arc<Inner>,
}

impl Default for TracingRateLimitLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl TracingRateLimitLayer {
    pub fn new() -> Self {
        Self::builder()
            .build()
            .expect("default rate limit configuration is valid")
    }

    pub fn builder() -> TracingRateLimitLayerBuilder {
        TracingRateLimitLayerBuilder::default()
    }

    pub fn metrics(&self) -> &Arc<RateLimitMetrics> {
        &self.inner.metrics
    }

    /// Gracefully shut down active emission and clear state.
    pub fn shutdown(&self) {
        let mut state = self.inner.state.lock();
        state.shutdown = true;
        state.buckets.clear();
    }

    fn is_exempt(&self, target: &str) -> bool {
        self.inner.exempt_targets.iter().any(|exempt| {
            target == exempt
                || (target.len() > exempt.len()
                    && target.starts_with(exempt.as_str())
                    && target.as_bytes().get(exempt.len()..exempt.len() + 2) == Some(b"::"))
        })
    }

    /// Evaluate whether `event` should be emitted, returning any due suppression summaries.
    pub(crate) fn check_event(&self, event: &Event<'_>) -> (bool, Vec<String>) {
        let target = event.metadata().target();
        if self.is_exempt(target) {
            self.inner.metrics.record_allowed();
            return (true, Vec::new());
        }

        let mut sig_visitor = SignatureVisitor {
            excluded: &self.inner.excluded_fields,
            signature: format!("{}|{target}", event.metadata().level()),
            message: String::new(),
        };
        event.record(&mut sig_visitor);

        let now = Instant::now();
        let mut summaries = Vec::new();
        let allowed = {
            let mut state = self.inner.state.lock();
            if state.shutdown {
                self.inner.metrics.record_allowed();
                return (true, Vec::new());
            }
            state.seq = state.seq.wrapping_add(1);
            let seq = state.seq;

            if self.inner.active_emission
                && now.duration_since(state.last_summary) >= self.inner.summary_interval
            {
                state.last_summary = now;
                for bucket in state.buckets.values_mut() {
                    if bucket.suppressed_since_summary > 0 {
                        summaries.push(format!(
                            "Suppressed {} repetitive log events from {}: {}",
                            bucket.suppressed_since_summary, bucket.target, bucket.sample_message
                        ));
                        bucket.suppressed_since_summary = 0;
                    }
                }
            }

            if let Some(bucket) = state.buckets.get_mut(&sig_visitor.signature) {
                let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
                if elapsed > 0.0 {
                    let replenished = bucket.tokens + elapsed * self.inner.refill_per_sec;
                    bucket.tokens = if replenished < self.inner.burst_capacity {
                        replenished
                    } else {
                        self.inner.burst_capacity
                    };
                    bucket.last_refill = now;
                }
                bucket.last_seen_seq = seq;
                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    true
                } else {
                    bucket.suppressed_since_summary =
                        bucket.suppressed_since_summary.saturating_add(1);
                    false
                }
            } else {
                if state.buckets.len() >= self.inner.max_signatures {
                    if let Some(oldest_key) = state
                        .buckets
                        .iter()
                        .min_by_key(|(_, b)| b.last_seen_seq)
                        .map(|(k, _)| k.clone())
                    {
                        state.buckets.remove(&oldest_key);
                        self.inner.metrics.record_evicted();
                    }
                }
                state.buckets.insert(
                    sig_visitor.signature,
                    BucketState {
                        tokens: (self.inner.burst_capacity - 1.0).max(0.0),
                        last_refill: now,
                        last_seen_seq: seq,
                        suppressed_since_summary: 0,
                        sample_message: sig_visitor.message,
                        target: String::from(target),
                    },
                );
                true
            }
        };

        if allowed {
            self.inner.metrics.record_allowed();
        } else {
            self.inner.metrics.record_suppressed();
        }
        (allowed, summaries)
    }
}

struct SignatureVisitor<'a> {
    excluded: &'a BTreeSet<String>,
    signature: String,
    message: String,
}

impl tracing::field::Visit for SignatureVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        let name = field.name();
        if name == "message" {
            let _ = write!(self.message, "{value:?}");
        }
        if !self.excluded.contains(name) {
            let _ = write!(self.signature, "|{name}={value:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Subscriber;
    use tracing::span;

    struct ThrottlingTestSubscriber {
        layer: TracingRateLimitLayer,
        emitted: Mutex<Vec<String>>,
        summaries: Mutex<Vec<String>>,
    }

    impl Subscriber for ThrottlingTestSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}
        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}
        fn event(&self, event: &Event<'_>) {
            let (allowed, due) = self.layer.check_event(event);
            if !due.is_empty() {
                self.summaries.lock().extend(due);
            }
            if allowed {
                self.emitted
                    .lock()
                    .push(String::from(event.metadata().target()));
            }
        }
        fn enter(&self, _span: &span::Id) {}
        fn exit(&self, _span: &span::Id) {}
    }

    #[test]
    fn burst_suppression_excluded_fields_exempt_targets_and_eviction() {
        let mut builder = TracingRateLimitLayer::builder()
            .with_excluded_fields([String::from("req_id")])
            .with_exempt_targets([String::from("audit")])
            .with_active_emission(true)
            .with_summary_interval(Duration::from_millis(1))
            .with_max_signatures(2);
        builder.burst_capacity = 2.0;
        builder.refill_per_sec = 0.0;
        let layer = builder.build().unwrap();

        let sub = Arc::new(ThrottlingTestSubscriber {
            layer: layer.clone(),
            emitted: Mutex::new(Vec::new()),
            summaries: Mutex::new(Vec::new()),
        });

        crate::libs::log::with_test_subscriber(sub.clone(), || {
            // Varying `req_id` is excluded, so all 4 share one signature: 2 allowed, 2 suppressed.
            for id in 0..4 {
                tracing::warn!(target: "worker", req_id = id, "overload");
            }
            let snap = layer.metrics().snapshot();
            assert_eq!(snap.events_allowed, 2);
            assert_eq!(snap.events_suppressed, 2);
            assert!((snap.suppression_rate() - 0.5).abs() < f64::EPSILON);

            // Exempt target and sub-targets are never suppressed.
            for _ in 0..5 {
                tracing::error!(target: "audit::login", "auth failure");
            }
            assert_eq!(layer.metrics().events_allowed(), 7);
            assert_eq!(layer.metrics().events_suppressed(), 2);

            // Exceed `max_signatures = 2` to trigger eviction and emit due summary.
            eko::sleep_ms(2);
            tracing::info!(target: "worker", "second distinct signature");
            tracing::info!(target: "worker", "third distinct signature");
            assert_eq!(layer.metrics().signatures_evicted(), 1);
            assert!(!sub.summaries.lock().is_empty());
        });

        layer.shutdown();
    }
}
