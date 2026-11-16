//! Generic resident dispatch, counted sessions, MCP sessions and a control endpoint.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use eko::libc_lite as libc;

use crate::libs::door::protocol::Reply;
use crate::libs::door::queue::{Queue, Running, State};
use crate::libs::door::registry::{AnswerMode, Registry, Route};
use crate::libs::door::sys::socket::{Listener, Stream};
use crate::libs::door::sys::thread::Mutex;
use crate::libs::door::typed::{Asked, Shape};

pub use crate::libs::door::queue;

pub trait Handler: Send + 'static {
    type Registry: Registry;

    fn registry(&self) -> Arc<Self::Registry>;

    /// LegacyEnvelope handlers return the legacy body for either shape. CanonicalDocument
    /// handlers select the canonical writer using `shape` and return its complete document.
    fn handle(
        &mut self,
        request: <Self::Registry as Registry>::Request,
        shape: Shape,
        running: &Running<'_>,
    ) -> Reply;

    fn identity(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn idle(&mut self) -> Option<Duration> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct Dispatched {
    pub reply: Reply,
    pub shutdown: bool,
}

fn finish<R: Registry>(
    registry: &R,
    asked: &Asked<R::Request>,
    reply: Reply,
    about: &[(String, String)],
) -> Reply {
    match (asked.shape, registry.verb(&asked.request).answer_mode) {
        (Shape::Json, AnswerMode::LegacyEnvelope) => {
            crate::libs::door::typed::envelope(&asked.request.to_string(), reply, about)
        }
        _ => reply,
    }
}

fn admission<R: Registry>(registry: &R, asked: &Asked<R::Request>) -> Result<(), String> {
    if asked.shape == Shape::Json {
        crate::libs::door::registry::admitted(registry, &asked.request)
    } else {
        Ok(())
    }
}

pub(crate) fn queue_identity(queue: &Queue, mut about: Vec<(String, String)>) -> Vec<(String, String)> {
    let snapshot = queue.diagnostics();
    about.extend([
        ("queue_generation".into(), snapshot.generation.to_string()),
        ("queue_submitted".into(), snapshot.submitted.to_string()),
        ("queue_started".into(), snapshot.started.to_string()),
        ("queue_cancelled".into(), snapshot.cancelled.to_string()),
        ("queue_completed".into(), snapshot.completed.to_string()),
        ("queue_replies_retained".into(), snapshot.replies_retained.to_string()),
        ("queue_diagnostics_available".into(), snapshot.available.to_string()),
        ("queue_diagnostics_complete".into(), snapshot.complete.to_string()),
    ]);
    about
}

fn shared_identity<H: Handler>(handler: &Mutex<H>, queue: &Queue) -> Vec<(String, String)> {
    // Cancellation and liveness replies must not wait for application work.
    let about = match handler.try_lock() {
        Some(held) => held.identity(),
        None => vec![("compiler_identity_available".into(), "false".into())],
    };
    queue_identity(queue, about)
}

fn refused<R: Registry>(
    registry: &R,
    asked: &Asked<R::Request>,
    reason: &str,
    about: &[(String, String)],
) -> Dispatched {
    let shape = match registry.verb(&asked.request).answer_mode {
        AnswerMode::LegacyEnvelope => Shape::Text,
        AnswerMode::CanonicalDocument => asked.shape,
    };
    let reply = registry.rejected(&asked.request.to_string(), shape, reason);
    Dispatched {
        reply: finish(registry, asked, reply, about),
        shutdown: false,
    }
}

fn transport<R: Registry>(
    registry: &R,
    request: &R::Request,
    queue: &Queue,
) -> Option<Dispatched> {
    let route = registry.route(request);
    let reply = match route {
        Route::Handler => return None,
        Route::Help => {
            let mut lines = registry.help();
            lines.push(crate::libs::door::frame::HELP_LINE.to_string());
            Reply::ok(lines)
        }
        Route::Ping => alive(queue),
        Route::List => jobs(queue),
        Route::Cancel { id } => match queue.cancel(id) {
            Ok(said) => Reply::ok(said),
            Err(why) => Reply::err(why),
        },
        Route::Shutdown => Reply::ok("closing"),
    };
    Some(Dispatched { reply, shutdown: route == Route::Shutdown })
}

/// The answer and its identity are captured while the caller owns the same handler guard.
///
/// There is no containment here: every unit is compiled with `panic = "abort"`, so a handler that
/// panics ends the process. A request a handler cannot serve returns an error reply instead.
fn handled<H: Handler>(
    handler: &mut H,
    registry: &H::Registry,
    asked: &Asked<<H::Registry as Registry>::Request>,
    running: &Running<'_>,
) -> Reply {
    let reply = handler.handle(asked.request.clone(), asked.shape, running);
    let about = if asked.shape == Shape::Json
        && registry.verb(&asked.request).answer_mode == AnswerMode::LegacyEnvelope
    {
        running.queue_identity(handler.identity())
    } else {
        Vec::new()
    };
    finish(registry, asked, reply, &about)
}

pub fn dispatch<H: Handler>(
    handler: &mut H,
    asked: Asked<<H::Registry as Registry>::Request>,
    queue: &Arc<Queue>,
) -> Dispatched {
    let registry = handler.registry();
    if let Err(why) = admission(registry.as_ref(), &asked) {
        let about = if asked.shape == Shape::Json { queue_identity(queue, handler.identity()) } else { Vec::new() };
        return refused(registry.as_ref(), &asked, &why, &about);
    }
    if let Some(mut answer) = transport(registry.as_ref(), &asked.request, queue) {
        let about = if asked.shape == Shape::Json { queue_identity(queue, handler.identity()) } else { Vec::new() };
        answer.reply = finish(registry.as_ref(), &asked, answer.reply, &about);
        return answer;
    }
    let submission = registry.submission(&asked.request);
    let described = asked.request.to_string();
    let ticket = match queue.submit(&described, submission.owner, submission.priority) {
        Ok(ticket) => ticket,
        Err(why) => {
            let about = if asked.shape == Shape::Json { queue_identity(queue, handler.identity()) } else { Vec::new() };
            return refused(registry.as_ref(), &asked, &why, &about);
        }
    };
    if !queue.wait_for_turn(ticket.id) {
        let about = if asked.shape == Shape::Json { queue_identity(queue, handler.identity()) } else { Vec::new() };
        return refused(registry.as_ref(), &asked, &format!(
            "job {} was cancelled before it started", ticket.id,
        ), &about);
    }
    let running = Running::in_queue(queue, ticket.id);
    let answer = Dispatched {
        reply: handled(handler, registry.as_ref(), &asked, &running),
        shutdown: false,
    };
    queue.retain_reply();
    answer
}

fn dispatch_shared<H: Handler>(
    handler: &Mutex<H>,
    registry: &H::Registry,
    asked: Asked<<H::Registry as Registry>::Request>,
    queue: &Arc<Queue>,
) -> Dispatched {
    if let Err(why) = admission(registry, &asked) {
        let about = if asked.shape == Shape::Json {
            shared_identity(handler, queue)
        } else {
            Vec::new()
        };
        return refused(registry, &asked, &why, &about);
    }
    if let Some(mut answer) = transport(registry, &asked.request, queue) {
        let about = if asked.shape == Shape::Json {
            shared_identity(handler, queue)
        } else {
            Vec::new()
        };
        answer.reply = finish(registry, &asked, answer.reply, &about);
        return answer;
    }
    let submission = registry.submission(&asked.request);
    let ticket = match queue.submit(
        &asked.request.to_string(), submission.owner, submission.priority,
    ) {
        Ok(ticket) => ticket,
        Err(why) => {
            let about = if asked.shape == Shape::Json {
                shared_identity(handler, queue)
            } else {
                Vec::new()
            };
            return refused(registry, &asked, &why, &about);
        }
    };
    if !queue.wait_for_turn(ticket.id) {
        let about = if asked.shape == Shape::Json {
            shared_identity(handler, queue)
        } else {
            Vec::new()
        };
        return refused(registry, &asked, &format!(
            "job {} was cancelled before it started", ticket.id,
        ), &about);
    }
    // The ticket takes its turn before this lock. Ticket drop releases admission on every
    // return path.
    let running = Running::in_queue(queue, ticket.id);
    let mut held = handler.lock();
    let answer = Dispatched { reply: handled(&mut *held, registry, &asked, &running), shutdown: false };
    queue.retain_reply();
    answer
}

pub fn bind(socket: &str) -> Result<Listener, crate::libs::door::sys::Errno> {
    let path = crate::libs::door::client::c_path(socket);
    Listener::bind(&path, 128)
}

pub fn serve<H: Handler>(socket: &str, handler: H) -> Result<(), crate::libs::door::sys::Errno> {
    let queue = Arc::new(Queue::new());
    let listener = bind(socket)?;
    serve_control(socket, handler.registry(), Arc::clone(&queue))?;
    serve_on(listener, socket, handler, &queue)
}

/// `serve` for a program that is not `ekod`: the MCP `initialize` answer names this server, and
/// a debugger answering as the compiler would send an agent to the wrong documentation.
pub fn serve_named<H: Handler>(
    socket: &str,
    handler: H,
    server_name: &str,
    version: &str,
) -> Result<(), crate::libs::door::sys::Errno> {
    let queue = Arc::new(Queue::new());
    let listener = bind(socket)?;
    serve_control(socket, handler.registry(), Arc::clone(&queue))?;
    serve_on_named(listener, socket, handler, &queue, server_name, version)
}

const REQUEST_STACK: usize = 64 * 1024 * 1024;
const CONTROL_STACK: usize = 1024 * 1024;

// Descriptor exhaustion is recoverable when another session closes. Back off so
// a busy process does not spin, and retain both listeners for that recovery.
fn retry_accept(error: &crate::libs::door::sys::Errno) -> bool {
    match error.0 {
        libc::EINTR => true,
        libc::EMFILE | libc::ENFILE => {
            crate::libs::door::sys::sleep_ms(200);
            true
        }
        _ => false,
    }
}

pub fn serve_on<H: Handler>(
    listener: Listener,
    socket: &str,
    handler: H,
    queue: &Arc<Queue>,
) -> Result<(), crate::libs::door::sys::Errno> {
    serve_on_named(listener, socket, handler, queue, "ekod", env!("CARGO_PKG_VERSION"))
}

/// An additive entry point for applications with another MCP identity.
/// Binding and control startup remain separate so readiness can be announced afterward.
pub fn serve_on_named<H: Handler>(
    listener: Listener,
    socket: &str,
    handler: H,
    queue: &Arc<Queue>,
    server_name: &str,
    version: &str,
) -> Result<(), crate::libs::door::sys::Errno> {
    serve_on_named_with_warmup::<H, fn(&mut H, &Running<'_>)>(
        listener,
        socket,
        handler,
        queue,
        server_name,
        version,
        None,
    )
}

/// Entry point that acquires the first queue turn for cold-boot warmup (`"Warming up"`)
/// before accepting client connections, so the socket is immediately live for `ping`/`list`
/// while the user's 1st `compile` blocks in the queue until warmup finishes.
pub fn serve_on_named_with_warmup<H, W>(
    listener: Listener,
    socket: &str,
    handler: H,
    queue: &Arc<Queue>,
    server_name: &str,
    version: &str,
    warmup: Option<(String, W)>,
) -> Result<(), crate::libs::door::sys::Errno>
where
    H: Handler,
    W: FnOnce(&mut H, &Running<'_>) + Send + 'static,
{
    let registry = handler.registry();
    let handler = Arc::new(Mutex::new(handler));
    if let Some((warmup_label, warmup_fn)) = warmup {
        let warmup_handler = Arc::clone(&handler);
        let warmup_queue = Arc::clone(queue);
        let started = Arc::new(AtomicBool::new(false));
        let started_signal = Arc::clone(&started);
        if crate::libs::door::sys::thread::spawn_detached(REQUEST_STACK, move || {
            let Ok(ticket) = warmup_queue.submit(&warmup_label, Some("ekod".to_string()), 0) else {
                started_signal.store(true, Ordering::Release);
                return;
            };
            let my_turn = warmup_queue.wait_for_turn(ticket.id);
            started_signal.store(true, Ordering::Release);
            if !my_turn {
                return;
            }
            let running = Running::in_queue(&warmup_queue, ticket.id);
            let mut held = warmup_handler.lock();
            let mut warmup_opt = Some(warmup_fn);
            if let Some(f) = warmup_opt.take() {
                f(&mut *held, &running);
            }
        })
        .is_ok()
        {
            while !started.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let socket_owned = socket.to_string();
    let server_name: Arc<str> = Arc::from(server_name);
    let version: Arc<str> = Arc::from(version);
    while !stop.load(Ordering::Relaxed) {
        let tick = handler.try_lock().and_then(|mut held| held.idle());
        listener.set_nonblocking(tick.is_some())?;
        let stream = match listener.accept() {
            Ok(stream) => stream,
            Err(error) if error.would_block() => {
                crate::libs::door::sys::sleep_ms(tick.map(|d| d.as_millis() as u64).unwrap_or(200));
                continue;
            }
            Err(error) if retry_accept(&error) => continue,
            Err(error) => return Err(error),
        };
        if stop.load(Ordering::Relaxed) {
            break;
        }
        stream.set_nonblocking(false)?;
        let handler = Arc::clone(&handler);
        let registry = Arc::clone(&registry);
        let queue = Arc::clone(queue);
        let stop = Arc::clone(&stop);
        let knock = socket_owned.clone();
        let server_name = Arc::clone(&server_name);
        let version = Arc::clone(&version);
        let _ = crate::libs::door::sys::thread::spawn_detached(REQUEST_STACK, move || {
            if let Ok(true) = session(
                stream, &handler, registry.as_ref(), &queue, &server_name, &version,
            ) {
                stop.store(true, Ordering::Relaxed);
                let _ = Stream::connect(&crate::libs::door::client::c_path(&knock));
            }
        });
    }
    // The API does not retain an ownership token for either pathname. Leave endpoint cleanup
    // to the process that can prove ownership rather than unlinking a possibly replaced path.
    Ok(())
}

fn session<H: Handler>(
    stream: Stream,
    handler: &Mutex<H>,
    registry: &H::Registry,
    queue: &Arc<Queue>,
    server_name: &str,
    version: &str,
) -> Result<bool, crate::libs::door::sys::Errno> {
    if crate::libs::door::frame::peek_first(stream.raw())? == Some(crate::libs::door::frame::FIRST_BYTE) {
        return session_framed(stream, handler, registry, queue, server_name, version);
    }
    let mut out = stream.duplicate()?;
    let mut reader = stream.lines();
    let mut rpc = None;
    while let Some(line) = reader.next_line()? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rpc = *rpc.get_or_insert_with(|| line.starts_with('{'));
        if rpc {
            match crate::libs::door::mcp::dispatch(line, registry, server_name, version) {
                crate::libs::door::mcp::Outcome::Silent => {}
                crate::libs::door::mcp::Outcome::Reply(reply) => {
                    out.write_all(reply.as_bytes())?;
                    out.write_all(b"\n")?;
                }
                crate::libs::door::mcp::Outcome::Ask { asked, id } => {
                    let answer = dispatch_shared(handler, registry, asked, queue);
                    let text = crate::libs::door::mcp::answered(id, answer.reply.ok, &answer.reply.body);
                    out.write_all(text.as_bytes())?;
                    out.write_all(b"\n")?;
                    if answer.shutdown {
                        return Ok(true);
                    }
                }
            }
        } else {
            let asked = match crate::libs::door::typed::parse(registry, line) {
                Ok(asked) => asked,
                Err(why) => {
                    let reply = registry.rejected(
                        line, crate::libs::door::typed::requested_shape(line), &why,
                    );
                    out.write_all(reply.wire().as_bytes())?;
                    continue;
                }
            };
            let answer = dispatch_shared(handler, registry, asked, queue);
            out.write_all(answer.reply.wire().as_bytes())?;
            if answer.shutdown {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The framed wire (see `crate::libs::door::frame`): the same requests as the counted session, one Text frame
/// each, answered by one Text frame each.
fn session_framed<H: Handler>(
    stream: Stream,
    handler: &Mutex<H>,
    registry: &H::Registry,
    queue: &Arc<Queue>,
    server_name: &str,
    version: &str,
) -> Result<bool, crate::libs::door::sys::Errno> {
    use crate::libs::door::frame::{Frame, FrameDecoder};
    let mut out = stream.duplicate()?;
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        loop {
            let frame = match decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(why) => {
                    let mut bytes = Vec::new();
                    let _ = crate::libs::door::frame::encode_close(Some((1002, &why.to_string())), &mut bytes);
                    let _ = out.write_all(&bytes);
                    return Ok(false);
                }
            };
            match frame {
                Frame::Text(bytes) => {
                    let Ok(text) = core::str::from_utf8(&bytes) else { continue };
                    let line = text.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let mut reply_text = None;
                    let mut shutdown = false;
                    if line.starts_with('{') {
                        match crate::libs::door::mcp::dispatch(line, registry, server_name, version) {
                            crate::libs::door::mcp::Outcome::Silent => {}
                            crate::libs::door::mcp::Outcome::Reply(reply) => reply_text = Some(reply),
                            crate::libs::door::mcp::Outcome::Ask { asked, id } => {
                                let answer = dispatch_shared(handler, registry, asked, queue);
                                reply_text =
                                    Some(crate::libs::door::mcp::answered(id, answer.reply.ok, &answer.reply.body));
                                shutdown = answer.shutdown;
                            }
                        }
                        if let Some(text) = reply_text {
                            let mut bytes = Vec::new();
                            if crate::libs::door::frame::encode_text(&text, &mut bytes).is_ok() {
                                out.write_all(&bytes)?;
                            }
                        }
                    } else {
                        let reply = match crate::libs::door::typed::parse(registry, line) {
                            Ok(asked) => {
                                let answer = dispatch_shared(handler, registry, asked, queue);
                                shutdown = answer.shutdown;
                                answer.reply
                            }
                            Err(why) => registry.rejected(
                                line, crate::libs::door::typed::requested_shape(line), &why,
                            ),
                        };
                        out.write_all(&crate::libs::door::frame::reply_frame(&reply))?;
                    }
                    if shutdown {
                        return Ok(true);
                    }
                }
                Frame::Ping(data) => {
                    let mut bytes = Vec::new();
                    if crate::libs::door::frame::encode_frame(crate::libs::door::frame::KIND_PONG, &data, &mut bytes).is_ok() {
                        out.write_all(&bytes)?;
                    }
                }
                Frame::Close(_) => {
                    let mut bytes = Vec::new();
                    let _ = crate::libs::door::frame::encode_close(None, &mut bytes);
                    let _ = out.write_all(&bytes);
                    return Ok(false);
                }
                Frame::Binary(_) | Frame::Pong(_) => {}
            }
        }
        let got = crate::libs::door::frame::read_some(stream.raw(), &mut buf)?;
        if got == 0 {
            return Ok(false);
        }
        decoder.push(&buf[..got]);
    }
}

pub fn control_socket(socket: &str) -> String {
    format!("{socket}.control")
}

pub fn serve_control<R: Registry>(
    socket: &str,
    registry: Arc<R>,
    queue: Arc<Queue>,
) -> Result<(), crate::libs::door::sys::Errno> {
    let at = control_socket(socket);
    let path = crate::libs::door::client::c_path(&at);
    let listener = Listener::bind(&path, 128)?;
    crate::libs::door::sys::thread::spawn_detached(CONTROL_STACK, move || {
        loop {
            let stream = match listener.accept() {
                Ok(stream) => stream,
                Err(error) if retry_accept(&error) => continue,
                Err(_) => break,
            };
            let registry = Arc::clone(&registry);
            let queue = Arc::clone(&queue);
            let _ = crate::libs::door::sys::thread::spawn_detached(CONTROL_STACK, move || {
                let Ok(mut out) = stream.duplicate() else { return };
                let mut reader = stream.lines();
                while let Ok(Some(line)) = reader.next_line() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let reply = match crate::libs::door::typed::parse(registry.as_ref(), line) {
                        Err(why) => registry.rejected(
                            line, crate::libs::door::typed::requested_shape(line), &why,
                        ),
                        Ok(asked) => match admission(registry.as_ref(), &asked) {
                            Err(why) => registry.rejected(line, asked.shape, &why),
                            Ok(()) => match registry.route(&asked.request) {
                                Route::Ping | Route::List | Route::Cancel { .. } => {
                                    let answer = transport(registry.as_ref(), &asked.request, &queue)
                                        .expect("control routes have transport answers");
                                    finish(registry.as_ref(), &asked, answer.reply, &[])
                                }
                                _ => registry.rejected(
                                    line, asked.shape,
                                    &format!(
                                        "the control socket answers `list`, `cancel <id>` and \
                                         `ping`; `{}` needs the handler, which is what is busy",
                                        asked.request,
                                    ),
                                ),
                            },
                        },
                    };
                    if out.write_all(reply.wire().as_bytes()).is_err() {
                        break;
                    }
                }
            });
        }
    })?;
    Ok(())
}

fn alive(queue: &Queue) -> Reply {
    let jobs = queue.jobs();
    let running = jobs.iter().filter(|j| j.state == State::Running).count();
    let waiting = jobs.len() - running;
    if let Some(warmup) = jobs
        .iter()
        .find(|j| j.state == State::Running && j.request.starts_with("Warming up"))
    {
        return Reply::ok(format!(
            "alive, {}; {running} running and {waiting} queued of {} slots; `list` says whose",
            warmup.request,
            queue.width().concurrent,
        ));
    }
    match (running, waiting) {
        (0, 0) => Reply::ok("alive, idle"),
        _ => Reply::ok(format!(
            "alive, {running} running and {waiting} queued of {} slots; `list` says whose",
            queue.width().concurrent,
        )),
    }
}

fn jobs(queue: &Queue) -> Reply {
    let rows: Vec<Vec<String>> = queue.jobs().into_iter().map(|job| {
        vec![
            job.id.to_string(),
            job.state.spelling().to_string(),
            job.owner.clone().unwrap_or_default(),
            job.priority.to_string(),
            job.request.clone(),
            format!("{:.1}", crate::libs::door::since(job.submitted).as_secs_f64()),
            job.started.map(|at| format!("{:.1}", crate::libs::door::since(at).as_secs_f64()))
                .unwrap_or_default(),
            job.polled.to_string(),
        ]
    }).collect();
    // This control table historically used literal cells. Preserve those bytes separately
    // from the driver's general-purpose legacy writer.
    let fields = [
        "id", "state", "owner", "priority", "request", "waiting_s", "running_s", "cancellable",
    ];
    let mut out = format!("jobs[{}]{{{}}}:\n", rows.len(), fields.join(","));
    for row in rows {
        out.push_str("  ");
        out.push_str(&row.join(","));
        out.push('\n');
    }
    Reply::ok(out)
}
