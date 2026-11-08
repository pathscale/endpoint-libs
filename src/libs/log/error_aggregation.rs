/// Error Aggregation - Collection of recent errors with optional deduping and other convenience stuff
use std::cmp::Ordering;
use std::fmt::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Weak};

use itertools::Itertools;
use tracing_subscriber::Layer;

// Public re-export of Rotation so clients don't need to include tracing_appender just for log setup
pub use tracing_appender::rolling::Rotation as LogRotation;

use std::collections::HashMap;

use std::hash::Hash;

const PENDING_CAPACITY: usize = 256;
const MAX_EVENT_BYTES: usize = 8 * 1024;

// Version with error aggregation feature
pub fn get_error_aggregation(
    error_aggregation: ErrorAggregationConfig,
) -> (Arc<ErrorAggregationContainer>, ErrorAggregationLayer) {
    // Create error aggregation container and layer
    let error_container = Arc::new(ErrorAggregationContainer::new(error_aggregation));
    let error_layer = ErrorAggregationLayer {
        ingress: Arc::downgrade(&error_container.ingress),
    };

    (error_container, error_layer)
}

/// Error object provided for convenience for display in UI etc.
#[derive(Debug, Clone)]
pub struct ErrorEntry {
    pub message: String,
    pub timestamp: i64, // Unix timestamp of last occurrence in milliseconds
    pub target: String, // Module path (e.g., "core::math_utils")
    pub count: usize,
}

/// Statistics for deduplicated errors
#[derive(Debug, Clone)]
pub struct ErrorStats {
    pub count: usize,
    pub first_seen: i64,        // Unix timestamp in milliseconds
    pub last_seen: i64,         // Unix timestamp in milliseconds
    pub message: String,        // Most recent message variant
    pub target: String,         // Module path
    pub normalized_key: String, // Normalized pattern for debugging
}

/// Key for deduplication HashMap
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ErrorKey {
    target: String,
    normalized_message: String,
}

/// Configuration for error aggregation
#[derive(Debug, Clone)]
pub struct ErrorAggregationConfig {
    pub limit: usize,    // Maximum errors to keep
    pub normalize: bool, // Whether to normalize messages for deduplication
}

impl Default for ErrorAggregationConfig {
    fn default() -> Self {
        Self {
            limit: 100,
            normalize: true,
        }
    }
}

/// Internal storage representation
#[derive(Debug)]
struct ErrorStorage {
    storage: HashMap<ErrorKey, ErrorStats>,
}

impl ErrorStorage {
    pub fn new() -> Self {
        Self {
            storage: HashMap::new(),
        }
    }

    pub fn get_map(&self) -> &HashMap<ErrorKey, ErrorStats> {
        &self.storage
    }
}

#[derive(Debug)]
struct ErrorIngress {
    sender: async_channel::Sender<ErrorEntry>,
    dropped: AtomicUsize,
}

impl ErrorIngress {
    fn record_drop(&self) {
        let _ = self
            .dropped
            .fetch_update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
    }
}

/// Container for aggregated errors with async query methods.
///
/// The aggregation layer admits up to 256 pending ERROR events without waiting
/// for space. The worker can hold one additional event while processing it.
/// Full queues reject the incoming event. The rendered target and message are
/// limited to 8 KiB combined. See [Self::dropped_events] for admission loss.
///
/// Dropping the last container owner closes admission and cancels the worker.
/// Pending entries may be discarded during shutdown.
#[derive(Debug)]
pub struct ErrorAggregationContainer {
    storage: Arc<nagoya::sync::RwLock<ErrorStorage>>,
    ingress: Arc<ErrorIngress>,
    /// `Option` only so [`Drop`] can take the handle: nagoya's
    /// `JoinHandle::cancel` consumes it, where tokio's `abort` took `&self`.
    task_handle: Option<nagoya::JoinHandle<()>>,
    config: ErrorAggregationConfig,
}

pub type ErrorSortingFn = Box<dyn FnMut(&ErrorEntry, &ErrorEntry) -> Ordering + 'static>;

impl ErrorAggregationContainer {
    /// Create a new error aggregation container
    pub fn new(config: ErrorAggregationConfig) -> Self {
        let storage = ErrorStorage::new();
        let storage = Arc::new(nagoya::sync::RwLock::new(storage));

        let (sender, receiver) = async_channel::bounded(PENDING_CAPACITY);
        let ingress = Arc::new(ErrorIngress {
            sender,
            dropped: AtomicUsize::new(0),
        });

        // Spawn background aggregation task
        let storage_clone = Arc::clone(&storage);
        let config_clone = config.clone();
        let task_handle = Some(nagoya::runtime::background().spawn(aggregation_task(
            receiver,
            storage_clone,
            config_clone,
        )));

        Self {
            storage,
            ingress,
            task_handle,
            config,
        }
    }

    /// Count of ERROR events rejected by this aggregator because the queue was
    /// full or the event could not be rendered within its byte budget.
    ///
    /// This cumulative count saturates at `usize::MAX`. Clearing stored errors
    /// leaves it intact. Retention evictions and shutdown discards are separate
    /// from admission loss.
    pub fn dropped_events(&self) -> usize {
        self.ingress.dropped.load(AtomicOrdering::Relaxed)
    }

    /// Get errors with pagination support and optional custom sorting. Sorts by most-recent by default
    /// TODO: performance. Figure out how to paginate and sort without having to read/iterate over the entire container
    pub async fn get_errors(
        &self,
        limit: usize,
        offset: usize,
        sort_by: Option<ErrorSortingFn>,
    ) -> Vec<ErrorEntry> {
        let storage = self.storage.read().await;
        let map = storage.get_map();

        // Convert stats to entries
        let mut entries: Vec<ErrorEntry> = map
            .values()
            .map(|stats| ErrorEntry {
                message: stats.message.clone(),
                timestamp: stats.last_seen,
                target: stats.target.clone(),
                count: stats.count,
            })
            .collect();

        if let Some(mut cmp) = sort_by {
            entries.sort_by(|a, b| cmp(a, b));
        } else {
            entries.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
        }

        entries.into_iter().skip(offset).take(limit).collect()
    }

    /// Get error statistics with pagination
    pub async fn get_stats(&self, limit: usize, offset: usize) -> Vec<ErrorStats> {
        let storage = self.storage.read().await;
        let map = storage.get_map();

        let mut stats: Vec<ErrorStats> = map.values().cloned().collect();
        stats.sort_by_key(|s| std::cmp::Reverse(s.last_seen));
        stats.into_iter().skip(offset).take(limit).collect()
    }

    /// Get total count of unique errors
    pub async fn count(&self) -> usize {
        let storage = self.storage.read().await;
        storage.get_map().len()
    }

    /// Clear all errors
    pub async fn clear(&self) {
        let mut storage = self.storage.write().await;
        storage.storage.clear();
    }

    /// Get all errors (convenience method)
    pub async fn get_all_errors(&self) -> Vec<ErrorEntry> {
        self.get_errors(self.config.limit, 0, None).await
    }

    /// Get latest N errors (convenience method)
    pub async fn get_latest(&self, n: usize) -> Vec<ErrorEntry> {
        self.get_errors(n, 0, None).await
    }
}

impl Drop for ErrorAggregationContainer {
    fn drop(&mut self) {
        self.ingress.sender.close();
        if let Some(handle) = self.task_handle.take() {
            handle.cancel();
        }
    }
}

/// Background task that aggregates errors from the channel
async fn aggregation_task(
    receiver: async_channel::Receiver<ErrorEntry>,
    storage: Arc<nagoya::sync::RwLock<ErrorStorage>>,
    config: ErrorAggregationConfig,
) {
    while let Ok(entry) = receiver.recv().await {
        let mut storage_lock = storage.write().await;
        let map = &mut storage_lock.storage;

        // Construct key based on mode
        let key = ErrorKey {
            target: entry.target.clone(),
            normalized_message: if config.normalize {
                normalize_message(&entry.message)
            } else {
                entry.message.clone() // Use raw message as key
            },
        };

        if let Some(stats) = map.get_mut(&key) {
            // Update existing entry
            stats.count += 1;
            stats.last_seen = entry.timestamp;
            stats.message = entry.message; // Always store the latest actual message
        } else {
            // New entry - check limit
            if map.len() >= config.limit {
                // Evict oldest entry by first_seen timestamp
                if let Some(oldest_key) = map
                    .iter()
                    .min_by_key(|(_, stats)| stats.first_seen)
                    .map(|(k, _)| k.clone())
                {
                    map.remove(&oldest_key);
                }
            }

            map.insert(
                key.clone(),
                ErrorStats {
                    count: 1,
                    first_seen: entry.timestamp,
                    last_seen: entry.timestamp,
                    message: entry.message,
                    target: entry.target,
                    normalized_key: key.normalized_message,
                },
            );
        }
    }
}

/// Message normalization for deduplication
fn normalize_message(message: &str) -> String {
    use lazy_static::lazy_static;
    use regex::Regex;

    lazy_static! {
        // IPv4 addresses
        static ref IP_PATTERN: Regex = Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap();

        // UUIDs (both with and without hyphens)
        static ref UUID_PATTERN: Regex = Regex::new(
            r"\b[0-9a-fA-F]{8}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{12}\b"
        ).unwrap();

        // Numbers (integers and floats)
        static ref NUMBER_PATTERN: Regex = Regex::new(r"\b\d+\.?\d*\b").unwrap();

        // File paths (Unix and Windows style)
        static ref PATH_PATTERN: Regex = Regex::new(
            r"(?:/[\w\-./]+)|(?:[A-Z]:\\[\w\-\\./]+)"
        ).unwrap();

        // Hex strings (0x prefix or just hex)
        static ref HEX_PATTERN: Regex = Regex::new(r"\b0x[0-9a-fA-F]+\b").unwrap();

        // Timestamps (ISO 8601 and common formats)
        static ref TIMESTAMP_PATTERN: Regex = Regex::new(
            r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?"
        ).unwrap();

        // Whitespace collapsing
        static ref WHITESPACE_PATTERN: Regex = Regex::new(r"\s+").unwrap();
    }

    let mut normalized = message.to_string();

    // Apply normalizations in order
    normalized = TIMESTAMP_PATTERN
        .replace_all(&normalized, "<TIMESTAMP>")
        .to_string();
    normalized = UUID_PATTERN.replace_all(&normalized, "<UUID>").to_string();
    normalized = IP_PATTERN.replace_all(&normalized, "<IP>").to_string();
    normalized = PATH_PATTERN.replace_all(&normalized, "<PATH>").to_string();
    normalized = HEX_PATTERN.replace_all(&normalized, "<HEX>").to_string();
    normalized = NUMBER_PATTERN.replace_all(&normalized, "<NUM>").to_string();

    // Collapse multiple spaces
    normalized = WHITESPACE_PATTERN.replace_all(&normalized, " ").to_string();

    normalized.trim().to_string()
}

/// Tracing layer that captures ERROR level events and sends them to the aggregator
pub struct ErrorAggregationLayer {
    ingress: Weak<ErrorIngress>,
}

impl<S: tracing::Subscriber> Layer<S> for ErrorAggregationLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // Only capture ERROR level events
        if !event.metadata().level().eq(&tracing::Level::ERROR) {
            return;
        }

        let Some(ingress) = self.ingress.upgrade() else {
            return;
        };
        if ingress.sender.is_closed() {
            return;
        }

        let Some(entry) = error_entry(event) else {
            ingress.record_drop();
            return;
        };
        if let Err(async_channel::TrySendError::Full(_)) = ingress.sender.try_send(entry) {
            ingress.record_drop();
        }
    }
}

fn error_entry(event: &tracing::Event<'_>) -> Option<ErrorEntry> {
    let metadata_target = event.metadata().target();
    let caller_location = event
        .fields()
        .find_or_first(|field| field.name() == "caller_location");

    let mut target = BoundedText::new(MAX_EVENT_BYTES);
    if let Some(location) = caller_location {
        write!(target, "{location} ({metadata_target})").ok()?;
    } else {
        target.write_str(metadata_target).ok()?;
    }

    let timestamp = chrono::Utc::now().timestamp_millis();
    let mut message = BoundedText::new(MAX_EVENT_BYTES - target.text.len());
    event.record(&mut MessageVisitor(&mut message));
    if message.failed {
        return None;
    }

    let mut message = message.text;
    // Preserve the existing display of Debug values surrounded by quotes.
    if message.len() > 1 && message.starts_with('"') && message.ends_with('"') {
        message.pop();
        message.remove(0);
    }

    Some(ErrorEntry {
        message,
        timestamp,
        target: target.text,
        count: 1,
    })
}

struct BoundedText {
    text: String,
    limit: usize,
    failed: bool,
}

impl BoundedText {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            failed: false,
        }
    }
}

impl Write for BoundedText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.failed || text.len() > self.limit - self.text.len() {
            self.failed = true;
            return Err(fmt::Error);
        }
        self.text.push_str(text);
        Ok(())
    }
}

/// Visitor to extract message field from tracing Event
struct MessageVisitor<'a>(&'a mut BoundedText);

impl<'a> tracing::field::Visit for MessageVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && write!(self.0, "{value:?}").is_err() {
            self.0.failed = true;
        }
    }
}

#[cfg(all(test, feature = "error_aggregation"))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tracing_subscriber::prelude::*;

    struct ErrorObserver(Arc<AtomicUsize>);

    impl<S: tracing::Subscriber> Layer<S> for ErrorObserver {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().level() == &tracing::Level::ERROR {
                self.0.fetch_add(1, AtomicOrdering::Relaxed);
            }
        }
    }

    #[test]
    fn aggregation_ingress_saturation_recovers_and_closes() {
        nagoya::block_on(nagoya::timeout(Duration::from_secs(10), async {
            let (container, layer) = get_error_aggregation(ErrorAggregationConfig::default());
            let observed = Arc::new(AtomicUsize::new(0));
            let dispatch = tracing::Dispatch::new(
                tracing_subscriber::registry()
                    .with(layer)
                    .with(ErrorObserver(Arc::clone(&observed))),
            );
            let emit_queued = || {
                tracing::dispatcher::with_default(&dispatch, || {
                    tracing::error!("queued error");
                });
            };
            let storage = Arc::clone(&container.storage);
            let blocked = storage.write().await;

            // The worker takes one event before waiting on the storage lock.
            emit_queued();
            while !container.ingress.sender.is_empty() {
                nagoya::sleep(Duration::from_millis(1)).await;
            }
            for _ in 0..PENDING_CAPACITY {
                emit_queued();
            }
            assert_eq!(container.dropped_events(), 0);
            tracing::dispatcher::with_default(&dispatch, || {
                tracing::error!("rejected at capacity");
            });
            assert_eq!(container.dropped_events(), 1);
            drop(blocked);

            loop {
                let errors = container.get_all_errors().await;
                if errors.iter().map(|entry| entry.count).sum::<usize>() == PENDING_CAPACITY + 1 {
                    assert_eq!(errors.len(), 1);
                    assert_eq!(errors[0].message, "queued error");
                    break;
                }
                nagoya::sleep(Duration::from_millis(1)).await;
            }

            tracing::dispatcher::with_default(&dispatch, || {
                tracing::error!("{}", "é".repeat(MAX_EVENT_BYTES / 2 + 1));
                tracing::error!("admitted after draining");
            });
            assert_eq!(container.dropped_events(), 2);
            loop {
                let errors = container.get_all_errors().await;
                if errors.iter().any(|entry| entry.message == "admitted after draining") {
                    assert_eq!(errors.len(), 2);
                    break;
                }
                nagoya::sleep(Duration::from_millis(1)).await;
            }
            container.clear().await;
            assert_eq!(container.dropped_events(), 2);

            let _blocked = storage.write().await;
            emit_queued();
            while !container.ingress.sender.is_empty() {
                nagoya::sleep(Duration::from_millis(1)).await;
            }
            emit_queued();
            // Simulate an in-flight callback retaining ingress during shutdown.
            let in_flight = Arc::clone(&container.ingress);
            let ingress = Arc::downgrade(&container.ingress);
            drop(container);
            assert!(in_flight.sender.is_closed());
            emit_queued();
            drop(in_flight);
            assert!(ingress.upgrade().is_none());
            while Arc::strong_count(&storage) != 1 {
                nagoya::sleep(Duration::from_millis(1)).await;
            }
            assert_eq!(observed.load(AtomicOrdering::Relaxed), PENDING_CAPACITY + 7);
        }))
        .expect("aggregation should admit, recover, and cancel within the deadline");
    }

    #[test]
    fn test_message_normalization() {
        // Test IP normalization
        assert_eq!(
            normalize_message("Failed to connect to 192.168.1.1"),
            "Failed to connect to <IP>"
        );

        // Test UUID normalization
        assert_eq!(
            normalize_message("User 550e8400-e29b-41d4-a716-446655440000 not found"),
            "User <UUID> not found"
        );

        // Test number normalization
        assert_eq!(normalize_message("Error on line 42"), "Error on line <NUM>");

        // Test path normalization
        assert_eq!(
            normalize_message("Failed to read /var/log/app.log"),
            "Failed to read <PATH>"
        );

        // Test hex normalization
        assert_eq!(
            normalize_message("Memory address 0xdeadbeef"),
            "Memory address <HEX>"
        );

        // Test combined normalization
        assert_eq!(
            normalize_message("Connection to 10.0.0.1:8080 failed at /home/user/file.txt"),
            "Connection to <IP>:<NUM> failed at <PATH>"
        );
    }

    // The bodies below genuinely need a runtime: the container's query methods
    // are `async`, and the aggregation task they wait on runs on nagoya's shared
    // pool. Each test is a plain `#[test]` that drives its own body with
    // `nagoya::block_on`, which parks this thread between polls rather than
    // spinning, and the body stays an `async fn` so the diff is the wrapper.
    #[test]
    fn test_raw_mode_eviction() {
        nagoya::block_on(test_raw_mode_eviction_body());
    }

    async fn test_raw_mode_eviction_body() {
        let config = ErrorAggregationConfig {
            limit: 3,
            normalize: false,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add 4 entries, oldest should be evicted when limit exceeded
        for i in 0..4 {
            container
                .ingress
                .sender
                .try_send(ErrorEntry {
                    message: format!("Error {}", i),
                    timestamp: i as i64,
                    target: "test".to_string(),
                    count: 1,
                })
                .unwrap();
        }

        // Wait for background task to process
        nagoya::sleep(std::time::Duration::from_millis(100)).await;

        let errors = container.get_all_errors().await;
        assert_eq!(errors.len(), 3);
        // Oldest (Error 0) should be evicted, remaining sorted by most recent first
        assert_eq!(errors[0].message, "Error 3");
        assert_eq!(errors[0].count, 1);
        assert_eq!(errors[1].message, "Error 2");
        assert_eq!(errors[2].message, "Error 1");
    }

    #[test]
    fn test_normalized_mode_counting() {
        nagoya::block_on(test_normalized_mode_counting_body());
    }

    async fn test_normalized_mode_counting_body() {
        let config = ErrorAggregationConfig {
            limit: 10,
            normalize: true,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add similar errors with different IPs
        container
            .ingress
            .sender
            .try_send(ErrorEntry {
                message: "Connection failed to 192.168.1.1".to_string(),
                timestamp: 1000,
                target: "network".to_string(),
                count: 1,
            })
            .unwrap();

        container
            .ingress
            .sender
            .try_send(ErrorEntry {
                message: "Connection failed to 10.0.0.1".to_string(),
                timestamp: 2000,
                target: "network".to_string(),
                count: 1,
            })
            .unwrap();

        // Wait for background task
        nagoya::sleep(std::time::Duration::from_millis(100)).await;

        // Should be normalized to 1 entry
        assert_eq!(container.count().await, 1);

        let stats = container.get_stats(10, 0).await;
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].count, 2);
        assert_eq!(stats[0].last_seen, 2000);
        assert_eq!(stats[0].first_seen, 1000);
    }

    #[test]
    fn test_pagination() {
        nagoya::block_on(test_pagination_body());
    }

    async fn test_pagination_body() {
        let config = ErrorAggregationConfig {
            limit: 100,
            normalize: false,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add 50 errors
        for i in 0..50 {
            container
                .ingress
                .sender
                .try_send(ErrorEntry {
                    message: format!("Error {}", i),
                    timestamp: i as i64,
                    target: "test".to_string(),
                    count: 1,
                })
                .unwrap();
        }

        // Wait for background task
        nagoya::sleep(std::time::Duration::from_millis(100)).await;

        let page1 = container.get_errors(10, 0, None).await;
        let page2 = container.get_errors(10, 10, None).await;

        assert_eq!(page1.len(), 10);
        assert_eq!(page2.len(), 10);
        // Sorted by most recent first, so Error 49 comes first
        assert_eq!(page1[0].message, "Error 49");
        assert_eq!(page1[9].message, "Error 40");
        assert_eq!(page2[0].message, "Error 39");
        assert_eq!(page2[9].message, "Error 30");
    }

    #[test]
    fn test_normalized_mode_eviction() {
        nagoya::block_on(test_normalized_mode_eviction_body());
    }

    async fn test_normalized_mode_eviction_body() {
        let config = ErrorAggregationConfig {
            limit: 2,
            normalize: true,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add 3 different error types
        container
            .ingress
            .sender
            .try_send(ErrorEntry {
                message: "Error type A".to_string(),
                timestamp: 1000,
                target: "test".to_string(),
                count: 1,
            })
            .unwrap();

        container
            .ingress
            .sender
            .try_send(ErrorEntry {
                message: "Error type B".to_string(),
                timestamp: 2000,
                target: "test".to_string(),
                count: 1,
            })
            .unwrap();

        container
            .ingress
            .sender
            .try_send(ErrorEntry {
                message: "Error type C".to_string(),
                timestamp: 3000,
                target: "test".to_string(),
                count: 1,
            })
            .unwrap();

        // Wait for background task
        nagoya::sleep(std::time::Duration::from_millis(100)).await;

        // Should have 2 entries (oldest by first_seen evicted)
        assert_eq!(container.count().await, 2);

        let stats = container.get_stats(10, 0).await;
        // Check that Error type A (oldest) is evicted
        assert!(!stats.iter().any(|s| s.message.contains("type A")));
        assert!(stats.iter().any(|s| s.message.contains("type B")));
        assert!(stats.iter().any(|s| s.message.contains("type C")));
    }

    #[test]
    fn test_clear() {
        nagoya::block_on(test_clear_body());
    }

    async fn test_clear_body() {
        let config = ErrorAggregationConfig {
            limit: 10,
            normalize: false,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add some errors
        for i in 0..5 {
            container
                .ingress
                .sender
                .try_send(ErrorEntry {
                    message: format!("Error {}", i),
                    timestamp: i as i64,
                    target: "test".to_string(),
                    count: 1,
                })
                .unwrap();
        }

        // Wait for background task
        nagoya::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(container.count().await, 5);

        // Clear all errors
        container.clear().await;

        assert_eq!(container.count().await, 0);
    }

    #[test]
    fn test_get_latest() {
        nagoya::block_on(test_get_latest_body());
    }

    async fn test_get_latest_body() {
        let config = ErrorAggregationConfig {
            limit: 100,
            normalize: false,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add 20 errors
        for i in 0..20 {
            container
                .ingress
                .sender
                .try_send(ErrorEntry {
                    message: format!("Error {}", i),
                    timestamp: i as i64,
                    target: "test".to_string(),
                    count: 1,
                })
                .unwrap();
        }

        // Wait for background task
        nagoya::sleep(std::time::Duration::from_millis(100)).await;

        let latest = container.get_latest(10).await;

        assert_eq!(latest.len(), 10);
        // Sorted by most recent first, so Error 19 comes first
        assert_eq!(latest[0].message, "Error 19");
        assert_eq!(latest[9].message, "Error 10");
    }
}
