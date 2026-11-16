//! Bounded admission, priority ordering and cooperative cancellation.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diagnostics {
    pub generation: u64,
    pub submitted: u64,
    pub started: u64,
    pub cancelled: u64,
    pub completed: u64,
    pub replies_retained: u64,
    pub available: bool,
    pub complete: bool,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            generation: 1,
            submitted: 0,
            started: 0,
            cancelled: 0,
            completed: 0,
            replies_retained: 0,
            available: true,
            complete: true,
        }
    }
}

use crate::libs::door::sys::thread::{Condvar, Mutex};

pub const MAX_QUEUED_REQUEST_BYTES: usize = 1_048_576;
pub const MAX_QUEUE_OWNER_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Queued,
    Running,
}

impl State {
    pub fn spelling(&self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub owner: Option<String>,
    pub priority: i32,
    pub request: String,
    pub state: State,
    pub submitted: u64,
    pub started: Option<u64>,
    pub polled: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Width {
    pub concurrent: usize,
    pub queued: usize,
}

impl Default for Width {
    fn default() -> Width {
        Width { concurrent: 1, queued: 64 }
    }
}

#[derive(Debug, Default)]
pub struct Queue {
    inner: Mutex<Inner>,
    turn: Condvar,
}

#[derive(Debug)]
struct Inner {
    next_id: u64,
    width: Width,
    jobs: BTreeMap<u64, Job>,
    diagnostics: Diagnostics,
    cancellations: BTreeMap<u64, Arc<Cancellation>>,
}

impl Default for Inner {
    fn default() -> Inner {
        Inner {
            next_id: 1,
            width: Width::default(),
            jobs: BTreeMap::new(),
            diagnostics: Diagnostics::default(),
            cancellations: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct Ticket<'a> {
    queue: &'a Queue,
    pub id: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Running<'a> {
    queue: Option<&'a Queue>,
    id: u64,
}

/// A retained cancellation subscription. Cancellation wakes waiters without polling the queue.
#[derive(Debug, Default)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
    gate: Mutex<()>,
    changed: Condvar,
}

impl Cancellation {
    /// Share directly with cooperative emitter budgets; the queue publishes with Release.
    pub fn flag(&self) -> Arc<AtomicBool> { Arc::clone(&self.cancelled) }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Wait until this job is cancelled or its ticket is released.
    pub fn wait(&self) {
        let mut guard = self.gate.lock();
        while !self.is_cancelled() {
            guard = self.changed.wait(guard);
        }
    }

    fn cancel(&self) {
        let guard = self.gate.lock();
        self.cancelled.store(true, Ordering::Release);
        drop(guard);
        self.changed.notify_all();
    }
}

impl<'a> Running<'a> {
    pub fn in_queue(queue: &'a Queue, id: u64) -> Running<'a> {
        Running { queue: Some(queue), id }
    }

    pub fn none() -> Running<'static> {
        Running { queue: None, id: 0 }
    }

    pub fn stop_requested(&self) -> bool {
        match self.queue {
            None => false,
            Some(queue) => queue.stop_requested(self.id),
        }
    }

    /// Subscribe to the same job state used by the control socket's cancel operation.
    pub fn subscribe_cancellation(&self) -> Option<Arc<Cancellation>> {
        self.queue.map(|queue| queue.subscribe_cancellation(self.id))
    }

    pub(crate) fn queue_identity(&self, about: Vec<(String, String)>) -> Vec<(String, String)> {
        match self.queue {
            Some(queue) => crate::libs::door::server::queue_identity(queue, about),
            None => about,
        }
    }
}

impl Queue {
    pub fn new() -> Queue {
        Queue::default()
    }

    pub fn submit(
        &self,
        request: &str,
        owner: Option<String>,
        priority: i32,
    ) -> Result<Ticket<'_>, String> {
        if request.len() > MAX_QUEUED_REQUEST_BYTES {
            return Err(format!(
                "crates/door/src/queue.rs: request exceeds {MAX_QUEUED_REQUEST_BYTES} bytes"
            ));
        }
        if owner.as_ref().is_some_and(|value| value.len() > MAX_QUEUE_OWNER_BYTES) {
            return Err(format!(
                "crates/door/src/queue.rs: owner exceeds {MAX_QUEUE_OWNER_BYTES} bytes"
            ));
        }

        let mut inner = self.lock();
        // Retained cancelled tickets still own payloads and consume admission capacity.
        let waiting = inner.jobs.values().filter(|j| j.state == State::Queued).count();
        if waiting >= inner.width.queued {
            return Err(format!(
                "the queue is full: {waiting} retained queued, {} running, depth {}. \
                 `list` says whose they are; `set cpu queued=N` raises it",
                inner.jobs.values().filter(|j| j.state == State::Running).count(),
                inner.width.queued
            ));
        }
        let id = inner.next_id;
        let next_id = id.checked_add(1)
            .ok_or_else(|| "queue.rs: ticket id space exhausted".to_string())?;
        if !inner.record(Event::Submitted) {
            return Err("queue.rs: lifecycle diagnostics exhausted; submission refused".into());
        }
        inner.next_id = next_id;
        inner.jobs.insert(
            id,
            Job {
                id,
                owner,
                priority,
                request: request.to_string(),
                state: State::Queued,
                submitted: eko::clock::now_ns(),
                started: None,
                polled: false,
                cancelled: false,
            },
        );
        inner.cancellations.insert(id, Arc::new(Cancellation::default()));
        Ok(Ticket { queue: self, id })
    }

    pub fn wait_for_turn(&self, id: u64) -> bool {
        let mut inner = self.lock();
        loop {
            match inner.jobs.get(&id) {
                None => return false,
                Some(job) if job.cancelled => return false,
                Some(job) if job.state == State::Running => return true,
                Some(_) => {}
            }
            if inner.next_up() == Some(id) {
                let running = inner.jobs.values().filter(|j| j.state == State::Running).count();
                if running < inner.width.concurrent {
                    if !inner.record(Event::Started) {
                        return false;
                    }
                    if let Some(job) = inner.jobs.get_mut(&id) {
                        job.state = State::Running;
                        job.started = Some(eko::clock::now_ns());
                    }
                    return true;
                }
            }
            inner = self.turn.wait(inner);
        }
    }

    pub fn stop_requested(&self, id: u64) -> bool {
        let mut inner = self.lock();
        match inner.jobs.get_mut(&id) {
            None => false,
            Some(job) => {
                job.polled = true;
                job.cancelled
            }
        }
    }

    pub fn cancel(&self, id: u64) -> Result<String, String> {
        let mut inner = self.lock();
        let Some(job) = inner.jobs.get_mut(&id) else {
            return Err(format!("no job {id}; `list` says what there is"));
        };
        let newly_cancelled = !job.cancelled;
        job.cancelled = true;
        let whose = match &job.owner {
            Some(owner) => format!(" (owned by {owner})"),
            None => String::new(),
        };
        let said = match (job.state, job.polled) {
            (State::Queued, _) => format!("job {id}{whose} was queued and will not start"),
            (State::Running, true) => {
                format!("asked job {id}{whose} to stop at its next check")
            }
            (State::Running, false) => format!(
                "asked job {id}{whose} to stop, but it has not checked for cancellation yet; \
                 it may have no point at which it can"
            ),
        };
        if newly_cancelled {
            inner.record(Event::Cancelled);
        }
        let subscription = inner.cancellations.get(&id).cloned();
        drop(inner);
        if let Some(subscription) = subscription {
            subscription.cancel();
        }
        self.turn.notify_all();
        Ok(said)
    }

    pub fn finish(&self, id: u64) {
        let mut inner = self.lock();
        if let Some(job) = inner.jobs.remove(&id) {
            if job.state == State::Running {
                inner.record(Event::Completed);
            }
        }
        let subscription = inner.cancellations.remove(&id);
        drop(inner);
        if let Some(subscription) = subscription {
            subscription.cancel();
        }
        self.turn.notify_all();
    }

    /// Registration and cancellation are serialized by the queue lock, so no wakeup is lost.
    pub fn subscribe_cancellation(&self, id: u64) -> Arc<Cancellation> {
        let mut inner = self.lock();
        if let Some(job) = inner.jobs.get_mut(&id) {
            job.polled = true;
        }
        if let Some(subscription) = inner.cancellations.get(&id) {
            return Arc::clone(subscription);
        }
        let subscription = Arc::new(Cancellation::default());
        subscription.cancel();
        subscription
    }

    /// A coherent snapshot of queue lifecycle accounting.
    pub fn diagnostics(&self) -> Diagnostics {
        self.lock().diagnostics
    }

    /// Record a reply after the dispatcher has retained its complete body.
    pub(crate) fn retain_reply(&self) {
        self.lock().record(Event::ReplyRetained);
    }

    pub fn jobs(&self) -> Vec<Job> {
        self.lock().jobs.values().cloned().collect()
    }

    pub fn width(&self) -> Width {
        self.lock().width
    }

    pub fn rule(&self, rule: &str) -> Result<(), String> {
        let mut inner = self.lock();
        if rule == "clear" {
            inner.width = Width::default();
            drop(inner);
            self.turn.notify_all();
            return Ok(());
        }
        let (name, value) = rule.split_once('=').ok_or_else(|| {
            format!("`{rule}` is not a cpu rule; try concurrent=N, queued=N or clear")
        })?;
        let n: usize = value.parse().map_err(|_| format!("`{value}` is not a number"))?;
        match name {
            "concurrent" if n == 0 => return Err("concurrent=0 would never run anything".into()),
            "concurrent" => inner.width.concurrent = n,
            "queued" => inner.width.queued = n,
            other => {
                return Err(format!("`{other}` is not a cpu rule; try concurrent=N or queued=N"))
            }
        }
        drop(inner);
        self.turn.notify_all();
        Ok(())
    }

    fn lock(&self) -> crate::libs::door::sys::thread::Guard<'_, Inner> {
        self.inner.lock()
    }
}

#[derive(Clone, Copy)]
enum Event {
    Submitted,
    Started,
    Cancelled,
    Completed,
    ReplyRetained,
}

impl Inner {
    fn record(&mut self, event: Event) -> bool {
        if !self.diagnostics.available {
            return false;
        }
        let generation = self.diagnostics.generation.checked_add(1);
        let count = match event {
            Event::Submitted => &mut self.diagnostics.submitted,
            Event::Started => &mut self.diagnostics.started,
            Event::Cancelled => &mut self.diagnostics.cancelled,
            Event::Completed => &mut self.diagnostics.completed,
            Event::ReplyRetained => &mut self.diagnostics.replies_retained,
        };
        let next_count = count.checked_add(1);
        match (generation, next_count) {
            (Some(generation), Some(next_count)) => {
                self.diagnostics.generation = generation;
                *count = next_count;
                true
            }
            _ => {
                self.diagnostics.available = false;
                self.diagnostics.complete = false;
                false
            }
        }
    }

    fn next_up(&self) -> Option<u64> {
        self.jobs.values()
            .filter(|j| j.state == State::Queued && !j.cancelled)
            .max_by_key(|j| (j.priority, core::cmp::Reverse(j.id)))
            .map(|j| j.id)
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        self.queue.finish(self.id);
    }
}
