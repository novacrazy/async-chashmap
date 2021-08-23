use std::borrow::Borrow;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};

use tokio::sync::{OwnedRwLockWriteGuard, RwLock, RwLockWriteGuard};

pub use hashbrown::hash_map::{DefaultHashBuilder, RawEntryMut};

use rand::Rng;

use crate::{Erased, ReadHandle, WriteHandle};

mod shard;

use shard::IndexedShard;

pub trait AtomicTimestamp {
    /// Create a new timestamp at the given time
    fn now() -> Self;
    /// Update the timestamp to `now` in-place
    fn update(&self);
    fn is_before(&self, other: &Self) -> bool;
}

#[derive(Debug)]
pub struct AtomicInstant(AtomicU64);

impl AtomicTimestamp for AtomicInstant {
    #[inline]
    fn now() -> Self {
        AtomicInstant(AtomicU64::new(quanta::Instant::now().as_u64()))
    }

    #[inline]
    fn update(&self) {
        self.0.store(quanta::Instant::now().as_u64(), Ordering::SeqCst);
    }

    #[inline]
    fn is_before(&self, other: &Self) -> bool {
        self.0.load(Ordering::SeqCst) < other.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
struct TimestampedValue<V, T> {
    value: V,
    timestamp: T,
}

impl<V, T> Clone for TimestampedValue<V, T>
where
    V: Clone,
    T: AtomicTimestamp,
{
    fn clone(&self) -> Self {
        TimestampedValue {
            value: self.value.clone(),
            timestamp: T::now(),
        }
    }
}

type Shard<K, T> = Arc<RwLock<IndexedShard<K, T>>>;

#[derive(Debug)]
pub struct LruCache<K, V, T = AtomicInstant, S = DefaultHashBuilder> {
    hash_builder: S,
    shards: Vec<(Shard<K, TimestampedValue<V, T>>, AtomicUsize)>,
    size: AtomicUsize,
}

impl<K, V, T> LruCache<K, V, T, DefaultHashBuilder> {
    pub fn new(num_shards: usize) -> Self {
        Self::with_hasher(num_shards, DefaultHashBuilder::new())
    }
}

impl<K, V> Default for LruCache<K, V, AtomicInstant, DefaultHashBuilder> {
    fn default() -> Self {
        Self::new(num_cpus::get())
    }
}

impl<K, V, T, S> LruCache<K, V, T, S> {
    pub fn with_hasher(num_shards: usize, hash_builder: S) -> Self {
        LruCache {
            shards: (0..num_shards)
                .into_iter()
                .map(|_| (Arc::new(RwLock::new(IndexedShard::new())), AtomicUsize::new(0)))
                .collect(),
            hash_builder,
            size: AtomicUsize::new(0),
        }
    }
}

impl<K, V, T, S> LruCache<K, V, T, S>
where
    S: Clone,
    K: Clone,
    V: Clone,
    T: AtomicTimestamp,
{
    /// Attempts to duplicate/clone the LruCache. An LruCache cannot be cloned regularly due to internal asynchronous locking.
    pub async fn duplicate(&self) -> Self {
        let mut shards = Vec::with_capacity(self.shards.len());
        let mut size = 0;

        for shard in &self.shards {
            let shard = shard.0.read().await.clone();

            let shard_len = shard.len();
            size += shard_len;
            shards.push((Arc::new(RwLock::new(shard)), AtomicUsize::new(shard_len)));
        }

        LruCache {
            shards,
            hash_builder: self.hash_builder.clone(),
            size: AtomicUsize::new(size),
        }
    }
}

impl<K, V, T, S> LruCache<K, V, T, S>
where
    K: Hash + Eq,
    S: BuildHasher,
    T: AtomicTimestamp,
{
    #[inline]
    pub fn size(&self) -> usize {
        self.size.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub async fn test_size(&self) -> usize {
        let mut size = 0;
        for shard in &self.shards {
            size += shard.0.read().await.len();
        }

        size
    }

    #[inline]
    pub fn hash_builder(&self) -> &S {
        &self.hash_builder
    }

    #[inline]
    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    pub async fn retain<F>(&self, f: F)
    where
        F: Fn(&K, &mut V) -> bool,
    {
        for (shard, _) in &self.shards {
            let mut shard = shard.write().await;

            let len = shard.len();
            shard.retain(|k, tv| f(k, &mut tv.value));

            self.size.fetch_sub(len - shard.len(), Ordering::SeqCst);
        }
    }

    #[inline]
    fn hash_and_shard<Q: ?Sized>(&self, key: &Q) -> (u64, usize)
    where
        Q: Hash + Eq,
    {
        let mut hasher = self.hash_builder.build_hasher();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        (hash, hash as usize % self.shards.len())
    }

    async fn get_mut_raw<Q: ?Sized>(
        &self,
        key: &Q,
    ) -> Option<WriteHandle<impl Erased, TimestampedValue<V, T>>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let (hash, shard_idx) = self.hash_and_shard(key);
        let shard = unsafe { self.shards.get_unchecked(shard_idx).0.clone().write_owned().await };

        OwnedRwLockWriteGuard::try_map(shard, |shard| shard.get_mut(hash, key)).ok()
    }

    async fn get_raw<Q: ?Sized>(&self, key: &Q) -> Option<ReadHandle<impl Erased, TimestampedValue<V, T>>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let (hash, shard_idx) = self.hash_and_shard(key);
        let shard = unsafe { self.shards.get_unchecked(shard_idx).0.clone().read_owned().await };

        ReadHandle::try_map(shard, |shard| shard.get(hash, key)).ok()
    }

    pub async fn peek<Q: ?Sized>(&self, key: &Q) -> Option<ReadHandle<impl Erased, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.get_raw(key)
            .await
            .map(|tv| ReadHandle::map(tv, |tv| &tv.value))
    }

    pub async fn get<Q: ?Sized>(&self, key: &Q) -> Option<ReadHandle<impl Erased, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let tv = self.get_raw(key).await;

        if let Some(ref tv) = tv {
            tv.timestamp.update();
        }

        tv.map(|tv| ReadHandle::map(tv, |tv| &tv.value))
    }

    pub async fn get_mut<Q: ?Sized>(&self, key: &Q) -> Option<WriteHandle<impl Erased, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let mut tv = self.get_mut_raw(key).await;

        // owned ref, don't bother with atomic overhead
        if let Some(ref mut tv) = tv {
            tv.timestamp = T::now();
        }

        tv.map(|tv| WriteHandle::map(tv, |tv| &mut tv.value))
    }

    pub async fn insert(&self, key: K, value: V) -> Option<V> {
        let (hash, shard_idx) = self.hash_and_shard(&key);
        let (locked_shard, shard_size) = unsafe { self.shards.get_unchecked(shard_idx) };

        let mut shard = locked_shard.write().await;

        let value = TimestampedValue {
            value,
            timestamp: T::now(),
        };

        shard
            .insert_full(hash, key, value, || {
                self.size.fetch_add(1, Ordering::SeqCst);
                shard_size.fetch_add(1, Ordering::SeqCst);
            })
            .1
            .map(|tv| tv.value)
    }

    pub async fn remove<Q: ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let (hash, shard_idx) = self.hash_and_shard(&key);
        let (locked_shard, shard_size) = unsafe { self.shards.get_unchecked(shard_idx) };

        let mut shard = locked_shard.write().await;

        match shard.swap_remove_full(hash, key) {
            Some((_, tv)) => {
                self.size.fetch_sub(1, Ordering::SeqCst);
                // know the real size, so just store it
                shard_size.store(shard.len(), Ordering::SeqCst);

                Some(tv.value)
            }
            None => None,
        }
    }

    fn non_empty_shards(&self) -> impl Iterator<Item = &Shard<K, TimestampedValue<V, T>>> {
        self.shards
            .iter()
            .filter_map(|(shard, shard_size)| match shard_size.load(Ordering::SeqCst) {
                0 => None,
                _ => Some(shard),
            })
    }

    pub async fn evict<F>(&self, mut rng: impl Rng, mut predicate: F) -> Vec<(K, V)>
    where
        F: FnMut(&K, &mut V) -> Evict,
    {
        use rand::seq::SliceRandom;

        /* Algorithm:

            Overall: Evict and collect items until the predicate returns false
            The predicate will test the oldest of the two selected items at each iteration

            To start with, collect all non-empty shards, then shuffle them.

            Take one of them (pop) and lock it.

            Then, pick another random shard (pop), and begin selecting two random elements from between those,
            pass the oldest to the predicate, and if the predicate returns true then evict it.

            Swap shard_a and shard_b, then continue. This forms a random-walk of sorts between non-empty shards,
            where it goes from A->B, B->C, C->D, etc.

            Doing a random walk avoids having to reacquire the locks on each shard each iteration.

            When `non_empty` runs empty, refill it with the same method and shuffle it again

        */

        let mut evicted = Vec::new();

        let mut non_empty = Vec::with_capacity(self.shards.len());

        macro_rules! pop_shard {
            () => {
                loop {
                    match non_empty.pop() {
                        Some(shard) => {
                            let shard = shard.write().await;
                            // once locked, check if the shard is actually non-empty
                            if shard.len() > 0 {
                                break Some(shard);
                            }
                        }
                        None => break None,
                    }
                }
            };
        }

        'first: while self.size() > 0 {
            non_empty.extend(self.non_empty_shards());
            non_empty.shuffle(&mut rng);

            let mut shard_a = match pop_shard!() {
                Some(shard) => shard,
                // if we couldn't find an actual non-empty shard, go back to `while size > 0`, and if there is still one, sample it.
                None => continue 'first,
            };

            // handle one-shard case
            if non_empty.is_empty() {
                let res = match shard_a.len() {
                    1 => unsafe {
                        let shard::Bucket {
                            ref key,
                            ref mut value,
                            ..
                        } = shard_a.entries.get_unchecked_mut(0);

                        let res = predicate(key, &mut value.value);

                        if matches!(res, Evict::Continue | Evict::Once) {
                            shard_a.indices.clear();
                            let shard::Bucket { key, value, .. } = shard_a.entries.pop().unwrap();
                            self.size.fetch_sub(1, Ordering::SeqCst);
                            evicted.push((key, value.value));
                        }

                        res
                    },
                    len @ _ => {
                        let elem_a_idx;
                        let mut elem_b_idx;

                        if len > 2 {
                            elem_a_idx = rng.gen_range(0..len);

                            loop {
                                elem_b_idx = rng.gen_range(0..len);

                                if elem_b_idx != elem_a_idx {
                                    break;
                                }
                            }
                        } else {
                            elem_a_idx = 0;
                            elem_b_idx = 1;
                        }

                        unsafe {
                            let ts_a = &shard_a.entries.get_unchecked(elem_a_idx).value.timestamp;
                            let ts_b = &shard_a.entries.get_unchecked(elem_b_idx).value.timestamp;
                            let idx = if ts_a.is_before(ts_b) {
                                elem_a_idx
                            } else {
                                elem_b_idx
                            };

                            let shard::Bucket {
                                ref key,
                                ref mut value,
                                ..
                            } = shard_a.entries.get_unchecked_mut(idx);

                            let res = predicate(key, &mut value.value);

                            if matches!(res, Evict::Continue | Evict::Once) {
                                let (key, value) = shard_a.swap_remove_index_raw(idx);
                                self.size.fetch_sub(1, Ordering::SeqCst);
                                evicted.push((key, value.value));
                            }

                            res
                        }
                    }
                };

                if matches!(res, Evict::Once | Evict::None) {
                    break 'first;
                }

                continue;
            }

            'second: while non_empty.len() > 0 {
                // same method as above to get a non-empty shard
                let mut shard_b = match pop_shard!() {
                    Some(shard) => shard,
                    None => break 'second,
                };

                let shard_a_len = shard_a.len();
                let shard_b_len = shard_b.len();

                let sample_range = shard_a_len + shard_b_len;

                let elem_a_range_idx;
                let mut elem_b_range_idx;

                if sample_range > 2 {
                    elem_a_range_idx = rng.gen_range(0..sample_range);

                    // pick another element
                    loop {
                        elem_b_range_idx = rng.gen_range(0..sample_range);
                        if elem_b_range_idx != elem_a_range_idx {
                            break;
                        }
                    }
                } else {
                    elem_a_range_idx = 0;
                    elem_b_range_idx = 1;
                }

                unsafe {
                    let ts_a = if elem_a_range_idx < shard_a_len {
                        &shard_a.entries.get_unchecked(elem_a_range_idx).value.timestamp
                    } else {
                        &shard_b
                            .entries
                            .get_unchecked(elem_a_range_idx - shard_a_len)
                            .value
                            .timestamp
                    };

                    let ts_b = if elem_b_range_idx < shard_a_len {
                        &shard_a.entries.get_unchecked(elem_b_range_idx).value.timestamp
                    } else {
                        &shard_b
                            .entries
                            .get_unchecked(elem_b_range_idx - shard_a_len)
                            .value
                            .timestamp
                    };

                    let elem_range_idx = if ts_a.is_before(ts_b) {
                        elem_a_range_idx
                    } else {
                        elem_b_range_idx
                    };

                    let (shard, idx) = if elem_range_idx < shard_a_len {
                        (&mut shard_a, elem_range_idx)
                    } else {
                        (&mut shard_b, elem_range_idx - shard_a_len)
                    };

                    let shard::Bucket {
                        ref key,
                        ref mut value,
                        ..
                    } = shard.entries.get_unchecked_mut(idx);

                    let res = predicate(key, &mut value.value);

                    if matches!(res, Evict::Continue | Evict::Once) {
                        let (key, value) = shard.swap_remove_index_raw(idx);
                        self.size.fetch_sub(1, Ordering::SeqCst);
                        evicted.push((key, value.value));
                    }

                    if matches!(res, Evict::None | Evict::Once) {
                        break 'first;
                    }
                }

                // shard_a <- shard_b, unlock shard_a
                drop(std::mem::replace(&mut shard_a, shard_b));
            }
        }

        evicted
    }

    pub async fn evict_many(&self, mut count: usize, rng: impl Rng) -> Vec<(K, V)> {
        count = count.min(self.size());

        if count == 0 {
            return Vec::new();
        }

        let mut cur = count;

        self.evict(rng, |_, _| {
            cur -= 1;

            match cur {
                0 => Evict::Once,
                _ => Evict::Continue,
            }
        })
        .await
    }

    pub async fn evict_one(&self, rng: impl Rng) -> Option<(K, V)> {
        self.evict(rng, |_, _| Evict::Once).await.pop()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evict {
    /// Continue to evict after this item
    Continue,
    /// Evict only this item and then no more
    Once,
    /// Do not evict this item nor any more others
    None,
}
