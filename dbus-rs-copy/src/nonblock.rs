//! Async version of connection.
//!
//! You're probably going to need a companion crate - dbus-tokio - for this connection to make sense,
//! (although you can also just call read_write and process_all at regular intervals).
//! 
//! When async/await is stable, expect more here.

use crate::{Error, Message};
use crate::channel::{MatchingReceiver, Channel, Sender};
use crate::strings::{BusName, Path, Interface, Member};
use crate::arg::{AppendAll, ReadAll, IterAppend};
use crate::message::MatchRule;

use std::sync::{Arc, Mutex};
use std::{future, task, pin, mem};
use std::collections::{HashMap, BTreeMap, VecDeque};
use std::cell::{Cell, RefCell};
use std::task::{Waker, Context};

mod generated_org_freedesktop_notifications;
mod generated_org_freedesktop_dbus;

/// This module contains some standard interfaces and an easy way to call them.
///
/// See the [D-Bus specification](https://dbus.freedesktop.org/doc/dbus-specification.html#standard-interfaces) for more information about these standard interfaces.
///
/// The code was created by dbus-codegen.
pub mod stdintf {
    pub mod org_freedesktop_dbus {
        pub use super::super::generated_org_freedesktop_notifications::*;
        pub use super::super::generated_org_freedesktop_dbus::*;
    }
}

/// Thread local + async Connection 
pub struct LocalConnection {
    channel: Channel,
    waker: RefCell<Option<Waker>>,
    replies: RefCell<HashMap<u32, (Message, <Self as NonblockReply>::F)>>,
    filters: RefCell<BTreeMap<u32, (MatchRule<'static>, Box<dyn FnMut(Message, &LocalConnection) -> bool>)>>,
    filter_nextid: Cell<u32>,
}

impl AsRef<Channel> for LocalConnection {
    fn as_ref(&self) -> &Channel { &self.channel }
}

impl From<Channel> for LocalConnection {
    fn from(x: Channel) -> Self {
        LocalConnection {
            channel: x,
            waker: RefCell::new(None),
            replies: Default::default(),
            filters: Default::default(),
            filter_nextid: Default::default(),
        }
    }
}

impl Sender for LocalConnection {
    fn send(&self, msg: Message) -> Result<u32, ()> {
        let r = self.channel.send(msg);
        if let Some(v) = self.waker.borrow_mut().take() {
            v.wake();
        }
        r
    }
}

/// Async Connection which is Send + Sync.
pub struct SyncConnection {
    channel: Channel,
    waker: Mutex<Option<Waker>>,
    replies: Mutex<HashMap<u32, (Message, <Self as NonblockReply>::F)>>,
    filters: Mutex<(u32, BTreeMap<u32, (MatchRule<'static>, <Self as MatchingReceiver>::F)>)>,
    drop: Mutex<VecDeque<(String, MethodReply<()>)>>,
}

impl AsRef<Channel> for SyncConnection {
    fn as_ref(&self) -> &Channel { &self.channel }
}

impl From<Channel> for SyncConnection {
    fn from(x: Channel) -> Self {
        SyncConnection {
            channel: x,
            waker: Mutex::new(None),
            replies: Default::default(),
            filters: Default::default(),
            drop: Default::default(),
        }
    }
}

impl Sender for SyncConnection {
    fn send(&self, msg: Message) -> Result<u32, ()> {
        let r = self.channel.send(msg);
        if let Ok(v) = &r {
            debug!("send {}", *v);
        }
        // try_lock: It doesn't matter if this method or a concurrent send schedules a wakeup
        if let Ok(mut v) = self.waker.try_lock() {
            if let Some(v) = v.take() {
                v.wake();
            }
        }
        r
    }
}


/// Internal helper trait for async method replies.
pub trait NonblockReply {
    /// Callback type
    type F;
    type R;
    /// Sends a message and calls the callback when a reply is received.
    fn send_with_reply(&self, msg: Message, f: Self::F) -> Result<u32, ()>;
    /// Cancels a pending reply.
    fn cancel_reply(&self, id: u32) -> Option<Self::F>;
    /// Internal helper function that creates a callback.
    fn make_f<G: FnOnce(Message, &Self) + Send + 'static>(g: G) -> Self::F where Self: Sized;
}

impl NonblockReply for LocalConnection {
    type F = Box<dyn FnOnce(Message, &LocalConnection)>;
    // drop list: match_rule string + connection to call "remove_match"
    type R = Box<dyn FnOnce(String, &SyncConnection) + Send>;
    fn send_with_reply(&self, msg: Message, f: Self::F) -> Result<u32, ()> {
        let r = self.channel.send(msg.clone()).map(|x| {
            self.replies.borrow_mut().insert(x, (msg, f));
            x
        });
        if let Some(v) = self.waker.borrow_mut().take() {
            v.wake();
        }
        r
    }
    fn cancel_reply(&self, id: u32) -> Option<Self::F> { self.replies.borrow_mut().remove(&id).and_then(|(_, f)| Some(f)) }
    fn make_f<G: FnOnce(Message, &Self) + Send + 'static>(g: G) -> Self::F { Box::new(g) }
}

impl MatchingReceiver for LocalConnection {
    type F = Box<dyn FnMut(Message, &LocalConnection) -> bool>;
    type R = Box<dyn FnMut(Message, &LocalConnection) -> bool>;
    fn start_receive(&self, m: MatchRule<'static>, f: Self::F) -> u32 {
        let id = self.filter_nextid.get();
        self.filter_nextid.set(id + 1);
        self.filters.borrow_mut().insert(id, (m, f));
        id
    }
    fn stop_receive(&self, id: u32) -> Option<(MatchRule<'static>, Self::F)> {
        self.filters.borrow_mut().remove(&id)
    }
}

impl NonblockReply for SyncConnection {
    type F = Box<dyn FnOnce(Message, &SyncConnection) + Send>;
    // drop list: match_rule string + connection to call "remove_match"
    type R = Box<dyn FnOnce(String, &SyncConnection) + Send>;
    fn send_with_reply(&self, msg: Message, f: Self::F) -> Result<u32, ()> {
        let r = self.channel.send(msg.clone()).map(|x| {
            self.replies.lock().unwrap().insert(x, (msg, f));
            x
        });
        if let Ok(v) = &r {
            debug!("send with reply {}", *v);
        }
        // try_lock: It doesn't matter if this method or a concurrent send schedules a wakeup
        if let Ok(mut v) = self.waker.try_lock() {
            if let Some(v) = v.take() {
                v.wake();
            }
        }
        r
    }
    fn cancel_reply(&self, id: u32) -> Option<Self::F> { self.replies.lock().unwrap().remove(&id).and_then(|(_, f)| Some(f)) }
    fn make_f<G: FnOnce(Message, &Self) + Send + 'static>(g: G) -> Self::F { Box::new(g) }
}

impl MatchingReceiver for SyncConnection {
    type F = Box<dyn FnMut(Message, &Self) -> bool + Send>;
    type R = Box<dyn FnOnce(String, &Self) -> () + Send>;
    fn start_receive(&self, m: MatchRule<'static>, f: Self::F) -> u32 {
        let mut filters = self.filters.lock().unwrap();
        let id = filters.0 + 1;
        filters.0 = id;
        filters.1.insert(id, (m, f));
        id
    }
    fn stop_receive(&self, id: u32) -> Option<(MatchRule<'static>, Self::F)> {
        let mut filters = self.filters.lock().unwrap();
        let mr = filters.1.remove(&id);
        if let Some((mr, old_f)) = mr {
            let mut drop = self.drop.lock().unwrap();

            let p = Proxy::new("org.freedesktop.DBus", "/", self.clone());
            use stdintf::org_freedesktop_dbus::DBus;
            let fut = p.remove_match(&mr.match_str());

            drop.push_back((mr.match_str(), fut));
            Some((mr, old_f))
        } else {
            None
        }
    }
}


/// Internal helper trait, implemented for connections that process incoming messages.
pub trait Process: Sender + AsRef<Channel> {
    /// Dispatches all pending messages, without blocking.
    ///
    /// This is usually called from the reactor only, after read_write.
    /// Despite this taking &self and not "&mut self", it is a logic error to call this
    /// recursively or from more than one thread at a time.
    fn process_all(&self) {
        let c: &Channel = self.as_ref();
        while let Some(msg) = c.pop_message() {
            if let Some(v) = msg.get_reply_serial() {
                debug!("received {}", v);
            }
            self.process_one(msg);
        }
    }

    /// Dispatches a message.
    fn process_one(&self, msg: Message);

    /// Sets the waker that will be used by [`send`] to schedule a dbus socket write.
    fn set_waker(&self, waker: Waker);

    fn drops(&self, ctx: &mut task::Context<'_>);
}

impl Process for LocalConnection {
    fn set_waker(&self, waker: Waker) {
        self.waker.replace(Some(waker));
    }

    fn process_one(&self, msg: Message) {
        if let Some(serial) = msg.get_reply_serial() {
            if let Some((_msg_waiting_reply, callback)) = self.replies.borrow_mut().remove(&serial) {
                callback(msg, self);
                return;
            } else {
                debug!("Got message with no registered reply {}", serial);
            }
        }
        let mut filters = self.filters.borrow_mut();
        if let Some(k) = filters.iter_mut().find(|(_, v)| v.0.matches(&msg)).map(|(k, _)| *k) {
            let mut v = filters.remove(&k).unwrap();
            drop(filters);
            if v.1(msg, &self) {
                let mut filters = self.filters.borrow_mut();
                filters.insert(k, v);
            }
            return;
        }
        if let Some(reply) = crate::channel::default_reply(&msg) {
            let _ = self.send(reply);
        }
    }

    fn drops(&self, ctx: &mut Context<'_>) {
        unimplemented!()
    }
}

impl Process for SyncConnection {
    fn set_waker(&self, waker: Waker) {
        let mut m = self.waker.lock().unwrap();
        *m = Some(waker);
    }

    fn process_one(&self, msg: Message) {
        if let Some(serial) = msg.get_reply_serial() {
            if let Some((_msg_waiting_reply, callback)) = self.replies.lock().unwrap().remove(&serial) {
                callback(msg, self);
                return;
            } else {
                eprintln!("Got message with no registered reply {}", serial);
            }
        }
        let mut filters = self.filters.lock().unwrap();
        if let Some(k) = filters.1.iter_mut().find(|(_, v)| v.0.matches(&msg)).map(|(k, _)| *k) {
            let mut v = filters.1.remove(&k).unwrap();
            drop(filters);
            if v.1(msg, &self) {
                let mut filters = self.filters.lock().unwrap();
                filters.1.insert(k, v);
            }
            return;
        }
        if let Some(reply) = crate::channel::default_reply(&msg) {
            let _ = self.send(reply);
        }
    }

    fn drops(&self, ctx: &mut Context<'_>) {
        use std::future::Future;
        use std::ops::Deref;

        let mut drop = self.drop.lock().unwrap();
        let mut a = drop.drain(..).filter_map(|(match_str, mut method_reply)| {
            match unsafe { pin::Pin::new_unchecked(&mut method_reply) }.poll(ctx) {
                task::Poll::Pending => Some((match_str, method_reply)),
                task::Poll::Ready(_) => {
                    info!("Drop stream complete - {}", match_str);
                    None
                }
            }
        }).collect();
        drop.clear();
        drop.append(&mut a);
    }
}

/// A struct that wraps a connection, destination and path.
///
/// A D-Bus "Proxy" is a client-side object that corresponds to a remote object on the server side. 
/// Calling methods on the proxy object calls methods on the remote object.
/// Read more in the [D-Bus tutorial](https://dbus.freedesktop.org/doc/dbus-tutorial.html#proxies)
#[derive(Clone, Debug)]
pub struct Proxy<'a, C> {
    /// Destination, i e what D-Bus service you're communicating with
    pub destination: BusName<'a>,
    /// Object path on the destination
    pub path: Path<'a>,
    /// Some way to send and/or receive messages, non-blocking.
    pub connection: C,
}

impl<'a, C> Proxy<'a, C> {
    /// Creates a new proxy struct.
    pub fn new<D: Into<BusName<'a>>, P: Into<Path<'a>>>(dest: D, path: P, connection: C) -> Self {
        Proxy { destination: dest.into(), path: path.into(), connection }
    }
}

impl<'a, T, C> Proxy<'a, C>
    where
        T: NonblockReply,
        C: std::ops::Deref<Target=T>
{
    /// Make a method call using typed input argument, returns a future that resolves to the typed output arguments.
    pub fn method_call<'i, 'm, R: ReadAll + 'static, A: AppendAll, I: Into<Interface<'i>>, M: Into<Member<'m>>>(&self, i: I, m: M, args: A)
                                                                                                                -> MethodReply<R> {
        let mut msg = Message::method_call(&self.destination, &self.path, &i.into(), &m.into());
        args.append(&mut IterAppend::new(&mut msg));

        let mr = Arc::new(Mutex::new(MRInner::Neither));
        let mr2 = mr.clone();
        let f = T::make_f(move |msg: Message, _: &T| {
            let mut inner = mr2.lock().unwrap();
            let old = mem::replace(&mut *inner, MRInner::Ready(Ok(msg)));
            drop(inner);
            if let MRInner::Pending(waker) = old { waker.wake() }
        });
        if let Err(_) = self.connection.send_with_reply(msg, f) {
            *mr.lock().unwrap() = MRInner::Ready(Err(Error::new_failed("Failed to send message")));
        }
        MethodReply(mr, Some(Box::new(|msg: Message| { msg.read_all() })))
    }
}

enum MRInner {
    Ready(Result<Message, Error>),
    Pending(task::Waker),
    Neither,
}

/// Future method reply, used while waiting for a method call reply from the server.
pub struct MethodReply<T>(Arc<Mutex<MRInner>>, Option<Box<dyn FnOnce(Message) -> Result<T, Error> + Send + Sync + 'static>>);

impl<T> future::Future for MethodReply<T> {
    type Output = Result<T, Error>;
    fn poll(mut self: pin::Pin<&mut Self>, ctx: &mut task::Context) -> task::Poll<Result<T, Error>> {
        let r = {
            let mut inner = self.0.lock().unwrap();
            let r = mem::replace(&mut *inner, MRInner::Neither);
            if let MRInner::Ready(r) = r { r } else {
                mem::replace(&mut *inner, MRInner::Pending(ctx.waker().clone()));
                return task::Poll::Pending;
            }
        };
        let readfn = self.1.take().expect("Polled MethodReply after Ready");
        task::Poll::Ready(r.and_then(readfn))
    }
}

impl<T: 'static> MethodReply<T> {
    /// Convenience combinator in case you want to post-process the result after reading it
    pub fn and_then<T2>(self, f: impl FnOnce(T) -> Result<T2, Error> + Send + Sync + 'static) -> MethodReply<T2> {
        let MethodReply(inner, first) = self;
        MethodReply(inner, Some({
            let first = first.unwrap();
            Box::new(|r| first(r).and_then(f))
        }))
    }
}


#[test]
fn test_conn_send_sync() {
    fn is_send<T: Send>(_: &T) {}
    fn is_sync<T: Sync>(_: &T) {}
    let c = SyncConnection::from(Channel::get_private(crate::channel::BusType::Session).unwrap());
    is_send(&c);
    is_sync(&c);
}

