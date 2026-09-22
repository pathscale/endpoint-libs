use eyre::*;
use futures::future::BoxFuture;
use std::future::Future;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

pub struct AdaptiveJob {
    duration: Arc<RwLock<Duration>>,
    task: Box<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
}

impl AdaptiveJob {
    pub fn new<F>(duration: Duration, task: F) -> Self
    where
        F: Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
    {
        Self {
            duration: Arc::new(RwLock::new(duration)),
            task: Box::new(task),
        }
    }
    pub fn set_duration(&self, duration: Duration) {
        *self.duration.write().unwrap() = duration;
    }
    pub fn get_trigger(&self) -> JobTrigger {
        JobTrigger {
            duration: self.duration.clone(),
        }
    }
    pub async fn run(self) {
        loop {
            let duration = *self.duration.read().unwrap();
            nagoya::sleep(duration).await;
            let task = (self.task)();
            // Detached on purpose, as under `tokio::spawn` before: a tick must not
            // wait on the previous tick's body, or a slow task silently becomes the
            // period. Dropping a nagoya `JoinHandle` detaches rather than cancels,
            // which is the same contract tokio's has.
            nagoya::runtime::background().spawn(task);
        }
    }
}
#[derive(Clone)]
pub struct JobTrigger {
    duration: Arc<RwLock<Duration>>,
}
impl JobTrigger {
    pub fn new(duration: Arc<RwLock<Duration>>) -> Self {
        Self { duration }
    }
    pub fn set_duration(&self, duration: Duration) {
        *self.duration.write().unwrap() = duration;
    }
}
/// A set of periodic jobs, started together by [`Scheduler::spawn`].
///
/// This used to hold a `tokio_cron_scheduler::JobScheduler` beside the adaptive
/// jobs. Nothing here ever built a cron expression: the single call was
/// `Job::new_repeated_async`, a fixed interval, which is exactly what
/// [`AdaptiveJob`] already does with one sleep and one spawn. The cron engine
/// was therefore an entire tokio-native crate, and a 500ms tick wheel, serving
/// a loop this module also owns. Both kinds of job now run the same way, so a
/// `Scheduler` is just the pending list.
pub struct Scheduler {
    pending_jobs: Vec<AdaptiveJob>,
}

impl Scheduler {
    /// Still `async`, and still infallible, though it no longer awaits: the
    /// constructor it wrapped was the cron engine's, and changing the signature
    /// would break every caller for nothing.
    pub async fn new() -> Self {
        Self {
            pending_jobs: vec![],
        }
    }
    /// Run `f` every `duration`, starting one `duration` after [`Scheduler::spawn`].
    ///
    /// Still `async` and still returning `Result` for source compatibility; the
    /// fallible step was registering with the cron engine, so this no longer has
    /// a way to fail.
    pub async fn add_job<F, Fut>(&mut self, duration: Duration, f: F) -> Result<()>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
    {
        self.pending_jobs.push(AdaptiveJob::new(duration, move || {
            let fut = f();
            Box::pin(async move {
                fut.await;
            })
        }));
        Ok(())
    }
    pub fn add_adaptive_job<F, Fut>(&mut self, duration: Duration, f: F) -> Result<JobTrigger>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
    {
        let job = AdaptiveJob::new(duration, move || {
            let fut = f();
            Box::pin(async move {
                fut.await;
            })
        });
        let trigger = job.get_trigger();
        self.pending_jobs.push(job);
        Ok(trigger)
    }
    /// Start every registered job. Still `async` for source compatibility; the
    /// await was the cron engine's `start`.
    pub async fn spawn(mut self) {
        for job in self.pending_jobs.drain(..) {
            nagoya::runtime::background().spawn(job.run());
        }
    }
}
