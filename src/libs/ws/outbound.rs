//! Per-connection outbound queue.
//!
//! The bound is the number of queued messages. A [`Sender`] does not reserve
//! a slot of its own. `futures::channel::mpsc` does (`buffer + num_senders`),
//! and `drop_conn_on_buffer_full` would then fire at a different depth than
//! the one [`crate::libs::ws::WsServerConfig::message_buffer_size`] names.
//!
//! [`Sender::try_send`] reports [`TrySendError::Full`] and
//! [`TrySendError::Closed`]. [`Receiver::recv`] yields `None` once every
//! sender is gone and the queue is empty, which the session classifies as
//! `Outbound::Closed`. There is no asynchronous send: the producers use
//! `try_send`, and a capacity of zero therefore never accepts a message.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use parking_lot::Mutex;

/// The shared queue. One mutex covers the buffer, the sender count and the
/// parked receiver, so a send cannot land between the empty check and the
/// waker store.
struct Inner<T> {
    queue: VecDeque<T>,
    capacity: usize,
    senders: usize,
    receiver_alive: bool,
    recv_waker: Option<Waker>,
}

/// Sending half of a [`channel`].
///
/// Cloning does not consume capacity. Dropping the last clone is what lets
/// [`Receiver::recv`] return `None`.
#[must_use = "dropping the sender closes the queue once the last one is gone"]
pub struct Sender<T> {
    shared: Arc<Mutex<Inner<T>>>,
}

/// Receiving half of a [`channel`].
///
/// Not cloneable. The session loop is the only reader.
#[must_use = "dropping the receiver closes the queue"]
pub struct Receiver<T> {
    shared: Arc<Mutex<Inner<T>>>,
}

/// [`Sender::try_send`] could not accept the message. The message comes back
/// with the error.
#[derive(Debug)]
pub enum TrySendError<T> {
    /// `len == capacity`. A cloned sender does not make this less likely.
    Full(T),
    /// The [`Receiver`] has been dropped. This is reported even when the
    /// buffer still has room, and even when it is already full.
    Closed(T),
}

/// [`Receiver::try_recv`] found nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryRecvError {
    /// No message yet, and a sender still exists.
    Empty,
    /// No message, and every sender has been dropped.
    Disconnected,
}

impl<T> TrySendError<T> {
    /// The message that was not queued.
    pub fn into_inner(self) -> T {
        match self {
            TrySendError::Full(value) | TrySendError::Closed(value) => value,
        }
    }
}

impl<T> std::fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrySendError::Full(_) => f.write_str("outbound queue full"),
            TrySendError::Closed(_) => f.write_str("outbound queue closed"),
        }
    }
}

impl<T: std::fmt::Debug> std::error::Error for TrySendError<T> {}

impl std::fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryRecvError::Empty => f.write_str("outbound queue empty"),
            TryRecvError::Disconnected => f.write_str("outbound queue disconnected"),
        }
    }
}

impl std::error::Error for TryRecvError {}

/// Create a bounded queue. `capacity` is the maximum number of queued
/// messages, not a per-sender allowance. Zero accepts nothing via
/// [`Sender::try_send`].
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Mutex::new(Inner {
        queue: VecDeque::new(),
        capacity,
        senders: 1,
        receiver_alive: true,
        recv_waker: None,
    }));
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver { shared },
    )
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.lock().senders += 1;
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut inner = self.shared.lock();
            inner.senders -= 1;
            if inner.senders == 0 {
                inner.recv_waker.take()
            } else {
                None
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Sender<T> {
    /// Queue `value`, or give it back.
    ///
    /// A full buffer and a dropped receiver are different answers.
    /// [`TrySendError::Closed`] wins when both are true, so a late send after
    /// teardown is not reported as backpressure.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let waker = {
            let mut inner = self.shared.lock();
            if !inner.receiver_alive {
                return Err(TrySendError::Closed(value));
            }
            if inner.queue.len() >= inner.capacity {
                return Err(TrySendError::Full(value));
            }
            inner.queue.push_back(value);
            inner.recv_waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.lock().receiver_alive = false;
    }
}

impl<T> Receiver<T> {
    /// The next message, if one is already queued.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut inner = self.shared.lock();
        match inner.pop() {
            Pop::Value(value) => Ok(value),
            Pop::Empty => Err(TryRecvError::Empty),
            Pop::Closed => Err(TryRecvError::Disconnected),
        }
    }

    /// The next message, or `None` when every sender is gone and the queue
    /// is empty. Queued messages are delivered first.
    ///
    /// Cancel-safe: a message is removed only when this returns `Ready`.
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv { receiver: self }
    }
}

enum Pop<T> {
    Value(T),
    Empty,
    Closed,
}

impl<T> Inner<T> {
    fn pop(&mut self) -> Pop<T> {
        if let Some(value) = self.queue.pop_front() {
            Pop::Value(value)
        } else if self.senders == 0 {
            Pop::Closed
        } else {
            Pop::Empty
        }
    }
}

/// Future from [`Receiver::recv`].
pub struct Recv<'a, T> {
    receiver: &'a mut Receiver<T>,
}

impl<T> Future for Recv<'_, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let this = self.get_mut();
        let mut inner = this.receiver.shared.lock();
        match inner.pop() {
            Pop::Value(value) => Poll::Ready(Some(value)),
            Pop::Closed => Poll::Ready(None),
            Pop::Empty => {
                match inner.recv_waker {
                    Some(ref parked) if parked.will_wake(cx.waker()) => {}
                    _ => inner.recv_waker = Some(cx.waker().clone()),
                }
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use super::{TryRecvError, TrySendError, channel};

    struct Flag(AtomicBool);

    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn flag_waker() -> (Arc<Flag>, Waker) {
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        (flag, waker)
    }

    #[test]
    fn sends_leave_in_arrival_order() {
        let (tx, mut rx) = channel(4);
        let tx2 = tx.clone();
        tx.try_send(1).unwrap();
        tx2.try_send(2).unwrap();
        tx.try_send(3).unwrap();
        assert_eq!(rx.try_recv().unwrap(), 1);
        assert_eq!(rx.try_recv().unwrap(), 2);
        assert_eq!(rx.try_recv().unwrap(), 3);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn cloned_senders_do_not_add_capacity() {
        let (tx, _rx) = channel(1);
        let second = tx.clone();
        let third = tx.clone();
        tx.try_send(1).unwrap();
        assert!(matches!(second.try_send(2), Err(TrySendError::Full(2))));
        assert!(matches!(third.try_send(3), Err(TrySendError::Full(3))));
    }

    #[test]
    fn a_dropped_receiver_is_closed_even_when_full() {
        let (tx, rx) = channel(1);
        tx.try_send(1).unwrap();
        drop(rx);
        assert!(matches!(tx.try_send(2), Err(TrySendError::Closed(2))));
        assert_eq!(tx.try_send(3).unwrap_err().into_inner(), 3);
    }

    #[test]
    fn a_dropped_receiver_is_closed_while_there_is_room() {
        let (tx, rx) = channel(4);
        drop(rx);
        assert!(matches!(tx.try_send(1), Err(TrySendError::Closed(1))));
    }

    #[test]
    fn zero_capacity_rejects_while_the_receiver_lives() {
        let (tx, rx) = channel::<u8>(0);
        assert!(matches!(tx.try_send(1), Err(TrySendError::Full(1))));
        drop(rx);
        assert!(matches!(tx.try_send(1), Err(TrySendError::Closed(1))));
    }

    #[test]
    fn empty_while_a_sender_lives_and_disconnected_after_the_last_drop() {
        let (tx, mut rx) = channel::<u8>(1);
        let extra = tx.clone();
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(tx);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(extra);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn queued_messages_are_read_before_close() {
        let (tx, mut rx) = channel(2);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        drop(tx);
        assert_eq!(rx.try_recv().unwrap(), 1);
        assert_eq!(rx.try_recv().unwrap(), 2);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));

        let waker = flag_waker().1;
        let mut cx = Context::from_waker(&waker);
        let mut recv = std::pin::pin!(rx.recv());
        assert!(matches!(recv.as_mut().poll(&mut cx), Poll::Ready(None)));
    }

    #[test]
    fn a_message_sent_before_recv_is_ready_without_parking() {
        let (tx, mut rx) = channel(1);
        tx.try_send(9).unwrap();
        let waker = flag_waker().1;
        let mut cx = Context::from_waker(&waker);
        let mut recv = std::pin::pin!(rx.recv());
        assert!(matches!(recv.as_mut().poll(&mut cx), Poll::Ready(Some(9))));
    }

    #[test]
    fn a_parked_recv_wakes_when_a_message_arrives() {
        let (tx, mut rx) = channel(1);
        let (flag, waker) = flag_waker();
        let mut cx = Context::from_waker(&waker);
        let mut recv = std::pin::pin!(rx.recv());
        assert!(recv.as_mut().poll(&mut cx).is_pending());
        assert!(!flag.0.load(Ordering::SeqCst));
        tx.try_send(7).unwrap();
        assert!(flag.0.load(Ordering::SeqCst));
        assert!(matches!(recv.as_mut().poll(&mut cx), Poll::Ready(Some(7))));
    }

    #[test]
    fn the_receiver_parks_again_after_taking_a_message() {
        let (tx, mut rx) = channel(2);
        let (flag, waker) = flag_waker();
        let mut cx = Context::from_waker(&waker);
        let mut recv = std::pin::pin!(rx.recv());
        assert!(recv.as_mut().poll(&mut cx).is_pending());
        tx.try_send(1).unwrap();
        assert!(matches!(recv.as_mut().poll(&mut cx), Poll::Ready(Some(1))));

        flag.0.store(false, Ordering::SeqCst);
        let mut recv = std::pin::pin!(rx.recv());
        assert!(recv.as_mut().poll(&mut cx).is_pending());
        tx.try_send(2).unwrap();
        assert!(flag.0.load(Ordering::SeqCst));
        assert!(matches!(recv.as_mut().poll(&mut cx), Poll::Ready(Some(2))));
    }

    #[test]
    fn a_parked_recv_completes_when_the_last_sender_drops() {
        let (tx, mut rx) = channel::<u8>(1);
        let extra = tx.clone();
        drop(tx);
        let (flag, waker) = flag_waker();
        let mut cx = Context::from_waker(&waker);
        let mut recv = std::pin::pin!(rx.recv());
        assert!(recv.as_mut().poll(&mut cx).is_pending());
        drop(extra);
        assert!(flag.0.load(Ordering::SeqCst));
        assert!(matches!(recv.as_mut().poll(&mut cx), Poll::Ready(None)));
    }

    #[test]
    fn dropping_one_sender_does_not_close() {
        let (tx, mut rx) = channel(1);
        let extra = tx.clone();
        drop(tx);
        extra.try_send(4).unwrap();
        assert_eq!(rx.try_recv().unwrap(), 4);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }
}
