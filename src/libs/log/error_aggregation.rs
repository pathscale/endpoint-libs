//! Error Aggregation - Collection of recent errors with optional deduping and pagination.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cmp::{Ordering, Reverse};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use itertools::Itertools;

pub use super::LogRotation;

const PENDING_CAPACITY: usize = 256;
const MAX_EVENT_BYTES: usize = 8 * 1024;

/// Create error aggregation container and layer.
pub fn get_error_aggregation(
    error_aggregation: ErrorAggregationConfig,
) -> (Arc<ErrorAggregationContainer>, ErrorAggregationLayer) {
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

/// Key for deduplication map
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
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
    storage: BTreeMap<ErrorKey, ErrorStats>,
}

impl ErrorStorage {
    pub fn new() -> Self {
        Self {
            storage: BTreeMap::new(),
        }
    }

    pub fn get_map(&self) -> &BTreeMap<ErrorKey, ErrorStats> {
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
        let mut current = self.dropped.load(AtomicOrdering::Relaxed);
        loop {
            let next = current.saturating_add(1);
            match self.dropped.compare_exchange_weak(
                current,
                next,
                AtomicOrdering::Relaxed,
                AtomicOrdering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
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
    pub fn dropped_events(&self) -> usize {
        self.ingress.dropped.load(AtomicOrdering::Relaxed)
    }

    /// Get errors with pagination support and optional custom sorting. Sorts by most-recent by default
    pub async fn get_errors(
        &self,
        limit: usize,
        offset: usize,
        sort_by: Option<ErrorSortingFn>,
    ) -> Vec<ErrorEntry> {
        let storage = self.storage.read().await;
        let map = storage.get_map();

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
            entries.sort_by_key(|e| Reverse(e.timestamp));
        }

        entries.into_iter().skip(offset).take(limit).collect()
    }

    /// Get error statistics with pagination
    pub async fn get_stats(&self, limit: usize, offset: usize) -> Vec<ErrorStats> {
        let storage = self.storage.read().await;
        let map = storage.get_map();

        let mut stats: Vec<ErrorStats> = map.values().cloned().collect();
        stats.sort_by_key(|s| Reverse(s.last_seen));
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

        let key = ErrorKey {
            target: entry.target.clone(),
            normalized_message: if config.normalize {
                normalize_message(&entry.message)
            } else {
                entry.message.clone()
            },
        };

        if let Some(stats) = map.get_mut(&key) {
            stats.count += 1;
            stats.last_seen = entry.timestamp;
            stats.message = entry.message;
        } else {
            if map.len() >= config.limit {
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

// --- Pure no_std message normalization (exact parity with the 7 regex passes) ---

fn is_word_char_at(s: &str, byte_idx: usize) -> bool {
    s[byte_idx..]
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

fn is_prev_word_char(s: &str, byte_idx: usize) -> bool {
    byte_idx > 0
        && s[..byte_idx]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

fn is_word_boundary_start(s: &str, start: usize) -> bool {
    !is_prev_word_char(s, start)
}

fn is_word_boundary_end(s: &str, end: usize) -> bool {
    end >= s.len() || !is_word_char_at(s, end)
}

fn replace_pattern(
    input: &str,
    replacement: &str,
    matcher: impl Fn(&str, usize) -> Option<usize>,
) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        if let Some(end) = matcher(input, idx) {
            if end > idx {
                out.push_str(replacement);
                idx = end;
                continue;
            }
        }
        let ch = input[idx..].chars().next().unwrap();
        out.push(ch);
        idx += ch.len_utf8();
    }
    out
}

fn digits_at(bytes: &[u8], pos: usize, count: usize) -> bool {
    bytes
        .get(pos..pos + count)
        .is_some_and(|slice| slice.iter().all(u8::is_ascii_digit))
}

fn hex_at(bytes: &[u8], pos: usize, count: usize) -> bool {
    bytes
        .get(pos..pos + count)
        .is_some_and(|slice| slice.iter().all(u8::is_ascii_hexdigit))
}

/// `\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?`
fn match_timestamp(s: &str, start: usize) -> Option<usize> {
    let b = s.as_bytes();
    if b.len().saturating_sub(start) < 19 {
        return None;
    }
    if !digits_at(b, start, 4)
        || b.get(start + 4) != Some(&b'-')
        || !digits_at(b, start + 5, 2)
        || b.get(start + 7) != Some(&b'-')
        || !digits_at(b, start + 8, 2)
        || !matches!(b.get(start + 10), Some(b'T' | b' '))
        || !digits_at(b, start + 11, 2)
        || b.get(start + 13) != Some(&b':')
        || !digits_at(b, start + 14, 2)
        || b.get(start + 16) != Some(&b':')
        || !digits_at(b, start + 17, 2)
    {
        return None;
    }
    let mut i = start + 19;
    if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 2;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    if b.get(i) == Some(&b'Z') {
        i += 1;
    } else if matches!(b.get(i), Some(b'+' | b'-'))
        && digits_at(b, i + 1, 2)
        && b.get(i + 3) == Some(&b':')
        && digits_at(b, i + 4, 2)
    {
        i += 6;
    }
    Some(i)
}

/// `\b[0-9a-fA-F]{8}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{4}-?[0-9a-fA-F]{12}\b`
fn match_uuid(s: &str, start: usize) -> Option<usize> {
    if !is_word_boundary_start(s, start) {
        return None;
    }
    let b = s.as_bytes();
    let mut i = start;
    for (seg_idx, seg_len) in [8usize, 4, 4, 4, 12].into_iter().enumerate() {
        if seg_idx > 0 && b.get(i) == Some(&b'-') {
            i += 1;
        }
        if !hex_at(b, i, seg_len) {
            return None;
        }
        i += seg_len;
    }
    if is_word_boundary_end(s, i) {
        Some(i)
    } else {
        None
    }
}

/// `\b(?:\d{1,3}\.){3}\d{1,3}\b`
fn match_ipv4(s: &str, start: usize) -> Option<usize> {
    if !is_word_boundary_start(s, start) {
        return None;
    }
    let b = s.as_bytes();
    let mut i = start;
    for octet in 0..4 {
        if octet > 0 {
            if b.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
        let mut digits = 0;
        while digits < 3 && b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
    }
    if is_word_boundary_end(s, i) {
        Some(i)
    } else {
        None
    }
}

/// `(?:/[\w\-./]+)|(?:[A-Z]:\\[\w\-\\./]+)`
fn match_path(s: &str, start: usize) -> Option<usize> {
    let b = s.as_bytes();
    if b.get(start) == Some(&b'/') {
        let mut i = start + 1;
        let mut count = 0;
        while i < s.len() {
            let ch = s[i..].chars().next().unwrap();
            if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/') {
                i += ch.len_utf8();
                count += 1;
            } else {
                break;
            }
        }
        if count > 0 {
            return Some(i);
        }
    } else if b.get(start).is_some_and(u8::is_ascii_uppercase)
        && b.get(start + 1) == Some(&b':')
        && b.get(start + 2) == Some(&b'\\')
    {
        let mut i = start + 3;
        let mut count = 0;
        while i < s.len() {
            let ch = s[i..].chars().next().unwrap();
            if ch.is_alphanumeric() || matches!(ch, '_' | '-' | '\\' | '.' | '/') {
                i += ch.len_utf8();
                count += 1;
            } else {
                break;
            }
        }
        if count > 0 {
            return Some(i);
        }
    }
    None
}

/// `\b0x[0-9a-fA-F]+\b`
fn match_hex(s: &str, start: usize) -> Option<usize> {
    if !is_word_boundary_start(s, start) {
        return None;
    }
    let b = s.as_bytes();
    if b.get(start..start + 2) != Some(b"0x") {
        return None;
    }
    let mut i = start + 2;
    while b.get(i).is_some_and(u8::is_ascii_hexdigit) {
        i += 1;
    }
    if i > start + 2 && is_word_boundary_end(s, i) {
        Some(i)
    } else {
        None
    }
}

/// `\b\d+\.?\d*\b`
fn match_number(s: &str, start: usize) -> Option<usize> {
    if !is_word_boundary_start(s, start) {
        return None;
    }
    let b = s.as_bytes();
    if !b.get(start).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let mut i = start + 1;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let int_end = i;
    if b.get(i) == Some(&b'.') {
        let mut frac = i + 1;
        while b.get(frac).is_some_and(u8::is_ascii_digit) {
            frac += 1;
        }
        if is_word_boundary_end(s, frac) {
            return Some(frac);
        }
    }
    if is_word_boundary_end(s, int_end) {
        Some(int_end)
    } else {
        None
    }
}

/// Message normalization for deduplication
fn normalize_message(message: &str) -> String {
    let mut normalized = replace_pattern(message, "<TIMESTAMP>", match_timestamp);
    normalized = replace_pattern(&normalized, "<UUID>", match_uuid);
    normalized = replace_pattern(&normalized, "<IP>", match_ipv4);
    normalized = replace_pattern(&normalized, "<PATH>", match_path);
    normalized = replace_pattern(&normalized, "<HEX>", match_hex);
    normalized = replace_pattern(&normalized, "<NUM>", match_number);

    let mut collapsed = String::with_capacity(normalized.len());
    let mut in_ws = false;
    for ch in normalized.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                collapsed.push(' ');
                in_ws = true;
            }
        } else {
            collapsed.push(ch);
            in_ws = false;
        }
    }
    collapsed.trim().to_string()
}

/// Tracing event sink that captures ERROR level events and sends them to the aggregator.
pub struct ErrorAggregationLayer {
    ingress: Weak<ErrorIngress>,
}

impl ErrorAggregationLayer {
    /// Record an event into the error aggregator if it is `ERROR` level.
    pub fn record_event(&self, event: &tracing::Event<'_>) {
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

    let timestamp = crate::libs::utils::get_time_milliseconds();
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
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" && write!(self.0, "{value:?}").is_err() {
            self.0.failed = true;
        }
    }
}

#[cfg(all(test, feature = "error_aggregation"))]
mod tests {
    use super::*;
    use alloc::format;
    use core::sync::atomic::AtomicU64;
    use core::time::Duration;
    use tracing::span;

    struct TestSubscriber {
        layer: ErrorAggregationLayer,
        observed: Arc<AtomicUsize>,
        next_id: AtomicU64,
    }

    impl tracing::Subscriber for TestSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(self.next_id.fetch_add(1, AtomicOrdering::Relaxed))
        }

        fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

        fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            self.layer.record_event(event);
            if event.metadata().level() == &tracing::Level::ERROR {
                self.observed.fetch_add(1, AtomicOrdering::Relaxed);
            }
        }

        fn enter(&self, _span: &span::Id) {}

        fn exit(&self, _span: &span::Id) {}
    }

    #[test]
    fn aggregation_ingress_saturation_recovers_and_closes() {
        nagoya::block_on(nagoya::timeout(Duration::from_secs(10), async {
            let (container, layer) = get_error_aggregation(ErrorAggregationConfig::default());
            let observed = Arc::new(AtomicUsize::new(0));
            let subscriber: Arc<dyn tracing::Subscriber + Send + Sync> = Arc::new(TestSubscriber {
                layer,
                observed: Arc::clone(&observed),
                next_id: AtomicU64::new(1),
            });
            let emit_queued = || {
                crate::libs::log::with_test_subscriber(Arc::clone(&subscriber), || {
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
            crate::libs::log::with_test_subscriber(Arc::clone(&subscriber), || {
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

            crate::libs::log::with_test_subscriber(Arc::clone(&subscriber), || {
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
        assert_eq!(
            normalize_message("Failed to connect to 192.168.1.1"),
            "Failed to connect to <IP>"
        );

        assert_eq!(
            normalize_message("User 550e8400-e29b-41d4-a716-446655440000 not found"),
            "User <UUID> not found"
        );

        assert_eq!(normalize_message("Error on line 42"), "Error on line <NUM>");

        assert_eq!(
            normalize_message("Failed to read /var/log/app.log"),
            "Failed to read <PATH>"
        );

        assert_eq!(
            normalize_message("Memory address 0xdeadbeef"),
            "Memory address <HEX>"
        );

        assert_eq!(
            normalize_message("Connection to 10.0.0.1:8080 failed at /home/user/file.txt"),
            "Connection to <IP>:<NUM> failed at <PATH>"
        );
    }

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

        nagoya::sleep(Duration::from_millis(100)).await;

        let errors = container.get_all_errors().await;
        assert_eq!(errors.len(), 3);
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

        nagoya::sleep(Duration::from_millis(100)).await;

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

        nagoya::sleep(Duration::from_millis(100)).await;

        let page1 = container.get_errors(10, 0, None).await;
        let page2 = container.get_errors(10, 10, None).await;

        assert_eq!(page1.len(), 10);
        assert_eq!(page2.len(), 10);
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

        nagoya::sleep(Duration::from_millis(100)).await;

        assert_eq!(container.count().await, 2);

        let stats = container.get_stats(10, 0).await;
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

        nagoya::sleep(Duration::from_millis(100)).await;
        assert_eq!(container.count().await, 5);

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

        for i in 0..10 {
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

        nagoya::sleep(Duration::from_millis(100)).await;

        let latest = container.get_latest(3).await;
        assert_eq!(latest.len(), 3);
        assert_eq!(latest[0].message, "Error 9");
        assert_eq!(latest[1].message, "Error 8");
        assert_eq!(latest[2].message, "Error 7");
    }
}
