//! Deprecated logging utilities from previous implementations

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

#[cfg(feature = "std")]
pub use std::path::PathBuf;
#[cfg(not(feature = "std"))]
pub use eko::path::PathBuf;

#[cfg(feature = "std")]
pub type WorkerGuard = tracing_appender::non_blocking::WorkerGuard;

#[cfg(not(feature = "std"))]
#[derive(Debug, Clone, Default)]
pub struct WorkerGuard;

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

/// actually test writing, there is no direct way to check if the application has the ownership or the write access
// Deprecated compatibility helper: this creates and removes an empty probe file
// to report directory writability. It never writes configuration or credentials.
#[allow(clippy::disallowed_methods)]
pub fn can_create_file_in_directory(directory: &str) -> bool {
    let test_file_path: String = format!("{directory}/test_file.txt");
    #[cfg(feature = "std")]
    {
        match std::fs::File::create(&test_file_path) {
            Ok(file) => {
                // File created successfully; remove it after checking
                drop(file);
                if let Err(err) = std::fs::remove_file(&test_file_path) {
                    eprintln!("Error deleting test file: {err}");
                }
                true
            }
            Err(_) => false,
        }
    }
    #[cfg(not(feature = "std"))]
    {
        match eko::file::File::create(test_file_path.as_str()) {
            Ok(file) => {
                drop(file);
                eko::fs::unlink(test_file_path.as_bytes());
                true
            }
            Err(_) => false,
        }
    }
}
