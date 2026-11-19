//! Deprecated logging utilities from previous implementations

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

pub use eko::path::PathBuf;

pub use super::WorkerGuard;

pub enum LoggingGuard {
    NonBlocking(WorkerGuard, PathBuf),
    StdoutWithPath(Option<PathBuf>),
}
impl LoggingGuard {
    pub fn get_file(&self) -> Option<PathBuf> {
        match self {
            LoggingGuard::NonBlocking(_guard, path) => Some(path.clone()),
            LoggingGuard::StdoutWithPath(path) => path.clone(),
        }
    }
}

#[derive(Clone)]
pub struct DynLogger {
    logger: Arc<dyn Fn(&str) + Send + Sync>,
}
impl DynLogger {
    pub fn new(logger: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        Self { logger }
    }
    pub fn empty() -> Self {
        Self {
            logger: Arc::new(|_| {}),
        }
    }
    pub fn log(&self, msg: impl AsRef<str>) {
        (self.logger)(msg.as_ref())
    }
}

/// Probe directory writability by creating and removing an empty file.
#[allow(clippy::disallowed_methods)]
pub fn can_create_file_in_directory(directory: &str) -> bool {
    let test_file_path: String = format!("{directory}/test_file.txt");
    match eko::file::File::create(test_file_path.as_str()) {
        Ok(file) => {
            drop(file);
            eko::fs::unlink(test_file_path.as_bytes());
            true
        }
        Err(_) => false,
    }
}
