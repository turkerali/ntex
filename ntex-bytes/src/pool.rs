#![allow(clippy::type_complexity)]
#[cfg(feature = "mpool")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::task::{Context, Poll};
use std::{cell::Cell, cell::RefCell, fmt, future::Future, pin::Pin, ptr, rc::Rc};

#[cfg(feature = "mpool")]
use futures_core::task::__internal::AtomicWaker;

use crate::{BufMut, BytesMut, BytesVec};

pub struct Pool {
    #[cfg(feature = "mpool")]
    idx: Cell<usize>,
    inner: &'static MemoryPool,
}

#[derive(Copy, Clone)]
pub struct PoolRef(&'static MemoryPool);

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct PoolId(u8);

#[derive(Copy, Clone, Debug)]
pub struct BufParams {
    pub high: u32,
    pub low: u32,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const SPAWNED    = 0b0000_0001;
        const INCREASED  = 0b0000_0010;
    }
}

struct MemoryPool {
    id: PoolId,
    #[cfg(feature = "mpool")]
    waker: AtomicWaker,
    #[cfg(feature = "mpool")]
    waker_alive: AtomicBool,
    #[cfg(feature = "mpool")]
    waiters: RefCell<mpool::Waiters>,
    flags: Cell<Flags>,

    size: AtomicUsize,
    max_size: Cell<usize>,

    window_h: Cell<usize>,
    window_l: Cell<usize>,
    window_idx: Cell<usize>,
    window_waiters: Cell<usize>,
    windows: Cell<[(usize, usize); 10]>,

    // io read/write cache and params
    read_wm: Cell<BufParams>,
    read_cache: RefCell<Vec<BytesVec>>,
    write_wm: Cell<BufParams>,
    write_cache: RefCell<Vec<BytesVec>>,

    spawn: RefCell<Option<Rc<dyn Fn(Pin<Box<dyn Future<Output = ()>>>)>>>,
}

const CACHE_SIZE: usize = 16;

impl PoolId {
    pub const P0: PoolId = PoolId(0);
    pub const P1: PoolId = PoolId(1);
    pub const P2: PoolId = PoolId(2);
    pub const P3: PoolId = PoolId(3);
    pub const P4: PoolId = PoolId(4);
    pub const P5: PoolId = PoolId(5);
    pub const P6: PoolId = PoolId(6);
    pub const P7: PoolId = PoolId(7);
    pub const P8: PoolId = PoolId(8);
    pub const P9: PoolId = PoolId(9);
    pub const P10: PoolId = PoolId(10);
    pub const P11: PoolId = PoolId(11);
    pub const P12: PoolId = PoolId(12);
    pub const P13: PoolId = PoolId(13);
    pub const P14: PoolId = PoolId(14);
    pub const DEFAULT: PoolId = PoolId(15);

    #[inline]
    pub fn pool(self) -> Pool {
        POOLS.with(|pools| Pool {
            #[cfg(feature = "mpool")]
            idx: Cell::new(usize::MAX),
            inner: pools[self.0 as usize],
        })
    }

    #[inline]
    pub fn pool_ref(self) -> PoolRef {
        POOLS.with(|pools| PoolRef(pools[self.0 as usize]))
    }

    #[inline]
    /// Set max pool size
    pub fn set_pool_size(self, size: usize) -> Self {
        self.pool_ref().set_pool_size(size);
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_read_params(self, h: u32, l: u32) -> Self {
        self.pool_ref().set_read_params(h, l);
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_write_params(self, h: u32, l: u32) -> Self {
        self.pool_ref().set_write_params(h, l);
        self
    }

    /// Set future spawn fn
    pub fn set_spawn_fn<T>(self, f: T) -> Self
    where
        T: Fn(Pin<Box<dyn Future<Output = ()>>>) + 'static,
    {
        let spawn: Rc<dyn Fn(Pin<Box<dyn Future<Output = ()>>>)> = Rc::new(f);

        POOLS.with(move |pools| {
            *pools[self.0 as usize].spawn.borrow_mut() = Some(spawn.clone());
        });

        self
    }

    /// Set future spawn fn to all pools
    pub fn set_spawn_fn_all<T>(f: T)
    where
        T: Fn(Pin<Box<dyn Future<Output = ()>>>) + 'static,
    {
        let spawn: Rc<dyn Fn(Pin<Box<dyn Future<Output = ()>>>)> = Rc::new(f);

        POOLS.with(move |pools| {
            for pool in pools.iter().take(15) {
                *pool.spawn.borrow_mut() = Some(spawn.clone());
            }
        });
    }
}

thread_local! {
    static POOLS: [&'static MemoryPool; 16] = [
        MemoryPool::create(PoolId::P0),
        MemoryPool::create(PoolId::P1),
        MemoryPool::create(PoolId::P2),
        MemoryPool::create(PoolId::P3),
        MemoryPool::create(PoolId::P4),
        MemoryPool::create(PoolId::P5),
        MemoryPool::create(PoolId::P6),
        MemoryPool::create(PoolId::P7),
        MemoryPool::create(PoolId::P8),
        MemoryPool::create(PoolId::P9),
        MemoryPool::create(PoolId::P10),
        MemoryPool::create(PoolId::P11),
        MemoryPool::create(PoolId::P12),
        MemoryPool::create(PoolId::P13),
        MemoryPool::create(PoolId::P14),
        MemoryPool::create(PoolId::DEFAULT),
    ];
}

impl PoolRef {
    #[inline]
    /// Get pool id.
    pub fn id(self) -> PoolId {
        self.0.id
    }

    #[inline]
    /// Get `Pool` instance for this pool ref.
    pub fn pool(self) -> Pool {
        Pool {
            #[cfg(feature = "mpool")]
            idx: Cell::new(0),
            inner: self.0,
        }
    }

    #[inline]
    /// Get total number of allocated bytes.
    pub fn allocated(self) -> usize {
        self.0.size.load(Relaxed)
    }

    #[inline]
    pub fn move_in(self, _buf: &mut BytesMut) {
        #[cfg(feature = "mpool")]
        _buf.move_to_pool(self);
    }

    #[inline]
    pub fn move_vec_in(self, _buf: &mut BytesVec) {
        #[cfg(feature = "mpool")]
        _buf.move_to_pool(self);
    }

    #[inline]
    /// Creates a new `BytesMut` with the specified capacity.
    pub fn buf_with_capacity(self, cap: usize) -> BytesMut {
        BytesMut::with_capacity_in(cap, self)
    }

    #[inline]
    /// Creates a new `BytesVec` with the specified capacity.
    pub fn vec_with_capacity(self, cap: usize) -> BytesVec {
        BytesVec::with_capacity_in(cap, self)
    }

    #[doc(hidden)]
    #[inline]
    /// Set max pool size
    pub fn set_pool_size(self, size: usize) -> Self {
        self.0.max_size.set(size);
        self.0.window_waiters.set(0);
        self.0.window_l.set(size);
        self.0.window_h.set(usize::MAX);
        self.0.window_idx.set(0);

        let mut flags = self.0.flags.get();
        flags.insert(Flags::INCREASED);
        self.0.flags.set(flags);

        // calc windows
        let mut l = size;
        let mut h = usize::MAX;
        let mut windows: [(usize, usize); 10] = Default::default();
        windows[0] = (l, h);

        for (idx, item) in windows.iter_mut().enumerate().skip(1) {
            h = l;
            l = size - (size / 100) * idx;
            *item = (l, h);
        }
        self.0.windows.set(windows);

        // release old waiters
        #[cfg(feature = "mpool")]
        {
            let mut waiters = self.0.waiters.borrow_mut();
            while let Some(waker) = waiters.consume() {
                waker.wake();
            }
        }

        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn read_params(self) -> BufParams {
        self.0.read_wm.get()
    }

    #[doc(hidden)]
    #[inline]
    pub fn read_params_high(self) -> usize {
        self.0.read_wm.get().high as usize
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_read_params(self, h: u32, l: u32) -> Self {
        assert!(l < h);
        self.0.read_wm.set(BufParams { high: h, low: l });
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn write_params(self) -> BufParams {
        self.0.write_wm.get()
    }

    #[doc(hidden)]
    #[inline]
    pub fn write_params_high(self) -> usize {
        self.0.write_wm.get().high as usize
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_write_params(self, h: u32, l: u32) -> Self {
        assert!(l < h);
        self.0.write_wm.set(BufParams { high: h, low: l });
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn get_read_buf(self) -> BytesVec {
        if let Some(mut buf) = self.0.read_cache.borrow_mut().pop() {
            buf.clear();
            buf
        } else {
            BytesVec::with_capacity_in(self.0.read_wm.get().high as usize, self)
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Resize read buffer
    pub fn resize_read_buf(self, buf: &mut BytesVec) {
        let (hw, lw) = self.0.write_wm.get().unpack();
        let remaining = buf.remaining_mut();
        if remaining < lw {
            buf.reserve(hw - remaining);
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Release read buffer, buf must be allocated from this pool
    pub fn release_read_buf(self, buf: BytesVec) {
        let cap = buf.capacity();
        let (hw, lw) = self.0.read_wm.get().unpack();
        if cap > lw && cap <= hw {
            let v = &mut self.0.read_cache.borrow_mut();
            if v.len() < CACHE_SIZE {
                v.push(buf);
            }
        }
    }

    #[doc(hidden)]
    #[inline]
    pub fn get_write_buf(self) -> BytesVec {
        if let Some(mut buf) = self.0.write_cache.borrow_mut().pop() {
            buf.clear();
            buf
        } else {
            BytesVec::with_capacity_in(self.0.write_wm.get().high as usize, self)
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Resize write buffer
    pub fn resize_write_buf(self, buf: &mut BytesVec) {
        let (hw, lw) = self.0.write_wm.get().unpack();
        let remaining = buf.remaining_mut();
        if remaining < lw {
            buf.reserve(hw - remaining);
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Release write buffer, buf must be allocated from this pool
    pub fn release_write_buf(self, buf: BytesVec) {
        let cap = buf.capacity();
        let (hw, lw) = self.0.write_wm.get().unpack();
        if cap > lw && cap <= hw {
            let v = &mut self.0.write_cache.borrow_mut();
            if v.len() < CACHE_SIZE {
                v.push(buf);
            }
        }
    }

    #[inline]
    pub(crate) fn acquire(self, _size: usize) {
        #[cfg(feature = "mpool")]
        {
            let prev = self.0.size.fetch_add(_size, Relaxed);
            if self.0.waker_alive.load(Relaxed) {
                self.wake_driver(prev + _size)
            }
        }
    }

    #[inline]
    pub(crate) fn release(self, _size: usize) {
        #[cfg(feature = "mpool")]
        {
            let prev = self.0.size.fetch_sub(_size, Relaxed);
            if self.0.waker_alive.load(Relaxed) {
                self.wake_driver(prev - _size)
            }
        }
    }

    #[cfg(feature = "mpool")]
    fn wake_driver(self, allocated: usize) {
        let l = self.0.window_l.get();
        let h = self.0.window_h.get();
        if allocated < l || allocated > h {
            self.0.waker_alive.store(false, Relaxed);
            self.0.waker.wake();
        }
    }
}

impl Default for PoolRef {
    #[inline]
    fn default() -> PoolRef {
        PoolId::DEFAULT.pool_ref()
    }
}

impl From<PoolId> for PoolRef {
    #[inline]
    fn from(pid: PoolId) -> Self {
        pid.pool_ref()
    }
}

impl<'a> From<&'a Pool> for PoolRef {
    #[inline]
    fn from(pool: &'a Pool) -> Self {
        PoolRef(pool.inner)
    }
}

impl fmt::Debug for PoolRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolRef")
            .field("id", &self.id().0)
            .field("allocated", &self.allocated())
            .finish()
    }
}

impl Eq for PoolRef {}

impl PartialEq for PoolRef {
    fn eq(&self, other: &PoolRef) -> bool {
        ptr::eq(&self.0, &other.0)
    }
}

impl MemoryPool {
    fn create(id: PoolId) -> &'static MemoryPool {
        Box::leak(Box::new(MemoryPool {
            id,
            #[cfg(feature = "mpool")]
            waker: AtomicWaker::new(),
            #[cfg(feature = "mpool")]
            waker_alive: AtomicBool::new(false),
            #[cfg(feature = "mpool")]
            waiters: RefCell::new(mpool::Waiters::new()),
            flags: Cell::new(Flags::empty()),

            size: AtomicUsize::new(0),
            max_size: Cell::new(0),

            window_h: Cell::new(0),
            window_l: Cell::new(0),
            window_waiters: Cell::new(0),
            window_idx: Cell::new(0),
            windows: Default::default(),

            read_wm: Cell::new(BufParams {
                high: 4 * 1024,
                low: 1024,
            }),
            read_cache: RefCell::new(Vec::with_capacity(CACHE_SIZE)),
            write_wm: Cell::new(BufParams {
                high: 4 * 1024,
                low: 1024,
            }),
            write_cache: RefCell::new(Vec::with_capacity(CACHE_SIZE)),
            spawn: RefCell::new(None),
        }))
    }
}

impl BufParams {
    #[inline]
    pub fn unpack(self) -> (usize, usize) {
        (self.high as usize, self.low as usize)
    }
}

impl Clone for Pool {
    #[inline]
    fn clone(&self) -> Pool {
        Pool {
            #[cfg(feature = "mpool")]
            idx: Cell::new(usize::MAX),
            inner: self.inner,
        }
    }
}

impl From<PoolId> for Pool {
    #[inline]
    fn from(pid: PoolId) -> Self {
        pid.pool()
    }
}

impl From<PoolRef> for Pool {
    #[inline]
    fn from(pref: PoolRef) -> Self {
        pref.pool()
    }
}

#[cfg(feature = "mpool")]
impl Drop for Pool {
    fn drop(&mut self) {
        // cleanup waiter
        let idx = self.idx.get();
        if idx != usize::MAX {
            self.inner.waiters.borrow_mut().remove(idx);
        }
    }
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("id", &self.id().0)
            .field("allocated", &self.inner.size.load(Relaxed))
            .field("ready", &self.is_ready())
            .finish()
    }
}

impl Pool {
    #[inline]
    /// Get pool id.
    pub fn id(&self) -> PoolId {
        self.inner.id
    }

    #[inline]
    /// Check if pool is ready
    pub fn is_ready(&self) -> bool {
        #[cfg(feature = "mpool")]
        {
            let idx = self.idx.get();
            if idx != usize::MAX {
                if let Some(mpool::Entry::Occupied(_)) =
                    self.inner.waiters.borrow().entries.get(idx)
                {
                    return false;
                }
            }
        }
        true
    }

    #[inline]
    /// Get `PoolRef` instance for this pool.
    pub fn pool_ref(&self) -> PoolRef {
        PoolRef(self.inner)
    }

    #[inline]
    pub fn poll_ready(&self, _ctx: &mut Context<'_>) -> Poll<()> {
        #[cfg(feature = "mpool")]
        if self.inner.max_size.get() > 0 {
            let window_l = self.inner.window_l.get();
            if window_l == 0 {
                return Poll::Ready(());
            }

            // lower than low
            let allocated = self.inner.size.load(Relaxed);
            if allocated < window_l {
                let idx = self.idx.get();
                if idx != usize::MAX {
                    // cleanup waiter
                    self.inner.waiters.borrow_mut().remove(idx);
                    self.idx.set(usize::MAX);
                }
                return Poll::Ready(());
            }

            // register waiter only if spawn fn is provided
            if let Some(spawn) = &*self.inner.spawn.borrow() {
                let mut flags = self.inner.flags.get();
                let mut waiters = self.inner.waiters.borrow_mut();
                let new = {
                    let idx = self.idx.get();
                    if idx == usize::MAX {
                        self.idx.set(waiters.append(_ctx.waker().clone()));
                        true
                    } else {
                        waiters.update(idx, _ctx.waker().clone())
                    }
                };

                // if memory usage has increased since last window change,
                // block all readyness check. otherwise wake up one existing waiter
                if new {
                    if !flags.contains(Flags::INCREASED) {
                        if let Some(waker) = waiters.consume() {
                            waker.wake()
                        }
                    } else {
                        self.inner
                            .window_waiters
                            .set(self.inner.window_waiters.get() + 1);
                    }
                }

                // start driver task if needed
                if !flags.contains(Flags::SPAWNED) {
                    flags.insert(Flags::SPAWNED);
                    self.inner.flags.set(flags);
                    spawn(Box::pin(mpool::Driver { pool: self.inner }))
                }
                return Poll::Pending;
            }
        }
        Poll::Ready(())
    }
}

#[cfg(feature = "mpool")]
mod mpool {
    use std::{mem, sync::atomic::Ordering::Release, task::Waker};

    use super::*;

    pub(super) struct Driver {
        pub(super) pool: &'static MemoryPool,
    }

    impl Driver {
        pub(super) fn release(&self, waiters_num: usize) {
            let mut waiters = self.pool.waiters.borrow_mut();

            let mut to_release = waiters.occupied_len >> 4;
            if waiters_num > to_release {
                to_release += waiters_num >> 1;
            } else {
                to_release += waiters_num;
            }

            while to_release > 0 {
                if let Some(waker) = waiters.consume() {
                    waker.wake();
                    to_release -= 1;
                } else {
                    break;
                }
            }
        }

        pub(super) fn release_all(&self) {
            let mut waiters = self.pool.waiters.borrow_mut();
            while let Some(waker) = waiters.consume() {
                waker.wake();
            }
        }
    }

    impl Future for Driver {
        type Output = ();

        #[inline]
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let pool = self.as_ref().pool;
            let allocated = pool.size.load(Relaxed);

            let win_l = pool.window_l.get();
            let win_h = pool.window_h.get();

            // allocated size is decreased, release waiters
            if allocated < win_l {
                let mut idx = pool.window_idx.get() + 1;
                let mut waiters = pool.window_waiters.get();
                let windows = pool.windows.get();

                loop {
                    // allocated size decreased more than 10%, release all
                    if idx == 10 {
                        self.release_all();

                        pool.window_l.set(windows[0].0);
                        pool.window_h.set(windows[0].1);
                        pool.window_idx.set(0);
                        pool.window_waiters.set(0);
                        pool.flags.set(Flags::INCREASED);
                        return Poll::Ready(());
                    } else {
                        // release 6% of pending waiters
                        self.release(waiters);

                        if allocated > windows[idx].0 {
                            pool.window_l.set(windows[idx].0);
                            pool.window_h.set(windows[idx].1);
                            pool.window_idx.set(idx);
                            pool.window_waiters.set(0);
                            pool.flags.set(Flags::SPAWNED);
                            break;
                        }
                        idx += 1;
                        waiters = 0;
                    }
                }
            }
            // allocated size is increased
            else if allocated > win_h {
                // reset window
                let idx = pool.window_idx.get() - 1;
                let windows = pool.windows.get();
                pool.window_l.set(windows[idx].0);
                pool.window_h.set(windows[idx].1);
                pool.window_idx.set(idx);
                pool.window_waiters.set(0);
                pool.flags.set(Flags::SPAWNED | Flags::INCREASED);
            }

            // register waker
            pool.waker.register(cx.waker());
            pool.waker_alive.store(true, Release);

            Poll::Pending
        }
    }

    pub(super) struct Waiters {
        pub(super) entries: Vec<Entry>,
        root: usize,
        tail: usize,
        free: usize,
        len: usize,
        occupied_len: usize,
    }

    #[derive(Debug)]
    pub(super) enum Entry {
        Vacant(usize),
        Consumed,
        Occupied(Node),
    }

    #[derive(Debug)]
    pub(super) struct Node {
        item: Waker,
        prev: usize,
        next: usize,
    }

    impl Waiters {
        pub(super) fn new() -> Waiters {
            Waiters {
                entries: Vec::new(),
                root: usize::MAX,
                tail: usize::MAX,
                free: 0,
                len: 0,
                occupied_len: 0,
            }
        }

        fn get_node(&mut self, key: usize) -> &mut Node {
            if let Some(Entry::Occupied(ref mut node)) = self.entries.get_mut(key) {
                return node;
            }
            unreachable!()
        }

        // consume root item
        pub(super) fn consume(&mut self) -> Option<Waker> {
            if self.root != usize::MAX {
                self.occupied_len -= 1;
                let entry =
                    mem::replace(self.entries.get_mut(self.root).unwrap(), Entry::Consumed);

                match entry {
                    Entry::Occupied(node) => {
                        debug_assert!(node.prev == usize::MAX);

                        // last item
                        if self.tail == self.root {
                            self.tail = usize::MAX;
                            self.root = usize::MAX;
                        } else {
                            // remove from root
                            self.root = node.next;
                            if self.root != usize::MAX {
                                self.get_node(self.root).prev = usize::MAX;
                            }
                        }
                        Some(node.item)
                    }
                    _ => unreachable!(),
                }
            } else {
                None
            }
        }

        pub(super) fn update(&mut self, idx: usize, val: Waker) -> bool {
            let entry = self
                .entries
                .get_mut(idx)
                .expect("Entry is expected to exist");
            match entry {
                Entry::Occupied(ref mut node) => {
                    node.item = val;
                    false
                }
                Entry::Consumed => {
                    // append to the tail
                    *entry = Entry::Occupied(Node {
                        item: val,
                        prev: self.tail,
                        next: usize::MAX,
                    });

                    self.occupied_len += 1;
                    if self.root == usize::MAX {
                        self.root = idx;
                    }
                    if self.tail != usize::MAX {
                        self.get_node(self.tail).next = idx;
                    }
                    self.tail = idx;
                    true
                }
                Entry::Vacant(_) => unreachable!(),
            }
        }

        pub(super) fn remove(&mut self, key: usize) {
            if let Some(entry) = self.entries.get_mut(key) {
                // Swap the entry at the provided value
                let entry = mem::replace(entry, Entry::Vacant(self.free));

                self.len -= 1;
                self.free = key;

                match entry {
                    Entry::Occupied(node) => {
                        self.occupied_len -= 1;

                        // remove from root
                        if self.root == key {
                            self.root = node.next;
                            if self.root != usize::MAX {
                                self.get_node(self.root).prev = usize::MAX;
                            }
                        }
                        // remove from tail
                        if self.tail == key {
                            self.tail = node.prev;
                            if self.tail != usize::MAX {
                                self.get_node(self.tail).next = usize::MAX;
                            }
                        }
                    }
                    Entry::Consumed => {}
                    Entry::Vacant(_) => unreachable!(),
                }

                if self.len == 0 {
                    self.entries.truncate(128);
                }
            }
        }

        pub(super) fn append(&mut self, val: Waker) -> usize {
            let idx = self.free;

            self.len += 1;
            self.occupied_len += 1;

            // root points to first entry, append to empty list
            if self.root == usize::MAX {
                self.root = idx;
            }
            // tail points to last entry
            if self.tail != usize::MAX {
                self.get_node(self.tail).next = idx;
            }

            // append item to entries, first free item is not allocated yet
            if idx == self.entries.len() {
                self.entries.push(Entry::Occupied(Node {
                    item: val,
                    prev: self.tail,
                    next: usize::MAX,
                }));
                self.tail = idx;
                self.free = idx + 1;
            } else {
                // entries has enough capacity
                self.free = match self.entries.get(idx) {
                    Some(&Entry::Vacant(next)) => next,
                    _ => unreachable!(),
                };
                self.entries[idx] = Entry::Occupied(Node {
                    item: val,
                    prev: self.tail,
                    next: usize::MAX,
                });
                self.tail = idx;
            }

            idx
        }
    }
}
