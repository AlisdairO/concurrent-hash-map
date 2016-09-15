extern crate crossbeam;
extern crate rand;

use std::sync::Arc;
use std::sync::Mutex;
use std::hash::{Hasher, BuildHasher, Hash};
use std::cmp::min;
use std::sync::atomic::Ordering::{Acquire, Release, Relaxed};
use std::sync::atomic::AtomicUsize;
use crossbeam::mem::epoch::Guard;
use crossbeam::mem::epoch::{self, Atomic, Owned, Shared};
use std::collections::hash_map::RandomState;

const DEFAULT_SEGMENT_COUNT: u32 = 8;
const DEFAULT_CAPACITY: u32 = 16;
const DEFAULT_LOAD_FACTOR: f32 = 0.8;
const MAX_CAPACITY: u32 = 1 << 30;
const MAX_SEGMENT_COUNT: u32 = 1 << 12;
const MIN_LOAD_FACTOR: f32 = 0.2;
const MAX_LOAD_FACTOR: f32 = 1.0;

//TODO: reconsider use of u32/u64/usize for hashes.  ATM this is all u32 for no good reason at all
//TODO: use of the word 'capacity' is overloaded
//TODO: currently have to clone K,V during grow operation, could consider other options (extra indirection)
//TODO: probably going to want to modularise the code better soon :-)
//TODO: iterators, extra useful methods, etc etc
//TODO: fix memory leaks: crossbeam issue #13 allows full resolution, but we can improve the current situation.
//TODO: comments!

/// This is a simple concurrent hash map. It uses a design that's lock free on gets,
/// and locking on inserts/removals. In order to maintain concurrency on insert/removal
/// operations, the map is segmented into several sub-maps, each of which has its own
/// write lock.
/// 
/// This code is currently extremely pre-alpha. Most particularly, it leaks memory on table growth and drop,
/// as well as when using keys or values that (even transitively) use custom Drop implementations.  It should be possible
/// to fix this, but a clean solution will require support for running destructors in crossbeam (see crossbeam
/// issue #13).
/// 
/// For now it may be useful for long lived hashmaps with a relatively steady size, but I
/// don't recommend using it for anything important :-).
pub struct ConcurrentHashMap<K: Eq + Hash + Sync + Clone, V: Sync + Clone, H: BuildHasher> {
    inner: Arc<CHMInner<K, V>>,
    hasher: H,
}

struct CHMInner<K: Eq + Hash + Sync + Clone, V: Sync + Clone> {
    segments: Vec<CHMSegment<K, V>>,
    bit_mask: u32,
    mask_shift_count: u32,
}

struct CHMSegment<K: Eq + Hash + Sync + Clone, V: Sync + Clone> {
    table: Atomic<Vec<Atomic<CHMEntry<K, V>>>>,
    lock: Mutex<()>,
    //todo: the following could just be fenced rather than using atomic types.  Easier
    //not to bother right now, but could certainly change later
    max_capacity: AtomicUsize,
    len: AtomicUsize,
}

struct CHMEntry<K, V> {
    hash: u32,
    key: K,
    value: V,
    next: Atomic<CHMEntry<K, V>>
}

/// Gives a new handle onto the HashMap (much like clone on an Arc).  Does not copy the contents
/// of the map.
impl<K: Eq + Hash + Sync + Clone, V: Sync + Clone, H: BuildHasher + Clone> Clone for ConcurrentHashMap<K, V, H> {
    fn clone(&self) -> ConcurrentHashMap<K, V, H> {
        ConcurrentHashMap{ inner: self.inner.clone(), hasher: self.hasher.clone() }
    }
}


impl<K: Eq + Hash + Sync + Clone, V: Sync + Clone> ConcurrentHashMap<K, V, RandomState> {
    /// Creates a new HashMap with default options (segment count = 8, capacity = 16, load factor = 0.8)
    pub fn new() -> ConcurrentHashMap<K, V, RandomState> {
        ConcurrentHashMap::new_with_options(DEFAULT_CAPACITY, DEFAULT_SEGMENT_COUNT, DEFAULT_LOAD_FACTOR, RandomState::new())
    }
}

impl<K: Eq + Hash + Sync + Clone, V: Sync + Clone, H: BuildHasher> ConcurrentHashMap<K, V, H> {

    /// Creates a new HashMap.  There are three tuning options: capacity, segment count, and load factor.
    /// Load factor and capacity are pretty much what you'd expect: load factor describes the level of table
    /// full-ness of the table at which we grow to avoid excess collisions.  Capacity is simply initial capacity.
    ///
    /// Segment count describes the number of segments the table is divided into.  Each segment is essentially
    /// an autonomous hash table that receives a division of the hash space. Each segment has a write lock,
    /// so only one write can happen per segment at any one time.  Reads can still proceed while writes are in
    /// progress.  For tables with a lot of write activity, a higher segment count will be beneficial.
    ///
    /// Both capacity and segment count must be powers of two - if they're not, each number is raised to
    /// the next power of two. Capacity must be >= segment count, and again is increased as necessary.
    pub fn new_with_options(capacity: u32, segments: u32, load_factor: f32, hasher: H) -> ConcurrentHashMap<K, V, H> {

        let (capacity, segments, load_factor) = Self::check_params(capacity, segments, load_factor);

        ConcurrentHashMap { inner: Arc::new(CHMInner::new(capacity, segments, load_factor)), hasher: hasher }
    }

    // error check params, making sure that capacity and segment counts are powers of two, etc.
    fn check_params(mut capacity: u32, mut segments: u32, mut load_factor: f32) -> (u32, u32, f32) {
        assert!(!load_factor.is_nan());

        segments = min(MAX_SEGMENT_COUNT, segments.checked_next_power_of_two().unwrap());
        if load_factor > MAX_LOAD_FACTOR {
            load_factor = MAX_LOAD_FACTOR;
        }

        if load_factor < MIN_LOAD_FACTOR {
            load_factor = MIN_LOAD_FACTOR;
        }

        capacity = (capacity as f64/load_factor as f64) as u32;

        capacity = min(MAX_CAPACITY, capacity);

        capacity = capacity.checked_next_power_of_two().unwrap();

        if capacity < segments {
            capacity = segments;
        }

        assert!(capacity % segments == 0u32);
        (capacity, segments, load_factor)
    }

    /// Inserts a key-value pair into the map. If the key was already present, the previous
    /// value is returned.  Otherwise, None is returned.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let mut hasher = self.hasher.build_hasher();
        self.inner.insert(key, value, &mut hasher)
    }

    /// Gets a value from the map. If it's not present, None is returned.
    pub fn get(&self, key: K) -> Option<V> {
        let mut hasher = self.hasher.build_hasher();
        self.inner.get(key, &mut hasher)
    }

    /// Removes a key-value pair from the map. If the key was found, the value associated with
    /// it is returned. Otherwise, None is returned.
    pub fn remove(&mut self, key: K) -> Option<V> {
        let mut hasher = self.hasher.build_hasher();
        self.inner.remove(key, &mut hasher)
    }

    /// Returns the size of the map. Note that this size is a best-effort approximation, as
    /// concurrent activity may lead to an inaccurate count.
    pub fn len(&self) -> u32 {
        self.inner.len()
    }

    pub fn entries(&self) -> Vec<(K, V)> {
        self.inner.entries()
    }

}

impl<K: Eq + Hash + Sync + Clone, V: Sync + Clone> CHMInner<K, V> {

    fn new(capacity: u32, seg_count: u32, load_factor: f32) -> CHMInner<K, V> {
        assert!(seg_count % 2 == 0 || seg_count == 1);
        assert!(capacity % seg_count == 0);
        assert!(capacity > 0);
        assert!(load_factor <= MAX_LOAD_FACTOR);
        assert!(load_factor >= MIN_LOAD_FACTOR);

        let per_seg_capacity = capacity / seg_count;

        let (bit_mask, shift_count) = Self::make_segment_bit_mask(seg_count);
        let mut segments = Vec::with_capacity(seg_count as usize);
        
        for _ in 0..seg_count {
            segments.push(CHMSegment::new(per_seg_capacity, load_factor));
        }

        CHMInner { segments: segments, bit_mask: bit_mask, mask_shift_count: shift_count }

    }

    // we mask from the top bits of the hash so as not to bias each segment's distribution
    fn make_segment_bit_mask(seg_count: u32) -> (u32, u32) {
        let mut bit_mask = seg_count - 1;
        let mut shift_count = 0;
        while bit_mask & 0b10000000000000000000000000000000 == 0 && bit_mask != 0 {
            bit_mask <<= 1;
            shift_count += 1;
        }
        (bit_mask, shift_count)
    }

    fn get_segment_from_hash(&self, mut hash: u32) -> u32 {
        hash &= self.bit_mask;
        hash >>= self.mask_shift_count;
        hash
    }

    fn insert<H: Hasher>(&self, key: K, value: V, hasher: &mut H) -> Option<V> {
        let (segment, hash) = self.get_hash_and_segment(&key, hasher);
        self.segments[segment].insert(key, value, hash)
    }

    fn get<H: Hasher>(&self, key: K, hasher: &mut H) -> Option<V> {
        let (segment, hash) = self.get_hash_and_segment(&key, hasher);
        self.segments[segment].get(key, hash)
    }

    fn remove<H: Hasher>(&self, key: K, hasher: &mut H) -> Option<V> {
        let (segment, hash) = self.get_hash_and_segment(&key, hasher);
        self.segments[segment].remove(key, hash)
    }

    fn get_hash_and_segment<H: Hasher>(&self, key: &K, hasher: &mut H) -> (usize, u32) {
        key.hash(hasher);
        let hash = hasher.finish() as u32;
        let segment = self.get_segment_from_hash(hash);
        (segment as usize, hash)
    }

    fn len(&self) -> u32 {
        self.segments.iter().fold(0, |acc, segment| acc + segment.len() as u32)
    }

    fn entries(&self) -> Vec<(K, V)> {
        self.segments.iter().fold(Vec::new(), |mut acc, segment| { acc.extend_from_slice(&segment.entries()); acc })
    }
}

impl<K: Eq + Hash + Sync + Clone, V: Sync + Clone> CHMSegment<K, V> {

    fn new(capacity: u32, load_factor: f32) -> CHMSegment<K, V> {
        debug_assert!(capacity % 2 == 0 || capacity == 1);
        debug_assert!(capacity > 0);

        let max_cap = (capacity as f32 * load_factor) as usize;

        let segment = CHMSegment { table: Atomic::null(), lock: Mutex::new(()), len: AtomicUsize::new(0), max_capacity: AtomicUsize::new(max_cap)};
        //Don't believe we need Release here - new only called when we're constructing the CHM. Releasing should be handled by
        //concurrency primitives transferring the CHM between threads.
        //segment.table.store(Some(Owned::new(Self::new_table(capacity))), Relaxed);
        segment.table.store(Some(Owned::new(Self::new_table(capacity))), Relaxed);

        segment
    }

    fn len(&self) -> usize {
        self.len.load(Relaxed)
    }

    fn insert(&self, key: K, value: V, hash: u32) -> Option<V> {
        //TODO: our mutex-guarded work is not contained by the mutex. Not 1000% sure if this is correct, but for this
        //work we can't have the data stored within the mutex.
        //Able to use relaxed ordering for reads because our mutex will do the necessary acquiring from any
        //previous writes
        let lock_guard = self.lock.lock().expect("Couldn't lock segment mutex");
        let ret = self.insert_inner(key, value, hash, &self.table);
        drop(lock_guard);
        ret
    }

    // Interior function to insert data.  Assumes appropriate locking has already been performed
    // to prevent multiple concurrent writes.
    //
    // NB: Able to use relaxed ordering for reads because our mutex will have done the necessary acquiring from any
    // previous writes
    fn insert_inner(&self, key: K, value: V, hash: u32, s_table: &Atomic<Vec<Atomic<CHMEntry<K, V>>>>) -> Option<V> {
        let guard = epoch::pin();
        
        let table = s_table.load(Relaxed, &guard).expect("Table should have been initialised on creation");
        let hash_bucket = self.get_bucket(hash, table.len() as u32);
        let mut ret = None;

        let mut bucket = &table[hash_bucket as usize];
        let new_node = self.create_new_entry(hash, key, value);
        loop {
            let bucket_data = bucket.load(Relaxed, &guard);
            let entry = match bucket_data {
                None => {
                    // Rather than doing atomic increment, do manual get + set. Atomic increment requires
                    // LOCK CMPXCHG on x86 which is unnecessary, we're single threaded here anyway
                    self.len.store(self.len() + 1, Relaxed);
                    break;
                },
                Some(data) => data
            };

        if entry.hash == new_node.hash && entry.key == new_node.key {
                // Key already exists, let's replace.
                ret = Some(entry.value.clone());
                new_node.next.store_shared(entry.next.load(Relaxed, &guard), Release);
                break;
            } else {
                bucket = &entry.next;
            }
        }
        let old_node = bucket.swap(Some(new_node), Release, &guard);
        if let Some(old_node_content) = old_node {
            unsafe {
                guard.unlinked(old_node_content);
            }
        } else {
            // do we need to resize the table?
            if self.len() >= self.max_capacity.load(Relaxed) {
                self.grow(&guard);
            }
        }
        ret
    }

    //TODO: check we're not bigger than max!
    fn grow(&self, guard: &Guard) {
        self.max_capacity.fetch_add(self.max_capacity.load(Relaxed), Relaxed);
        
        let old_table = self.table.load(Relaxed, &guard).expect("Table should have been initialised on creation");
        
        let new_table = Owned::new(Self::new_table(old_table.len() as u32 * 2));

        for mut old_bucket in old_table.iter() {
            while let Some(entry) = old_bucket.load(Relaxed, guard) {
                let hash_bucket = self.get_bucket(entry.hash, new_table.len() as u32);
                let mut new_bucket = &new_table[hash_bucket as usize];
                while let Some(new_entry) = new_bucket.load(Relaxed, guard) {
                    new_bucket = &new_entry.next;
                };
                let new_entry = self.create_new_entry(entry.hash, entry.key.clone(), entry.value.clone());
                new_bucket.store(Some(new_entry), Release);
                old_bucket = &entry.next;
            }
        }

        self.table.store(Some(new_table), Release);

        unsafe {Self::destroy_table(old_table, guard)};

    }

    unsafe fn destroy_table(table: Shared<Vec<Atomic<CHMEntry<K, V>>>>, guard: &Guard) {
        //unlink old entries and table
        for mut bucket in table.iter() {
            while let Some(entry) = bucket.load(Relaxed, guard) {
                guard.unlinked(entry);
                bucket = &entry.next;
            }
        }
        guard.unlinked(table);
    }

    fn entries(&self) -> Vec<(K, V)> {
        let mut xs = vec![];
        let guard = epoch::pin();
        let table = self.table.load(Relaxed, &guard).unwrap();
        for mut bucket in table.iter() {
            while let Some(entry) = bucket.load(Relaxed, &guard) {
                let e = (entry.key.clone(), entry.value.clone());
                xs.push(e);
                bucket = &entry.next;
            }
        }
        xs
    }

    fn get(&self, key: K, hash: u32) -> Option<V> {
        let guard = epoch::pin();
        let table = self.table.load(Acquire, &guard).expect("Table should have been initialised on creation");
        let hash_bucket = self.get_bucket(hash, table.len() as u32);

        let mut bucket = &table[hash_bucket as usize];

        loop {
            let bucket_data = bucket.load(Acquire, &guard);
            let entry = match bucket_data {
                None => {
                    return None;
                },
                Some(data) => data
            };
            
            if entry.hash == hash && entry.key == key {
                return Some(entry.value.clone());
            } else {
                bucket = &entry.next;
            }
        }
    }

    fn remove(&self, key: K, hash: u32) -> Option<V> {
        let lock_guard = self.lock.lock().unwrap();
        let ret = self.remove_inner(key, hash);
        drop(lock_guard);
        ret
    }

    // Interior function to remove data.  Assumes appropriate locking has already been performed
    // to prevent multiple concurrent writes.
    //
    // NB: Able to use relaxed ordering for reads because our mutex will have done the necessary acquiring from any
    // previous writes
    fn remove_inner(&self, key: K, hash: u32) -> Option<V> {
        let guard = epoch::pin();
        
        let table = self.table.load(Relaxed, &guard).expect("Table should have been initialised on creation");
        let hash_bucket = self.get_bucket(hash, table.len() as u32);

        let mut bucket = &table[hash_bucket as usize];
        loop {
            let bucket_data = bucket.load(Relaxed, &guard);
            let entry = match bucket_data {
                None => {
                    return None;
                },
                Some(data) => data
            };
            
            if entry.hash == hash && entry.key == key {
                bucket.store_shared(entry.next.load(Relaxed, &guard), Release);
                let ret = entry.value.clone();
                self.len.fetch_sub(1, Relaxed);
                unsafe {
                    guard.unlinked(entry);
                }
                return Some(ret);
            } else {
                bucket = &entry.next;
            }
        }
    }


    fn create_new_entry(&self, hash: u32, key: K, value: V) -> Owned<CHMEntry<K, V>> {
        Owned::new(CHMEntry {
            hash: hash,
            key: key,
            value: value,
            next: Atomic::null()
        })
    }

    fn get_bucket(&self, hash: u32, cap: u32) -> u32 {
        hash & (cap - 1)
    }

    // Test only
    #[allow(dead_code)]
    fn table_cap(&self) -> usize {
        let guard = epoch::pin();
        self.table.load(Acquire, &guard).expect("Table should have been initialised on creation").len()
    }

    // Test only
    #[allow(dead_code)]
    fn max_cap(&self) -> usize {
        self.max_capacity.load(Relaxed)
    }

    fn new_table(capacity: u32) -> Vec<Atomic<CHMEntry<K, V>>>{
        let mut v = Vec::with_capacity(capacity as usize);
        for _ in 0..capacity {
            v.push(Atomic::null());
        }
        v
    }

    // Test only, used to hold the lock while we do gets etc.  Checks non-blocking working
    #[allow(dead_code)]
    fn lock_then_do_work<F: Fn()>(&self, work: F) {
        let lock_guard = self.lock.lock();
        work();
        drop (lock_guard);
    }

}

impl<K: Eq + Hash + Sync + Clone, V: Sync + Clone> Drop for CHMSegment<K, V> {
    fn drop(&mut self) {
        let lock_guard = self.lock.lock().expect("Couldn't lock segment mutex");
        let guard = epoch::pin();
        unsafe {Self::destroy_table(self.table.load(Relaxed, &guard).unwrap(), &guard) };
        drop(lock_guard);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use super::CHMSegment;
    use super::CHMInner;
    use std::sync::mpsc::sync_channel;
    use std::thread;
    use std::sync::Arc;
    use std::hash::SipHasher;
    use std::hash::{Hasher, BuildHasher, BuildHasherDefault, Hash};

    #[test]
    fn seg_bit_mask() {
        assert_eq!(CHMInner::<u32,u32>::make_segment_bit_mask(16u32), (0b11110000000000000000000000000000u32, 28));
        assert_eq!(CHMInner::<u32,u32>::make_segment_bit_mask(1u32), (0u32, 0u32));
        assert_eq!(CHMInner::<u32,u32>::make_segment_bit_mask(2u32), (0b10000000000000000000000000000000u32, 31));
        assert_eq!(CHMInner::<u32,u32>::make_segment_bit_mask(1024u32), (0b11111111110000000000000000000000u32, 22));
    }
    
    #[test]
    fn settings_weird_load_factors() {
        validate_chm_settings(16, 16, 32, 32, 1.0, 1.0);
        validate_chm_settings(16, 16, 32, 256, 0.1, 0.2);
        validate_chm_settings(16, 16, 32, 32, 1.1, 1.0);
    }
    
    #[test]
    fn settings_weird_capacities() {
        validate_chm_settings(12, 16, 30, 32, 1.0, 1.0);
        validate_chm_settings(17, 32, 30, 32, 1.0, 1.0);
        validate_chm_settings(17, 32, 10, 32, 1.0, 1.0);
    }
    
    #[test]
    fn settings_weird_segments() {
    }
    
    #[test]
    fn simple_insert_and_get() {
        let mut chm = ConcurrentHashMap::<u32, u32, BuildHasherDefault<SipHasher>>::new_with_options(100, 1, 0.8, BuildHasherDefault::<SipHasher>::default());
        chm.insert(1,100);
        assert_eq!(chm.get(1), Some(100));
    }
    
    #[test]
    fn simple_insert_and_get_other() {
        let mut chm = ConcurrentHashMap::<u32, u32, BuildHasherDefault<SipHasher>>::new_with_options(100, 1, 0.8, BuildHasherDefault::<SipHasher>::default());
        chm.insert(1,100);
        assert_eq!(chm.get(2), None);
    }
    
    #[test]
    fn simple_insert_and_remove() {
        let mut chm = ConcurrentHashMap::<u32, u32, BuildHasherDefault<SipHasher>>::new_with_options(100, 1, 0.8, BuildHasherDefault::<SipHasher>::default());
        chm.insert(1,100);
        assert_eq!(chm.remove(1), Some(100));
        assert_eq!(chm.get(1), None);
    }
    
    #[test]
    fn many_insert_and_remove() {
        let mut chm = ConcurrentHashMap::<u32, u32, BuildHasherDefault<SipHasher>>::new_with_options(16, 1, 1.0, BuildHasherDefault::<SipHasher>::default());
        for i in 0..100 {
            assert_eq!(chm.insert(i,i), None);
        }
    
        assert_eq!(chm.remove(101), None);
    
        for i in 0..100 {
            assert_eq!(chm.remove(i), Some(i));
        }
    
        for i in 0..100 {
            assert_eq!(chm.get(i), None);
        }
    
    }
    
    //TODO try_lock test?
    
    #[test]
    fn many_insert_and_get() {
        let mut chm = ConcurrentHashMap::<u32, u32, BuildHasherDefault<SipHasher>>::new_with_options(16, 1, 1.0, BuildHasherDefault::<SipHasher>::default());
        for i in 0..100 {
            chm.insert(i,i);
        }
        for i in 0..100 {
            assert_eq!(chm.get(i), Some(i));
        }
    }
    
    #[test]
    fn many_insert_and_get_none() {
        let mut chm = ConcurrentHashMap::<u32, u32, BuildHasherDefault<SipHasher>>::new_with_options(16, 1, 1.0, BuildHasherDefault::<SipHasher>::default());
        for i in 0..100 {
            chm.insert(i,i);
        }
        for i in 100..200 {
            assert_eq!(chm.get(i), None);
        }
    }

    #[test]
    fn check_hash_collisions() {
        let chm = CHMSegment::<u32, u32>::new(16, 1.0);
        for i in 0..100 {
            assert_eq!(chm.insert(i,i,0), None);
        }
        for i in 0..100 {
            assert_eq!(chm.insert(i,i+1,0), Some(i));
        }
    }
    
    #[test]
    fn check_len() {
        let chm = CHMSegment::<u32, u32>::new(16, 1.0);
        assert_eq!(chm.max_cap(), 16);
        assert_eq!(chm.table_cap(), 16);
        assert_eq!(chm.len(), 0);
        for i in 0..100 {
            assert_eq!(chm.insert(i,i,i), None);
        }
        assert_eq!(chm.len(), 100);
        assert_eq!(chm.max_cap(), 128);
        assert_eq!(chm.table_cap(), 128);

        for i in 0..100 {
            assert_eq!(chm.insert(i,i+1,i), Some(i));
        }
        assert_eq!(chm.len(), 100);
        assert_eq!(chm.max_cap(), 128);
        assert_eq!(chm.table_cap(), 128);

        for i in 0..100 {
            assert_eq!(chm.remove(i,i), Some(i+1));
        }
        assert_eq!(chm.len(), 0);
        assert_eq!(chm.max_cap(), 128);
        assert_eq!(chm.table_cap(), 128);
        
    }
    
    #[test]
    fn read_segment_while_locked() {
        let chm = Arc::new(CHMSegment::<u32, u32>::new(16, 1.0));
        for i in 0..100 {
            chm.insert(i,i, i);
        }
        let chm_clone = chm.clone();
        let (tx, rx) = sync_channel::<()>(0);
        thread::spawn(move || {
            chm_clone.lock_then_do_work(|| {
                rx.recv().unwrap();
                for i in 0..100 {
                    assert_eq!(chm_clone.insert_inner(i,i+1, i, &chm_clone.table), Some(i));
                }
                rx.recv().unwrap();
                rx.recv().unwrap();
                for i in 0..100 {
                    assert_eq!(chm_clone.remove_inner(i, i), Some(i+1));
                }
                rx.recv().unwrap();
            })
        });
        for i in 0..100 {
            assert_eq!(chm.get(i,i), Some(i));
        }
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        for i in 0..100 {
            assert_eq!(chm.get(i,i), Some(i+1));
        }
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        for i in 0..100 {
            assert_eq!(chm.get(i,i), None);
        }
    }
    
    fn validate_chm_settings(seg_count: u32, expected_seg_count: u32,
                             capacity: u32, expected_capacity: u32,
                             load_factor: f32, expected_load_factor: f32) {
        
        let (capacity_chk, seg_count_chk, load_factor_chk) = ConcurrentHashMap::<u32, u32, BuildHasherDefault<SipHasher>>::check_params(capacity, seg_count, load_factor);
        assert_eq!(seg_count_chk, expected_seg_count);
        assert_eq!(capacity_chk, expected_capacity);
        assert_eq!(load_factor_chk, expected_load_factor);
    }
}
