/// Error Aggregation - Collection of recent errors with optional deduping and other convenience stuff
use std::cmp::Ordering;
use std::sync::Arc;

use tracing_subscriber::Layer;

// Public re-export of Rotation so clients don't need to include tracing_appender just for log setup
pub use tracing_appender::rolling::Rotation as LogRotation;

use std::collections::HashMap;

use std::hash::Hash;

// Version with error aggregation feature
pub fn get_error_aggregation(
    error_aggregation: ErrorAggregationConfig,
) -> (Arc<ErrorAggregationContainer>, ErrorAggregationLayer) {
    // Create error aggregation container and layer
    let error_container = Arc::new(ErrorAggregationContainer::new(error_aggregation));
    let error_layer = ErrorAggregationLayer::new(error_container.sender.clone());

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

/// Container for aggregated errors with async query methods
#[derive(Debug)]
pub struct ErrorAggregationContainer {
    storage: Arc<tokio::sync::RwLock<ErrorStorage>>,
    sender: tokio::sync::mpsc::UnboundedSender<ErrorEntry>,
    task_handle: tokio::task::JoinHandle<()>,
    config: ErrorAggregationConfig,
}

pub type ErrorSortingFn = Box<dyn FnMut(&ErrorEntry, &ErrorEntry) -> Ordering + 'static>;

impl ErrorAggregationContainer {
    /// Create a new error aggregation container
    pub fn new(config: ErrorAggregationConfig) -> Self {
        let storage = ErrorStorage::new();
        let storage = Arc::new(tokio::sync::RwLock::new(storage));

        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();

        // Spawn background aggregation task
        let storage_clone = Arc::clone(&storage);
        let config_clone = config.clone();
        let task_handle = tokio::spawn(aggregation_task(receiver, storage_clone, config_clone));

        Self {
            storage,
            sender,
            task_handle,
            config,
        }
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
            entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        }

        entries.into_iter().skip(offset).take(limit).collect()
    }

    /// Get error statistics with pagination
    pub async fn get_stats(&self, limit: usize, offset: usize) -> Vec<ErrorStats> {
        let storage = self.storage.read().await;
        let map = storage.get_map();

        let mut stats: Vec<ErrorStats> = map.values().cloned().collect();
        stats.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
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
        self.task_handle.abort();
    }
}

/// Background task that aggregates errors from the channel
async fn aggregation_task(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<ErrorEntry>,
    storage: Arc<tokio::sync::RwLock<ErrorStorage>>,
    config: ErrorAggregationConfig,
) {
    while let Some(entry) = receiver.recv().await {
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
    sender: tokio::sync::mpsc::UnboundedSender<ErrorEntry>,
}

impl ErrorAggregationLayer {
    fn new(sender: tokio::sync::mpsc::UnboundedSender<ErrorEntry>) -> Self {
        Self { sender }
    }
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

        // Extract metadata
        let target = event.metadata().target().to_string();
        let timestamp = chrono::Utc::now().timestamp_millis();

        // Extract message from event
        let mut message = String::new();
        let mut visitor = MessageVisitor(&mut message);
        event.record(&mut visitor);

        // Send to aggregator (non-blocking since unbounded channel)
        let entry = ErrorEntry {
            message,
            timestamp,
            target,
            count: 1, // Will be recalculated by aggregation task
        };

        // Ignore send errors (channel closed means container dropped)
        let _ = self.sender.send(entry);
    }
}

/// Visitor to extract message field from tracing Event
struct MessageVisitor<'a>(&'a mut String);

impl<'a> tracing::field::Visit for MessageVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.0 = format!("{:?}", value);
            // Remove quotes added by Debug formatting
            if self.0.starts_with('"') && self.0.ends_with('"') && self.0.len() > 1 {
                *self.0 = self.0[1..self.0.len() - 1].to_string();
            }
        }
    }
}

#[cfg(all(test, feature = "error_aggregation"))]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn test_raw_mode_eviction() {
        let config = ErrorAggregationConfig {
            limit: 3,
            normalize: false,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add 4 entries, oldest should be evicted when limit exceeded
        for i in 0..4 {
            container
                .sender
                .send(ErrorEntry {
                    message: format!("Error {}", i),
                    timestamp: i as i64,
                    target: "test".to_string(),
                    count: 1,
                })
                .unwrap();
        }

        // Wait for background task to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let errors = container.get_all_errors().await;
        assert_eq!(errors.len(), 3);
        // Oldest (Error 0) should be evicted, remaining sorted by most recent first
        assert_eq!(errors[0].message, "Error 3");
        assert_eq!(errors[0].count, 1);
        assert_eq!(errors[1].message, "Error 2");
        assert_eq!(errors[2].message, "Error 1");
    }

    #[tokio::test]
    async fn test_normalized_mode_counting() {
        let config = ErrorAggregationConfig {
            limit: 10,
            normalize: true,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add similar errors with different IPs
        container
            .sender
            .send(ErrorEntry {
                message: "Connection failed to 192.168.1.1".to_string(),
                timestamp: 1000,
                target: "network".to_string(),
                count: 1,
            })
            .unwrap();

        container
            .sender
            .send(ErrorEntry {
                message: "Connection failed to 10.0.0.1".to_string(),
                timestamp: 2000,
                target: "network".to_string(),
                count: 1,
            })
            .unwrap();

        // Wait for background task
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Should be normalized to 1 entry
        assert_eq!(container.count().await, 1);

        let stats = container.get_stats(10, 0).await;
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].count, 2);
        assert_eq!(stats[0].last_seen, 2000);
        assert_eq!(stats[0].first_seen, 1000);
    }

    #[tokio::test]
    async fn test_pagination() {
        let config = ErrorAggregationConfig {
            limit: 100,
            normalize: false,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add 50 errors
        for i in 0..50 {
            container
                .sender
                .send(ErrorEntry {
                    message: format!("Error {}", i),
                    timestamp: i as i64,
                    target: "test".to_string(),
                    count: 1,
                })
                .unwrap();
        }

        // Wait for background task
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

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

    #[tokio::test]
    async fn test_normalized_mode_eviction() {
        let config = ErrorAggregationConfig {
            limit: 2,
            normalize: true,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add 3 different error types
        container
            .sender
            .send(ErrorEntry {
                message: "Error type A".to_string(),
                timestamp: 1000,
                target: "test".to_string(),
                count: 1,
            })
            .unwrap();

        container
            .sender
            .send(ErrorEntry {
                message: "Error type B".to_string(),
                timestamp: 2000,
                target: "test".to_string(),
                count: 1,
            })
            .unwrap();

        container
            .sender
            .send(ErrorEntry {
                message: "Error type C".to_string(),
                timestamp: 3000,
                target: "test".to_string(),
                count: 1,
            })
            .unwrap();

        // Wait for background task
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Should have 2 entries (oldest by first_seen evicted)
        assert_eq!(container.count().await, 2);

        let stats = container.get_stats(10, 0).await;
        // Check that Error type A (oldest) is evicted
        assert!(!stats.iter().any(|s| s.message.contains("type A")));
        assert!(stats.iter().any(|s| s.message.contains("type B")));
        assert!(stats.iter().any(|s| s.message.contains("type C")));
    }

    #[tokio::test]
    async fn test_clear() {
        let config = ErrorAggregationConfig {
            limit: 10,
            normalize: false,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add some errors
        for i in 0..5 {
            container
                .sender
                .send(ErrorEntry {
                    message: format!("Error {}", i),
                    timestamp: i as i64,
                    target: "test".to_string(),
                    count: 1,
                })
                .unwrap();
        }

        // Wait for background task
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        assert_eq!(container.count().await, 5);

        // Clear all errors
        container.clear().await;

        assert_eq!(container.count().await, 0);
    }

    #[tokio::test]
    async fn test_get_latest() {
        let config = ErrorAggregationConfig {
            limit: 100,
            normalize: false,
        };
        let container = Arc::new(ErrorAggregationContainer::new(config));

        // Add 20 errors
        for i in 0..20 {
            container
                .sender
                .send(ErrorEntry {
                    message: format!("Error {}", i),
                    timestamp: i as i64,
                    target: "test".to_string(),
                    count: 1,
                })
                .unwrap();
        }

        // Wait for background task
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let latest = container.get_latest(10).await;

        assert_eq!(latest.len(), 10);
        // Sorted by most recent first, so Error 19 comes first
        assert_eq!(latest[0].message, "Error 19");
        assert_eq!(latest[9].message, "Error 10");
    }
}
