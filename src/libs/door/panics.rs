//! One installed catcher and request-scoped panic sinks per thread.
//!
//! **Dead under `panic = "abort"`, kept only for PathDB.** Nothing in this tree installs a
//! catcher (`catching` therefore just runs its closure) and a panic ends the process before any
//! sink could be drained. `PathDB/crates/pathdb/src/server.rs` still calls `arm`/`catching`; once
//! it stops, delete this module (see ekopathrs/docs/NO-UNWINDING.md, open items).

use alloc::string::String;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::libs::door::protocol::Reply;
use crate::libs::door::sys::thread::ThreadLocal;

struct SinkScope {
    id: usize,
    notes: Vec<String>,
}

static NEXT_SCOPE: AtomicUsize = AtomicUsize::new(1);
static SINK: ThreadLocal<Vec<SinkScope>> = ThreadLocal::new();

pub type Catcher = fn(&mut dyn FnMut() -> Reply) -> Result<Reply, String>;

static CATCHER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

pub fn set_catcher(catcher: Catcher) {
    CATCHER.store(catcher as usize, core::sync::atomic::Ordering::Release);
}

pub fn catching(f: &mut dyn FnMut() -> Reply) -> Result<Reply, String> {
    let raw = CATCHER.load(core::sync::atomic::Ordering::Acquire);
    if raw == 0 {
        return Ok(f());
    }
    // Safety: only `set_catcher` writes this value, from a `Catcher`.
    let catcher: Catcher = unsafe { core::mem::transmute::<usize, Catcher>(raw) };
    catcher(f)
}

pub fn claim(at: &str, what: &str) -> bool {
    SINK.with(
        Vec::new,
        |scopes| match scopes.last_mut() {
            Some(scope) => {
                scope.notes.push(alloc::format!("panicked at {at}: {what}"));
                true
            }
            None => false,
        },
    ).unwrap_or(false)
}

/// The outcome of an analysis or transform reported through a request panic sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactDisposition {
    Applied,
    Refused,
    Unavailable,
}

/// Retain an analysis fact with its exact outcome in the active request's panic report.
///
/// This does not claim that an unavailable analysis succeeded, or that a refusal is an error
/// in the user's program. It records the producer's disposition verbatim for the request owner.
pub fn claim_fact(
    at: &str,
    fact: &str,
    disposition: FactDisposition,
    detail: &str,
) -> bool {
    let state = match disposition {
        FactDisposition::Applied => "applied",
        FactDisposition::Refused => "refused",
        FactDisposition::Unavailable => "unavailable",
    };
    claim(at, &alloc::format!("{fact}: {state}: {detail}"))
}

pub fn arm() -> Armed {
    let id = NEXT_SCOPE.fetch_add(1, Ordering::Relaxed);
    let _ = SINK.with(Vec::new, |scopes| {
        scopes.push(SinkScope { id, notes: Vec::new() });
    });
    Armed { id, _thread_bound: PhantomData }
}

pub struct Armed {
    id: usize,
    // A scope belongs to the thread whose panic runtime and sink it protects.
    _thread_bound: PhantomData<alloc::rc::Rc<()>>,
}

impl Armed {
    pub fn drain(&self) -> Vec<String> {
        SINK.with(Vec::new, |scopes| {
            scopes.iter_mut()
                .find(|scope| scope.id == self.id)
                .map(|scope| core::mem::take(&mut scope.notes))
                .unwrap_or_default()
        }).unwrap_or_default()
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        let id = self.id;
        let _ = SINK.with(Vec::new, |scopes| {
            if let Some(index) = scopes.iter().position(|scope| scope.id == id) {
                scopes.remove(index);
            }
        });
    }
}
