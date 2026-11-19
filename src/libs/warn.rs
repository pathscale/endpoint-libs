use alloc::collections::BTreeMap as HashMap;
use alloc::string::{String, ToString};
use tracing::warn;

#[derive(Debug)]
pub struct WarnStats {
    pub warn_count: u64,
}
#[derive(Debug, Default)]
pub struct WarnManager {
    warns: HashMap<String, WarnStats>,
}

impl WarnManager {
    pub fn new() -> Self {
        Self {
            warns: HashMap::new(),
        }
    }

    pub fn warn(&mut self, s: impl AsRef<str>) {
        let s = s.as_ref();

        if let Some(stats) = self.warns.get_mut(s) {
            stats.warn_count += 1;
        } else {
            warn!("First warning: {}", s);
            self.warns
                .insert(s.to_string(), WarnStats { warn_count: 1 });
        }
    }
    pub fn get_warns(&self) -> &HashMap<String, WarnStats> {
        &self.warns
    }

    pub fn dump_stats(&self, mut file: impl core::fmt::Write) -> core::fmt::Result {
        let millis = crate::libs::utils::get_time_milliseconds();
        if let Some(dt) = chrono::DateTime::from_timestamp_millis(millis) {
            writeln!(file, "Warnings at {dt}:")?;
        } else {
            writeln!(file, "Warnings at {millis}:")?;
        }
        for (msg, stats) in &self.warns {
            writeln!(file, "count={} warn={}", stats.warn_count, msg)?;
        }
        Ok(())
    }
}
