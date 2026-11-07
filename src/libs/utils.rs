use std::sync::atomic::{AtomicU32, Ordering};

use eyre::Result;

pub fn get_log_id() -> u64 {
    chrono::Utc::now().timestamp_micros() as _
}

/// Allocates a connection ID unique within this process.
///
/// IDs start at 1 on each process start.
///
/// # Panics
///
/// Panics once all nonzero `u32` IDs have been issued in this process.
pub fn get_conn_id() -> u32 {
    try_get_conn_id().expect("connection ID space exhausted")
}

/// Returns `None` once all nonzero `u32` connection IDs have been issued.
pub(crate) fn try_get_conn_id() -> Option<u32> {
    static LAST: AtomicU32 = AtomicU32::new(0);
    next_conn_id(&LAST)
}

fn next_conn_id(last: &AtomicU32) -> Option<u32> {
    last.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_add(1)
    })
    .ok()
    .and_then(|previous| previous.checked_add(1))
}

#[cfg(test)]
mod conn_id_tests {
    use std::sync::Barrier;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn ids_from_many_threads_never_repeat() {
        let ids: Vec<u32> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        (0..10_000)
                            .map(|_| super::get_conn_id())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect()
        });
        let unique: std::collections::HashSet<u32> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn concurrent_allocation_exhausts_without_wrapping() {
        let last = AtomicU32::new(u32::MAX - 1);
        let barrier = Barrier::new(3);
        let results = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    let barrier = &barrier;
                    let last = &last;
                    scope.spawn(move || {
                        barrier.wait();
                        super::next_conn_id(last)
                    })
                })
                .collect();
            barrier.wait();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert_eq!(
            results.iter().filter(|id| **id == Some(u32::MAX)).count(),
            1
        );
        assert_eq!(results.iter().filter(|id| id.is_none()).count(), 1);
        assert_eq!(super::next_conn_id(&last), None);
        assert_eq!(super::next_conn_id(&last), None);
    }
}

pub fn get_time_milliseconds() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
pub fn get_time_micros() -> i64 {
    chrono::Utc::now().timestamp_micros()
}
pub fn hex_decode(s: &[u8]) -> Result<Vec<u8>> {
    if s.starts_with(b"0x") {
        Ok(hex::decode(&s[2..])?)
    } else {
        Ok(hex::decode(s)?)
    }
}

/// Aligns the precision of one `f64` value to match another `f64` value.
pub fn align_precision(a: f64, b: f64) -> f64 {
    let precision_b = count_dp(b);
    let precision_a = format!("{:.0$}", { precision_b });
    let aligned_a = format!("{:.*}", precision_a.parse::<usize>().unwrap(), a);
    aligned_a.parse().unwrap()
}

pub fn count_dp(num: f64) -> usize {
    // Convert the f64 to a string representation
    let num_str = format!("{num}");

    // Find the position of the decimal point
    if let Some(decimal_index) = num_str.find('.') {
        // Count the number of characters after the decimal point
        num_str.len() - decimal_index - 1
    } else {
        // If there is no decimal point, return 0
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_dp() {
        assert_eq!(count_dp(1.5), 1);
        assert_eq!(count_dp(1.1), 1);
    }

    #[test]
    fn test_align_precision() {
        let a = 123.456789;
        let b = 78.9;
        let aligned_a = align_precision(a, b);
        assert_eq!(aligned_a, 123.5);

        let a = 2.37;
        let b = 631.3;
        let aligned_a = align_precision(a, b);
        assert_eq!(aligned_a, 2.4);
    }
}
