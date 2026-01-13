//! Deprecated logging utilities from previous implementations

use std::path::PathBuf;
use std::sync::Arc;

pub enum LoggingGuard {
    NonBlocking(tracing_appender::non_blocking::WorkerGuard, PathBuf),
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
pub fn can_create_file_in_directory(directory: &str) -> bool {
    let test_file_path: String = format!("{directory}/test_file.txt");
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
