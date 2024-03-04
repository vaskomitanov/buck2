#![forbid(unsafe_code)]
#![warn(missing_docs)]

use futures::{future::*, stream::*, task::Poll};
use futures::Sink;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use futures::future::try_join_all;
use slab::Slab;
use std::fmt::{self, Debug};
use std::sync::Arc;

use parking_lot::RwLock;
use futures::task::Context;
use std::pin::Pin;
use futures::channel::mpsc::{channel, Receiver, Sender, TrySendError, unbounded, UnboundedReceiver, UnboundedSender};

/// A broadcast channel, wrapping any clonable Stream and Sink to have every message sent to every
/// receiver.
pub struct BroadcastChannel<
    T,
    S = UnboundedSender<T>,
    R = UnboundedReceiver<T>,
> where
    T: Send + Clone + 'static,
    S: Send + Sync + Unpin + Clone + Sink<T>,
    R: Unpin + Stream<Item = T>,
{
    senders: Arc<RwLock<Slab<S>>>,
    sender_key: usize,
    receiver: R,
    ctor: Arc<dyn Fn() -> (S, R) + Send + Sync>,
}

impl<T: Send + Clone> BroadcastChannel<T> {
    /// Create a new unbounded channel. Requires the `default-channels` feature.
    pub fn new() -> Self {
        let (tx, rx) = unbounded();
        let mut slab = Slab::new();
        let sender_key = slab.insert(tx);
        Self {
            senders: Arc::new(RwLock::new(slab)),
            sender_key,
            receiver: rx,
            ctor: Arc::new(unbounded),
        }
    }
}

impl<T: Send + Clone> BroadcastChannel<T, Sender<T>, Receiver<T>> {
    /// Create a new bounded channel with a specific capacity. Requires the `default-channels` feature.
    pub fn with_cap(cap: usize) -> Self {
        let (tx, rx) = channel(cap);
        let mut slab = Slab::new();
        let sender_key = slab.insert(tx);
        Self {
            senders: Arc::new(RwLock::new(slab)),
            sender_key,
            receiver: rx,
            ctor: Arc::new(move || channel(cap)),
        }
    }

    /// Try sending a value on a bounded channel. Requires the `default-channels` feature.
    pub fn try_send(&self, item: &T) -> Result<(), TrySendError<T>> {
        let mut senders: Slab<Sender<T>> = Slab::clone(&*self.senders.read());
        senders
            .iter_mut()
            .map(|(_, s)| s.try_send(item.clone()))
            .collect()
    }
}

impl<T, S, R> BroadcastChannel<T, S, R>
    where
        T: Send + Clone + 'static,
        S: Send + Sync + Unpin + Clone + Sink<T>,
        R: Unpin + Stream<Item = T>,
{
    /// Construct a new channel from any Sink and Stream. For proper functionality, cloning a
    /// Sender will create a new sink that also sends data to Receiver.
    pub fn with_ctor(ctor: Arc<dyn Fn() -> (S, R) + Send + Sync>) -> Self {
        let (tx, rx) = ctor();
        let mut slab = Slab::new();
        let sender_key = slab.insert(tx);
        Self {
            senders: Arc::new(RwLock::new(slab)),
            sender_key,
            receiver: rx,
            ctor,
        }
    }

    /// Send an item to all receivers in the channel, including this one. This is because
    /// futures-channel does not support comparing a sender and receiver. If this is not the
    /// desired behavior, you must handle it yourself.
    pub async fn send(&self, item: &T) -> Result<(), S::Error> {
        let mut senders = self.senders();
        try_join_all(senders.iter_mut().map(|(_, s)| s.send(item.clone()))).await?;
        Ok(())
    }

    /// Receive a single value from the channel.
    pub fn recv(&mut self) -> impl Future<Output = Option<T>> + '_ {
        self.next()
    }

    /// Internal helper method to get a copy of the senders
    fn senders(&self) -> Slab<S> {
        // can't be split up because of how async/await works
        let senders: Slab<S> = Slab::clone(&*self.senders.read());

        senders
    }
}

impl<T, S, R> Clone for BroadcastChannel<T, S, R>
    where
        T: Send + Clone + 'static,
        S: Send + Sync + Unpin + Clone + Sink<T>,
        R: Unpin + Stream<Item = T>,
{
    fn clone(&self) -> Self {
        let (tx, rx) = (self.ctor)();
        let sender_key = self.senders.write().insert(tx);

        Self {
            senders: self.senders.clone(),
            sender_key,
            receiver: rx,
            ctor: self.ctor.clone(),
        }
    }
}

impl<T, S, R> Drop for BroadcastChannel<T, S, R>
    where
        T: Send + Clone + 'static,
        S: Send + Sync + Unpin + Clone + Sink<T>,
        R: Unpin + Stream<Item = T>,
{
    fn drop(&mut self) {
        self.senders.write().remove(self.sender_key);
    }
}

impl<T, S, R> Debug for BroadcastChannel<T, S, R>
    where
        T: Send + Clone + 'static,
        S: Send + Sync + Unpin + Clone + Debug + Sink<T>,
        R: Unpin + Debug + Stream<Item = T>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BroadcastChannel")
            .field("senders", &self.senders)
            .field("sender_key", &self.sender_key)
            .field("receiver", &self.receiver)
            .finish()
    }
}

impl<T, S, R> Stream for BroadcastChannel<T, S, R>
    where
        T: Send + Clone + 'static,
        S: Send + Sync + Unpin + Clone + Sink<T>,
        R: Unpin + Stream<Item = T>,
{
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        (&mut self.receiver).poll_next_unpin(cx)
    }
}

impl<T, S, R> Sink<T> for &BroadcastChannel<T, S, R>
    where
        T: Send + Clone + 'static,
        S: Send + Sync + Unpin + Clone + Sink<T>,
        R: Unpin + Stream<Item = T>,
{
    type Error = S::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        (*self)
            .senders()
            .iter_mut()
            .map(|(_, sender)| Pin::new(sender).poll_ready(cx))
            .find_map(|poll| match poll {
                Poll::Ready(Err(_)) | Poll::Pending => Some(poll),
                _ => None,
            })
            .or_else(|| Some(Poll::Ready(Ok(()))))
            .unwrap()
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        (*self)
            .senders()
            .iter_mut()
            .map(|(_, sender)| Pin::new(sender).start_send(item.clone()))
            .collect::<Result<_, _>>()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        (*self)
            .senders()
            .iter_mut()
            .map(|(_, sender)| Pin::new(sender).poll_flush(cx))
            .find_map(|poll| match poll {
                Poll::Ready(Err(_)) | Poll::Pending => Some(poll),
                _ => None,
            })
            .or_else(|| Some(Poll::Ready(Ok(()))))
            .unwrap()
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        (*self)
            .senders()
            .iter_mut()
            .map(|(_, sender)| Pin::new(sender).poll_close(cx))
            .find_map(|poll| match poll {
                Poll::Ready(Err(_)) | Poll::Pending => Some(poll),
                _ => None,
            })
            .or_else(|| Some(Poll::Ready(Ok(()))))
            .unwrap()
    }
}

impl<T, S, R> Sink<T> for BroadcastChannel<T, S, R>
    where
        T: Send + Clone + 'static,
        S: Send + Sync + Unpin + Clone + Sink<T>,
        R: Unpin + Stream<Item = T>,
{
    type Error = S::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::poll_ready(Pin::new(&mut &*self), cx)
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        Sink::start_send(Pin::new(&mut &*self), item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::poll_flush(Pin::new(&mut &*self), cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::poll_close(Pin::new(&mut &*self), cx)
    }
}

#[cfg(all(feature = "default-channels", test))]
mod test {
    use super::BroadcastChannel;
    use futures_executor::block_on;
    use futures_util::future::{FutureExt, ready};
    use futures_core::future::Future;
    use futures_util::{StreamExt, SinkExt};
    use futures_channel::mpsc::SendError;

    #[test]
    fn send_next() {
        let mut chan = BroadcastChannel::new();
        block_on(chan.send(&5)).unwrap();
        assert_eq!(block_on(chan.next()), Some(5));
    }

    #[test]
    fn split() {
        // test some of the extension methods from StreamExt and SinkExt
        fn plus_1(num: usize) -> impl Future<Output = Result<usize, SendError>> {
            ready(Ok(num + 1))
        }

        let chan = BroadcastChannel::new();
        let chan_cloned = chan.clone();

        let (sink, stream) = chan.split();
        let mut sink = sink.with(plus_1);
        block_on(sink.send(5)).unwrap();
        block_on(chan_cloned.send(&10)).unwrap();

        assert_eq!(block_on(stream.take(2).collect::<Vec<_>>()), vec![6, 10]);
    }

    #[test]
    fn now_or_never() {
        let fut = async {
            let mut chan = BroadcastChannel::new();
            chan.send(&5i32).await?;
            assert_eq!(chan.next().await, Some(5));

            let mut chan2 = chan.clone();
            chan2.send(&6i32).await?;
            assert_eq!(chan.next().await, Some(6));
            assert_eq!(chan2.next().await, Some(6));
            Ok::<(), futures_channel::mpsc::SendError>(())
        };
        fut.now_or_never().unwrap().unwrap();
    }

    #[test]
    fn try_send() {
        let fut = async {
            let mut chan = BroadcastChannel::with_cap(2);
            chan.try_send(&5i32)?;
            assert_eq!(chan.next().await, Some(5));

            let mut chan2 = chan.clone();
            chan2.try_send(&6i32)?;
            assert_eq!(chan.next().await, Some(6));
            assert_eq!(chan2.next().await, Some(6));
            Ok::<(), futures_channel::mpsc::TrySendError<i32>>(())
        };
        fut.now_or_never().unwrap().unwrap();
    }

    fn assert_impl_send<T: Send>() {}
    fn assert_impl_sync<T: Sync>() {}
    fn assert_val_impl_send<T: Send>(_val: &T) {}
    fn assert_val_impl_sync<T: Sync>(_val: &T) {}

    #[test]
    fn recv_two() {
        let fut = async {
            let mut chan = BroadcastChannel::new();
            chan.send(&5i32).await?;
            assert_eq!(chan.next().await, Some(5));

            let mut chan2 = chan.clone();
            chan2.send(&6i32).await?;
            assert_eq!(chan.next().await, Some(6));
            assert_eq!(chan2.next().await, Some(6));
            Ok::<(), futures_channel::mpsc::SendError>(())
        };
        assert_val_impl_send(&fut);
        assert_val_impl_sync(&fut);
        block_on(fut).unwrap();
    }

    #[test]
    fn send_sync() {
        assert_impl_send::<BroadcastChannel<i32>>();
        assert_impl_sync::<BroadcastChannel<i32>>();
    }
}