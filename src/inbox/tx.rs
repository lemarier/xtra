use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::{atomic, Arc};
use std::task::{Context, Poll, Waker};

use event_listener::EventListener;
use futures_core::FusedFuture;
use futures_util::FutureExt;

use super::*;
use crate::envelope::ShutdownAll;
use crate::inbox::tx::private::RefCounterInner;
use crate::send_future::private::SetPriority;
use crate::{Actor, Error};

pub struct Sender<A, Rc: TxRefCounter> {
    pub(super) inner: Arc<Chan<A>>,
    pub(super) rc: Rc,
}

impl<A> Sender<A, TxStrong> {
    pub fn new(inner: Arc<Chan<A>>) -> Self {
        let rc = TxStrong(());
        rc.increment(&inner);

        Sender { inner, rc }
    }
}

impl<Rc: TxRefCounter, A> Sender<A, Rc> {
    fn try_send(&self, message: SentMessage<A>) -> Result<(), TrySendFail<A>> {
        let mut inner = self.inner.chan.lock().unwrap();

        if !self.is_connected() {
            return Err(TrySendFail::Disconnected);
        }

        match message {
            SentMessage::ToAllActors(m) if !self.inner.is_full(inner.broadcast_tail) => {
                inner.send_broadcast(MessageToAllActors(m));
                Ok(())
            }
            SentMessage::ToAllActors(m) => {
                // on_shutdown is only notified with inner locked, and it's locked here, so no race
                let waiting = WaitingSender::new(SentMessage::ToAllActors(m));
                inner.waiting_senders.push_back(Arc::downgrade(&waiting));
                Err(TrySendFail::Full(waiting))
            }
            msg => {
                let res = inner.try_fulfill_receiver(msg.into());
                match res {
                    Ok(()) => Ok(()),
                    Err(WakeReason::MessageToOneActor(m))
                        if m.priority == 0 && !self.inner.is_full(inner.ordered_queue.len()) =>
                    {
                        inner.ordered_queue.push_back(m.val);
                        Ok(())
                    }
                    Err(WakeReason::MessageToOneActor(m))
                        if m.priority != 0 && !self.inner.is_full(inner.priority_queue.len()) =>
                    {
                        inner.priority_queue.push(m);
                        Ok(())
                    }
                    Err(WakeReason::MessageToOneActor(m)) => {
                        let waiting = WaitingSender::new(m.into());
                        inner.waiting_senders.push_back(Arc::downgrade(&waiting));
                        Err(TrySendFail::Full(waiting))
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    pub fn stop_all_receivers(&self)
    where
        A: Actor,
    {
        self.inner
            .chan
            .lock()
            .unwrap()
            .send_broadcast(MessageToAllActors(Arc::new(ShutdownAll::new())));
    }

    pub fn send(&self, message: SentMessage<A>) -> SendFuture<A, Rc> {
        SendFuture::new(message, self.clone())
    }

    pub fn downgrade(&self) -> Sender<A, TxWeak> {
        Sender {
            inner: self.inner.clone(),
            rc: TxWeak(()),
        }
    }

    pub fn is_strong(&self) -> bool {
        self.rc.is_strong()
    }

    pub fn inner_ptr(&self) -> *const Chan<A> {
        (&self.inner as &Chan<A>) as *const Chan<A>
    }

    pub fn into_either_rc(self) -> Sender<A, TxEither> {
        Sender {
            inner: self.inner.clone(),
            rc: self.rc.increment(&self.inner).into_either(),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.inner.receiver_count.load(atomic::Ordering::SeqCst) > 0
            && self.inner.sender_count.load(atomic::Ordering::SeqCst) > 0
    }

    pub fn capacity(&self) -> Option<usize> {
        self.inner.capacity
    }

    pub fn len(&self) -> usize {
        let inner = self.inner.chan.lock().unwrap();
        inner.broadcast_tail + inner.ordered_queue.len() + inner.priority_queue.len()
    }

    pub fn disconnect_notice(&self) -> Option<EventListener> {
        // Listener is created before checking connectivity to avoid the following race scenario:
        //
        // 1. is_connected returns true
        // 2. on_shutdown is notified
        // 3. listener is registered
        //
        // The listener would never be woken in this scenario, as the notification preceded its
        // creation.
        let listener = self.inner.on_shutdown.listen();

        if self.is_connected() {
            Some(listener)
        } else {
            None
        }
    }
}

impl<A, Rc: TxRefCounter> Clone for Sender<A, Rc> {
    fn clone(&self) -> Self {
        Sender {
            inner: self.inner.clone(),
            rc: self.rc.increment(&self.inner),
        }
    }
}

impl<A, Rc: TxRefCounter> Drop for Sender<A, Rc> {
    fn drop(&mut self) {
        if self.rc.decrement(&self.inner) {
            let waiting_rx = {
                let mut inner = match self.inner.chan.lock() {
                    Ok(lock) => lock,
                    Err(_) => return, // Poisoned, ignore
                };

                // We don't need to notify on_shutdown here, as that is only used by senders
                // Receivers will be woken with the fulfills below, or they will realise there are
                // no senders when they check the tx refcount

                mem::take(&mut inner.waiting_receivers)
            };

            for rx in waiting_rx.into_iter().flat_map(|w| w.upgrade()) {
                let _ = rx.lock().fulfill(WakeReason::Shutdown);
            }
        }
    }
}

impl<A, Rc: TxRefCounter> Debug for Sender<A, Rc> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        use atomic::Ordering::SeqCst;

        let act = std::any::type_name::<A>();
        f.debug_struct(&format!("Sender<{}>", act))
            .field("rx_count", &self.inner.receiver_count.load(SeqCst))
            .field("tx_count", &self.inner.sender_count.load(SeqCst))
            .field("rc", &self.rc)
            .finish()
    }
}

#[must_use = "Futures do nothing unless polled"]
pub struct SendFuture<A, Rc: TxRefCounter> {
    pub tx: Sender<A, Rc>,
    inner: SendFutureInner<A>,
}

impl<A, Rc: TxRefCounter> SendFuture<A, Rc> {
    fn new(msg: SentMessage<A>, tx: Sender<A, Rc>) -> Self {
        SendFuture {
            tx,
            inner: SendFutureInner::New(msg),
        }
    }
}

impl<A, Rc: TxRefCounter> SetPriority for SendFuture<A, Rc> {
    fn set_priority(&mut self, priority: u32) {
        match &mut self.inner {
            SendFutureInner::New(SentMessage::ToOneActor(ref mut m)) => m.priority = priority,
            _ => panic!("setting priority after polling is unsupported"),
        }
    }
}

pub enum SendFutureInner<A> {
    New(SentMessage<A>),
    WaitingToSend(Arc<Spinlock<WaitingSender<A>>>),
    Complete,
}

impl<A, Rc: TxRefCounter> Future for SendFuture<A, Rc> {
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        match mem::replace(&mut self.inner, SendFutureInner::Complete) {
            SendFutureInner::New(msg) => match self.tx.try_send(msg) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(TrySendFail::Disconnected) => Poll::Ready(Err(Error::Disconnected)),
                Err(TrySendFail::Full(waiting)) => {
                    // Start waiting. The waiting sender should be immediately polled, in case a
                    // receive operation happened between `try_send` and here, in which case the
                    // WaitingSender would be fulfilled, but not properly woken.
                    self.inner = SendFutureInner::WaitingToSend(waiting);
                    self.poll_unpin(cx)
                }
            },
            SendFutureInner::WaitingToSend(waiting) => {
                {
                    let mut inner = waiting.lock();

                    match inner.message {
                        WaitingSenderInner::New(_) => inner.waker = Some(cx.waker().clone()), // The message has not yet been taken
                        WaitingSenderInner::Delivered => return Poll::Ready(Ok(())),
                        WaitingSenderInner::Closed => return Poll::Ready(Err(Error::Disconnected)),
                    }
                }

                self.inner = SendFutureInner::WaitingToSend(waiting);
                Poll::Pending
            }
            SendFutureInner::Complete => Poll::Pending,
        }
    }
}

pub struct WaitingSender<A> {
    waker: Option<Waker>,
    message: WaitingSenderInner<A>,
}

enum WaitingSenderInner<A> {
    New(SentMessage<A>),
    Delivered,
    Closed,
}

impl<A> WaitingSender<A> {
    pub fn new(message: SentMessage<A>) -> Arc<Spinlock<Self>> {
        let sender = WaitingSender {
            waker: None,
            message: WaitingSenderInner::New(message),
        };
        Arc::new(Spinlock::new(sender))
    }

    pub fn peek(&self) -> &SentMessage<A> {
        match &self.message {
            WaitingSenderInner::New(msg) => msg,
            _ => panic!("WaitingSender should have message"),
        }
    }

    pub fn fulfill(&mut self, is_delivered: bool) -> SentMessage<A> {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }

        let new = if is_delivered {
            WaitingSenderInner::Delivered
        } else {
            WaitingSenderInner::Closed
        };

        match mem::replace(&mut self.message, new) {
            WaitingSenderInner::New(msg) => msg,
            _ => panic!("WaitingSender should have message"),
        }
    }
}

impl<A, Rc: TxRefCounter> FusedFuture for SendFuture<A, Rc> {
    fn is_terminated(&self) -> bool {
        matches!(self.inner, SendFutureInner::Complete)
    }
}

/// This trait represents the strength of an address's reference counting. It is an internal trait.
/// There are two implementations of this trait: [`Weak`](TxWeak) and [`Strong`](TxStrong). These
/// can be provided as the second type argument to [`Address`](crate::Address) in order to change how the address
/// affects the actor's dropping. Read the docs of [`Address`](crate::Address) to find out more.
pub trait TxRefCounter: RefCounterInner + Unpin + Debug + Send + Sync + 'static {}

impl TxRefCounter for TxStrong {}
impl TxRefCounter for TxWeak {}
impl TxRefCounter for TxEither {}

/// The reference count of a strong address. Strong addresses will prevent the actor from being
/// dropped as long as they live. Read the docs of [`Address`](crate::Address) to find
/// out more.
#[derive(Debug)]
pub struct TxStrong(());

/// The reference count of a weak address. Weak addresses will bit prevent the actor from being
/// dropped. Read the docs of [`Address`](crate::Address) to find out more.
#[derive(Debug)]
pub struct TxWeak(());

impl TxStrong {
    /// Attempt to construct a new `TxStrong` pointing to the given `inner` if there are existing
    /// strong references to `inner`. This will return `None` if there were 0 strong references to
    /// the inner.
    pub(crate) fn try_new<A>(inner: &Chan<A>) -> Option<TxStrong> {
        // All code taken from Weak::upgrade in std
        use std::sync::atomic::Ordering::*;

        // Relaxed load because any write of 0 that we can observe leaves the field in a permanently
        // zero state (so a "stale" read of 0 is fine), and any other value is confirmed via the
        // CAS below.
        let mut n = inner.sender_count.load(Relaxed);

        loop {
            if n == 0 {
                return None;
            }

            // Relaxed is fine for the failure case because we don't have any expectations about the new state.
            // Acquire is necessary for the success case to synchronise with `Arc::new_cyclic`, when the inner
            // value can be initialized after `Weak` references have already been created. In that case, we
            // expect to observe the fully initialized value.
            match inner
                .sender_count
                .compare_exchange_weak(n, n + 1, Acquire, Relaxed)
            {
                Ok(_) => return Some(TxStrong(())), // 0-case checked above
                Err(old) => n = old,
            }
        }
    }
}

impl TxWeak {
    pub(crate) fn new<A>(_inner: &Chan<A>) -> TxWeak {
        TxWeak(())
    }
}

/// A reference counter that can be dynamically either strong or weak.
#[derive(Debug)]
pub enum TxEither {
    /// A strong reference counter.
    Strong(TxStrong),
    /// A weak reference counter.
    Weak(TxWeak),
}

mod private {
    use std::sync::atomic;
    use std::sync::atomic::Ordering;

    use super::{TxEither, TxStrong, TxWeak};
    use crate::inbox::Chan;

    pub trait RefCounterInner {
        /// Increments the reference counter, returning a new reference counter for the same
        /// allocation
        fn increment<A>(&self, inner: &Chan<A>) -> Self;
        /// Decrements the reference counter, returning whether the inner data should be dropped
        #[must_use = "If decrement returns false, the address must be disconnected"]
        fn decrement<A>(&self, inner: &Chan<A>) -> bool;
        /// Converts this reference counter into a dynamic reference counter.
        fn into_either(self) -> TxEither;
        /// Returns if this reference counter is a strong reference counter
        fn is_strong(&self) -> bool;
    }

    impl RefCounterInner for TxStrong {
        fn increment<A>(&self, inner: &Chan<A>) -> Self {
            // Memory orderings copied from Arc::clone
            inner.sender_count.fetch_add(1, Ordering::Relaxed);
            TxStrong(())
        }

        fn decrement<A>(&self, inner: &Chan<A>) -> bool {
            // Memory orderings copied from Arc::drop
            if inner.sender_count.fetch_sub(1, Ordering::Release) != 1 {
                return false;
            }

            atomic::fence(Ordering::Acquire);
            true
        }

        fn into_either(self) -> TxEither {
            TxEither::Strong(self)
        }

        fn is_strong(&self) -> bool {
            true
        }
    }

    impl RefCounterInner for TxWeak {
        fn increment<A>(&self, _inner: &Chan<A>) -> Self {
            // A weak being cloned does not affect the strong count
            TxWeak(())
        }

        fn decrement<A>(&self, _inner: &Chan<A>) -> bool {
            // A weak being dropped can never result in the inner data being dropped, as this
            // depends on strongs alone
            false
        }

        fn into_either(self) -> TxEither {
            TxEither::Weak(self)
        }
        fn is_strong(&self) -> bool {
            false
        }
    }

    impl RefCounterInner for TxEither {
        fn increment<A>(&self, inner: &Chan<A>) -> Self {
            match self {
                TxEither::Strong(strong) => TxEither::Strong(strong.increment(inner)),
                TxEither::Weak(weak) => TxEither::Weak(weak.increment(inner)),
            }
        }

        fn decrement<A>(&self, inner: &Chan<A>) -> bool {
            match self {
                TxEither::Strong(strong) => strong.decrement(inner),
                TxEither::Weak(weak) => weak.decrement(inner),
            }
        }

        fn into_either(self) -> TxEither {
            self
        }

        fn is_strong(&self) -> bool {
            match self {
                TxEither::Strong(_) => true,
                TxEither::Weak(_) => false,
            }
        }
    }
}
