use crate::api::*;
use super::*;

use std::num::NonZeroU32;
use core::ops::{Deref, DerefMut};
use core::mem::size_of;
use aes_gcm_siv::Aes256GcmSiv;
use std::collections::{HashMap, BinaryHeap, HashSet};
use std::io::{Result, Error, ErrorKind};
use bitfield::bitfield;
use std::cmp::Ordering;

bitfield! {
    #[derive(Copy, Clone, PartialEq, Eq)]
    pub struct DictFlags(u32);
    impl Debug;
    pub valid, set_valid: 0;
}

/// RAM based copy of the dictionary structures on disk. Most of the methods on this function operate on
/// keys within the Dictionary. Operations on the Dictionary itself originate from the containing Basis
/// structure.
pub(crate) struct DictCacheEntry {
    /// Use this to compute the virtual address of the dictionary's location
    /// multiply this by DICT_VSIZE to get at the virtual address. This /could/ be a
    /// NonZeroU32 type as it should never be 0. Maybe that's a thing to fix later on.
    pub(crate) index: u32,
    /// A cache of the keys within the dictionary. If the key does not exist in
    /// the cache, one should consult the on-disk copy, assuming the record is clean.
    pub(crate) keys: HashMap::<String, KeyCacheEntry>,
    /// count of total keys in the dictionary -- may be equal to or larger than the number of elements in `keys`
    pub(crate) key_count: u32,
    /// track the pool of free key indices. Wrapped in a refcell so we can work the index mechanism while updating the keys HashMap
    pub(crate) free_keys: BinaryHeap::<FreeKeyRange>,
    /// hint for when to stop doing a brute-force search for the existence of a key in the disk records.
    /// This field is set to the max count on a new, naive record; and set only upon a sync() or a fill() call.
    pub(crate) last_disk_key_index: u32,
    /// set if synced to disk. should be cleared if the dict is modified, and/or if a subordinate key descriptor is modified.
    pub(crate) clean: bool,
    /// track modification count
    pub(crate) age: u32,
    /// copy of the flags entry on the Dict on-disk
    pub(crate) flags: DictFlags,
    /// small pool data. index corresponds to portion on disk. This structure is built up as the dictionary is
    /// read in, and is the "master" for tracking purposes. We always fill this from index 0 and go up; if a KeySmallPool
    /// goes completely empty, the entry should still exist but indicate that it's got space. Thus if a key was found allocated
    /// to the Nth index position, but the previous N-1 positions are empty, the only way we could have gotten there was if we
    /// had allocated lots of small data, filled upo the pool to the Nth position, and then deleted all of that prior data.
    /// This situation could create pathologies in the memory usage overhead of the small_pool, which until we have a "defrag"
    /// operation for the small pool, we may just have to live with.
    pub(crate) small_pool: Vec<KeySmallPool>,
    /// free space of each small pool element. It's a collection of free space along with the Vec index of the small_pool.
    /// We don't keep the KeySmallPool itself in the small_pool_free directly because it's presumed to be more common
    /// that we want to index the data, than it is to need to ask the question of who has the most space free.
    /// This stays in lock-step with the small_pool data because we do a .pop() to get the target vector from the small_pool_free,
    /// then we modify the pool item, and then we .push() it back into the heap (or if it doesn't fit at all we allocate a new
    /// entry and return the original item plus the new one to the heap).
    pub(crate) small_pool_free: BinaryHeap<KeySmallPoolOrd>,
    /// copy of our AAD, for convenience
    pub(crate) aad: Vec::<u8>,
}
impl DictCacheEntry {
    pub fn new(dict: Dictionary, index: usize, aad: &Vec<u8>) -> DictCacheEntry {
        let mut my_aad = Vec::<u8>::new();
        for &b in aad.iter() {
            my_aad.push(b);
        }
        let mut free_keys = BinaryHeap::<FreeKeyRange>::new();
        free_keys.push(FreeKeyRange{start: dict.free_key_index, run: KEY_MAXCOUNT as u32 - 1});
        DictCacheEntry {
            index: index as u32,
            keys: HashMap::<String, KeyCacheEntry>::new(),
            key_count: dict.num_keys,
            free_keys,
            last_disk_key_index: dict.free_key_index,
            clean: true,
            age: dict.age,
            flags: dict.flags,
            small_pool: Vec::<KeySmallPool>::new(),
            small_pool_free: BinaryHeap::<KeySmallPoolOrd>::new(),
            aad: my_aad,
        }
    }
    /// Populates cache entries, reporting the maximum extent of large alloc data seen so far.
    /// Requires a descriptor for the hardware, and our virtual to physical page mapping.
    /// Does not overwrite existing cache entries, if they already exist -- only loads in ones that are missing.
    /// Todo: condense routines in common with ensure_key_entry() to make it easier to maintain.
    pub fn fill(&mut self, hw: &mut PddbOs, v2p_map: &HashMap::<VirtAddr, PhysPage>, cipher: &Aes256GcmSiv) -> VirtAddr {
        let mut try_entry = 1;
        let mut key_count = 0;
        let mut alloc_top = VirtAddr::new(LARGE_POOL_START).unwrap();

        let mut index_cache = PlaintextCache { data: None, tag: None };
        let mut data_cache = PlaintextCache { data: None, tag: None };
        while try_entry < KEY_MAXCOUNT && key_count < self.key_count {
            // cache our decryption data -- there's about 32 entries per page, and the scan is largely linear/sequential, so this should
            // be a substantial savings in effort.
            // Determine the absolute virtual address of the requested entry. It's written a little weird because
            // DK_PER_VPAGE is 32, which optimizes cleanly and removes an expensive division step
            let req_vaddr = self.index as u64 * DICT_VSIZE + ((try_entry / DK_PER_VPAGE) as u64) * VPAGE_SIZE as u64;
            index_cache.fill(hw, v2p_map, cipher, &self.aad, VirtAddr::new(req_vaddr).unwrap());

            if index_cache.data.is_none() || index_cache.tag.is_none() {
                // somehow we hit a page where nothing was allocated (perhaps it was previously deleted?), or less likely, the data was corrupted. Note the isuse, skip past it.
                log::warn!("Dictionary fill op encountered an unallocated page checking entry {} in the dictionary map. Marking it for re-use.", try_entry);
                try_entry += DK_PER_VPAGE;
            } else {
                let cache_pp = index_cache.tag.as_ref().expect("PP should be in existence, it was already checked...");
                let pp = v2p_map.get(&VirtAddr::new(req_vaddr).unwrap()).expect("dictionary PP should be in existence");
                assert!(cache_pp.page_number() == pp.page_number(), "cache inconsistency error");
                let cache = index_cache.data.as_ref().expect("Cache should be full, it was already checked...");
                let mut keydesc = KeyDescriptor::default();
                let start = size_of::<JournalType>() + (try_entry % DK_PER_VPAGE) * DK_STRIDE;
                for (&src, dst) in cache[start..start + DK_STRIDE].iter().zip(keydesc.deref_mut().iter_mut()) {
                    *dst = src;
                }
                if keydesc.flags.valid() {
                    let mut kcache = KeyCacheEntry {
                        start: keydesc.start,
                        len: keydesc.len,
                        reserved: keydesc.reserved,
                        flags: keydesc.flags,
                        age: keydesc.age,
                        descriptor_index: NonZeroU32::new(try_entry as u32).unwrap(),
                        clean: true,
                        data: None,
                    };
                    let kname = cstr_to_string(&keydesc.name);
                    if !self.keys.contains_key(&kname) {
                        if keydesc.start + keydesc.reserved > alloc_top.get() {
                            // if the key is within the large pool space, note its allocation for the basis overall
                            alloc_top = VirtAddr::new(keydesc.start + keydesc.reserved).unwrap();
                            // nothing else needs to be done -- we don't pre-cache large key data.
                        } else {
                            // try to fill the small key cache entry details
                            self.try_fill_small_key(hw, v2p_map, cipher, &mut data_cache, &mut kcache, &kname);
                        }
                        self.keys.insert(kname, kcache);
                    } else {
                        log::trace!("fill: entry already present {}", kname);
                    }
                    key_count += 1;
                }
                try_entry += 1;
            }
        }
        // note where the scan left off, so we don't have to brute-force it in the future
        self.last_disk_key_index = try_entry as u32;

        // now build the small_pool_free binary heap structure
        self.rebuild_free_pool();

        alloc_top
    }
    /// merges the list of keys in this dict cache entry into a merge_list.
    /// The `merge_list` is used because keys are presented as a union across all open basis.
    pub(crate) fn key_list(&mut self, hw: &mut PddbOs, v2p_map: &HashMap::<VirtAddr, PhysPage>, cipher: &Aes256GcmSiv, merge_list: &mut HashSet<String>) {
        // ensure that the key cache is filled
        if self.keys.len() < self.key_count as usize {
            self.fill(hw, v2p_map, cipher);
        }
        for key in self.keys.keys() {
            merge_list.insert(key.to_string());
        }
    }
    /// Simply ensures we have the description of a key in cache. Only tries to load small key data.
    /// Required by meta-operations on the keys that operate only out of the cache.
    /// This shares a lot of code with the fill() routine -- we should condense the common routines
    /// to make this easier to maintain. Returns false if the disk was searched and no key was found; true
    /// if cache is hot or key was found on search.
    pub(crate) fn ensure_key_entry(&mut self, hw: &mut PddbOs, v2p_map: &mut HashMap::<VirtAddr, PhysPage>, cipher: &Aes256GcmSiv,
        name_str: &str) -> bool {
        // only fill if the key isn't in the cache.
        if !self.keys.contains_key(name_str) {
            log::info!("searching for key {}", name_str);
            let mut data_cache = PlaintextCache { data: None, tag: None };
            let mut try_entry = 1;
            let mut key_count = 0;
            let mut index_cache = PlaintextCache { data: None, tag: None };
            while try_entry < KEY_MAXCOUNT && key_count < self.key_count && try_entry <= self.last_disk_key_index as usize {
                // cache our decryption data -- there's about 32 entries per page, and the scan is largely linear/sequential, so this should
                // be a substantial savings in effort.
                // Determine the absolute virtual address of the requested entry. It's written a little weird because
                // DK_PER_VPAGE is 32, which optimizes cleanly and removes an expensive division step
                let req_vaddr = self.index as u64 * DICT_VSIZE + ((try_entry / DK_PER_VPAGE) as u64) * VPAGE_SIZE as u64;
                index_cache.fill(hw, v2p_map, cipher, &self.aad, VirtAddr::new(req_vaddr).unwrap());

                if index_cache.data.is_none() || index_cache.tag.is_none() {
                    // this case "should not happen" in practice, because the last_disk_key_index would either be correctly set as
                    // short by a dict_add(), or a mount() operation would have limited the extent of the search.
                    // if we are hitting this, that means the last_disk_key_index operator was not managed correctly.
                    log::warn!("expensive search op");
                    try_entry += DK_PER_VPAGE;
                } else {
                    let cache = index_cache.data.as_ref().expect("Cache should be full, it was already checked...");
                    let cache_pp = index_cache.tag.as_ref().expect("PP should be in existence, it was already checked...");
                    let pp = v2p_map.get(&VirtAddr::new(req_vaddr).unwrap()).expect("dictionary PP should be in existence");
                    assert!(cache_pp.page_number() == pp.page_number(), "cache inconsistency error");
                    let mut keydesc = KeyDescriptor::default();
                    let start = size_of::<JournalType>() + (try_entry % DK_PER_VPAGE) * DK_STRIDE;
                    for (&src, dst) in cache[start..start + DK_STRIDE].iter().zip(keydesc.deref_mut().iter_mut()) {
                        *dst = src;
                    }
                    let kname = cstr_to_string(&keydesc.name);
                    if keydesc.flags.valid() {
                        if kname == name_str {
                            let mut kcache = KeyCacheEntry {
                                start: keydesc.start,
                                len: keydesc.len,
                                reserved: keydesc.reserved,
                                flags: keydesc.flags,
                                age: keydesc.age,
                                descriptor_index: NonZeroU32::new(try_entry as u32).unwrap(),
                                clean: true,
                                data: None,
                            };
                            self.try_fill_small_key(hw, v2p_map, cipher, &mut data_cache, &mut kcache, &kname);
                            self.keys.insert(kname, kcache);
                            return true;
                        }
                        key_count += 1;
                    }
                    try_entry += 1;
                }
            }
            false
        } else {
            // the key is in the cache, but is it valid?
            if self.keys.get(name_str).expect("inconsistent state").flags.valid() {
                true
            } else {
                // not valid -- it's an erased key, but waiting to be synced to disk. Return that the key wasn't found.
                false
            }
        }
    }
    fn try_fill_small_key(&mut self, hw: &mut PddbOs, v2p_map: &HashMap::<VirtAddr, PhysPage>, cipher: &Aes256GcmSiv,
        data_cache: &mut PlaintextCache, kcache: &mut KeyCacheEntry, key_name: &str) {
        if let Some(pool_index) = small_storage_index_from_key(&kcache, self.index) {
            // if the key is within the small pool space, create a bookkeeping record for it, and pre-cache its data.
            // generate the index within the small pool based on the address
            while self.small_pool.len() < pool_index + 1 {
                // fill in the pool with blank entries. In general, we should have a low amount of blank entries, but
                // one situation where we could get a leak is if we allocate a large amount of small data, and then delete
                // all but the most recently allocated one, leaving an orphan at a high index, which is then subsequently
                // treated as read-only so none of the subsequent write/update ops would have occassion to move it. This would
                // need to be remedied with a "defrag" operation, but for now, we don't have that.
                let ksp = KeySmallPool::new();
                self.small_pool.push(ksp);
            }
            let ksp = &mut self.small_pool[pool_index];
            ksp.contents.push(key_name.to_string());
            assert!(kcache.reserved >= kcache.len, "Reserved amount is less than length, this is an error!");
            assert!(kcache.reserved <= VPAGE_SIZE as u64, "Reserved amount is not appropriate for the small pool. Logic error in prior PDDB operation!");
            log::info!("avail: {} reserved: {}", ksp.avail, kcache.reserved);
            assert!((ksp.avail as u64) >= kcache.reserved, "Total amount allocated to a small pool chunk is incorrect; suspect logic error in prior PDDB operation!");
            ksp.avail -= kcache.reserved as u16;
            // note: small_pool_free is updated only after all the entries have been read in

            // now grab the *data* referred to by this key. Maybe this is a "bad" idea -- this can really eat up RAM fast to hold
            // all the small pool data right away, but let's give it a try and see how it works. Later on we can always skip this.
            // manage a separate small cache for data blocks, under the theory that small keys tend to be packed together
            let data_vaddr = (kcache.start / VPAGE_SIZE as u64) * VPAGE_SIZE as u64;
            data_cache.fill(hw, v2p_map, cipher, &self.aad, VirtAddr::new(data_vaddr).unwrap());
            if let Some(page) = data_cache.data.as_ref() {
                let start_offset = size_of::<JournalType>() + (kcache.start % VPAGE_SIZE as u64) as usize;
                let mut data = page[start_offset..start_offset + kcache.len as usize].to_vec();
                data.reserve_exact((kcache.reserved - kcache.len) as usize);
                kcache.data = Some(KeyCacheData::Small(
                    KeySmallData {
                        clean: true,
                        data
                    }
                ));
            } else {
                log::error!("Key {}'s data region at pp: {:x?} va: {:x} is unreadable", key_name, data_cache.tag, kcache.start);
            }
        }
    }
    /// Update a key entry. If the key does not already exist, it will create a new one.
    ///
    /// Assume: the caller has called ensure_fast_space_alloc() to make sure there is sufficient space for the inserted key before calling.
    ///
    /// `key_update` will write `data` starting at `offset`, and will grow the record if data
    /// is larger than the current allocation. If `truncate` is false, the existing data past the end of
    /// the `data` written is preserved; if `truncate` is true, the excess data past the end of the written
    /// data is removed.
    ///
    /// For small records, a `key_update` call would just want to replace the entire record, so it would have
    /// an `offset` of 0, `truncate` is true, and the data would be the new data. However, the `offset` and
    /// `truncate` records are particularly useful for updating very large file streams, which can't be
    /// held entirely in RAM.
    ///
    /// Note: it is up to the higher level Basis disambiguation logic to decide the cross-basis update policy: it
    /// could either be to update only the dictionary in the latest open basis, update all dictionaries, or update a
    /// specific dictionary in a named basis. In all of these cases, the Basis resolver will have had to find the
    /// correct DictCacheEntry and issue the `key_update` to it; for multiple updates, then multiple calls to
    /// multiple DictCacheEntry are required.
    pub fn key_update(&mut self, hw: &mut PddbOs, v2p_map: &mut HashMap::<VirtAddr, PhysPage>, cipher: &Aes256GcmSiv,
        name: &str, data: &[u8], offset: usize, alloc_hint:Option<usize>, truncate: bool, large_alloc_ptr: PageAlignedVa) -> Result <PageAlignedVa> {
        self.age = self.age.saturating_add(1);
        self.clean = false;
        if self.ensure_key_entry(hw, v2p_map, cipher, name) {
            let kcache = self.keys.get_mut(name).expect("Entry was assured, but then not there!");
            // the update isn't going to fit in the reserved space, remove it, and re-insert it with an entirely new entry.
            if kcache.reserved < (data.len() + offset) as u64 {
                self.key_remove(hw, v2p_map, cipher, name, false);
                return self.key_update(hw, v2p_map, cipher, name, data, offset, alloc_hint, truncate, large_alloc_ptr);
            }
            // the key exists, *and* there's sufficient space for the data
            if kcache.start < SMALL_POOL_END {
                log::info!("doing data update");
                // it's a small key; note that we didn't consult the *size* of the key to determine its pool type:
                // small-sized keys can still end up in the "large" space if the small pool allocation is exhausted.
                if let KeyCacheData::Small(cache_data) = kcache.data.as_mut().expect("small pool should all have their data 'hot' if the index entry is also in cache") {
                    cache_data.clean = false;
                    // grow the data cache to accommodate the necessary length; this should be efficient because we reserved space when the vector was allocated
                    while cache_data.data.len() < data.len() + offset {
                        cache_data.data.push(0);
                    }
                    for (&src, dst) in data.iter().zip(cache_data.data[offset..].iter_mut()) {
                        *dst = src;
                    }
                    // for now, we ignore "truncate" on a small key
                } else {
                    panic!("Key allocated to small area but its cache data was not of the small type");
                }
                // mark the storage pool entry as dirty, too.
                let pool_index = small_storage_index_from_key(&kcache, self.index).expect("index missing");
                self.small_pool[pool_index].clean = false;
                // note: there is no need to update small_pool_free because the reserved size did not change.
            } else {
                // it's a large key
                if let Some(_kcd) = &kcache.data {
                    unimplemented!("caching is not yet implemented for large data sets");
                } else {
                    kcache.age = kcache.age.saturating_add(1);
                    kcache.clean = false;
                    // 1. handle unaligned start offsets
                    let mut written: usize = 0;
                    if ((kcache.start + offset as u64) % VPAGE_SIZE as u64) != 0 {
                        let start_vpage_addr = ((kcache.start + offset as u64) / VPAGE_SIZE as u64) * VPAGE_SIZE as u64;
                        let pp = v2p_map.get(&VirtAddr::new(start_vpage_addr).unwrap()).expect("large key data allocation missing");
                        let mut pt_data = hw.data_decrypt_page(&cipher, &self.aad, pp).expect("Decryption auth error");
                        for (&src, dst) in data[written..].iter().zip(pt_data[size_of::<JournalType>() + offset..].iter_mut()) {
                            *dst = src;
                            written += 1;
                        }
                        if written < data.len() {
                            assert!((kcache.start + offset as u64 + written as u64) % VPAGE_SIZE as u64 == 0, "alignment algorithm failed");
                        }
                        hw.data_encrypt_and_patch_page(cipher, &self.aad, &mut pt_data, &pp);
                    }
                    // 2. do the rest
                    while written < data.len() {
                        let vpage_addr = ((kcache.start + written as u64 + offset as u64) / VPAGE_SIZE as u64) * VPAGE_SIZE as u64;
                        let pp = v2p_map.get(&VirtAddr::new(vpage_addr).unwrap()).expect("large key data allocation missing");
                        if data.len() - written >= VPAGE_SIZE {
                            // overwrite whole pages without decryption
                            let mut block = [0u8; VPAGE_SIZE + size_of::<JournalType>()];
                            for (&src, dst) in data[written..].iter().zip(block[size_of::<JournalType>()..].iter_mut()) {
                                *dst = src;
                                written += 1;
                            }
                            hw.data_encrypt_and_patch_page(cipher, &self.aad, &mut block, pp);
                        } else {
                            // handle partial trailing pages
                            if let Some(pt_data) = hw.data_decrypt_page(&cipher, &self.aad, pp).as_mut() {
                                for (&src, dst) in data[written..].iter().zip(pt_data[size_of::<JournalType>()..].iter_mut()) {
                                    *dst = src;
                                    written += 1;
                                }
                                hw.data_encrypt_and_patch_page(cipher, &self.aad, pt_data, pp);
                            } else {
                                // page didn't exist, initialize it with 0's and merge the tail end.
                                let mut pt_data = [0u8; VPAGE_SIZE + size_of::<JournalType>()];
                                for (&src, dst) in data[written..].iter().zip(pt_data[size_of::<JournalType>()..].iter_mut()) {
                                    *dst = src;
                                    written += 1;
                                }
                                hw.data_encrypt_and_patch_page(cipher, &self.aad, &mut pt_data, pp);
                            }
                        }
                    }
                    log::info!("data written: {}, data requested to write: {}", written, data.len());
                    assert!(written == data.len(), "algorithm problem -- didn't write all the data we thought we would");
                    // 3. truncate.
                    if truncate {
                        // discard all whole pages after written+offset, and reset the reserved field to the smaller size.
                        let vpage_end_offset = PageAlignedVa::from((written + offset) as u64);
                        if (vpage_end_offset.as_u64() - kcache.start) > kcache.reserved {
                            for vpage in (vpage_end_offset.as_u64()..kcache.start + kcache.reserved).step_by(VPAGE_SIZE) {
                                if let Some(pp) = v2p_map.remove(&VirtAddr::new(vpage).unwrap()) {
                                    hw.fast_space_free(pp);
                                }
                            }
                            kcache.reserved = vpage_end_offset.as_u64() - kcache.start;
                            kcache.clean = false;
                        }
                    }
                }
            }
        } else {
            // key does not exist (or was previously erased) -- create one or replace the erased one.
            // try to fit the key in the small pool first
            if ((data.len() + offset) < SMALL_CAPACITY) && (alloc_hint.unwrap_or(0) < SMALL_CAPACITY) {
                log::info!("creating small key");
                // handle the case that we're a brand new dictionary and no small keys have ever been stored before.
                if self.small_pool.len() == 0 {
                    self.small_pool.push(KeySmallPool::new());
                    self.rebuild_free_pool();
                }
                let pool_candidate = self.small_pool_free.pop().expect("Free pool was allocated & rebuilt, but still empty.");
                let reservation = if alloc_hint.unwrap_or(0) > data.len() + offset {
                    alloc_hint.unwrap_or(0)
                } else {
                    data.len() + offset
                };
                let index = if pool_candidate.avail as usize >= reservation {
                    // it fits in the current candidate slot, use this as the index
                    let ksp = &mut self.small_pool[pool_candidate.index];
                    ksp.contents.push(name.to_string());
                    ksp.avail -= reservation as u16;
                    ksp.clean = false;
                    self.small_pool_free.push(KeySmallPoolOrd { avail: ksp.avail, index: pool_candidate.index });
                    pool_candidate.index
                } else {
                    self.small_pool_free.push(pool_candidate);
                    // allocate a new small pool slot
                    let mut ksp = KeySmallPool::new();
                    ksp.contents.push(name.to_string());
                    ksp.avail -= reservation as u16;
                    ksp.clean = false;
                    // update the free pool with the current candidate
                    // we don't subtract 1 from len because we're about to push the ksp onto the end of the small_pool, consuming it
                    self.small_pool_free.push(KeySmallPoolOrd { avail: ksp.avail, index: self.small_pool.len() });
                    self.small_pool.push(ksp);
                    // the actual location is at len-1 now because we have done the push
                    self.small_pool.len() - 1
                };
                let mut kf = KeyFlags(0);
                kf.set_valid(true);
                kf.set_unresolved(true);
                let mut alloc_data = Vec::<u8>::new();
                for _ in 0..offset {
                    alloc_data.push(0);
                }
                for &b in data {
                    alloc_data.push(b);
                }
                let descriptor_index = if let Some(di) = self.get_free_key_index() {
                    di
                } else {
                    return Err(Error::new(ErrorKind::OutOfMemory, "Ran out of key indices in dictionary"));
                };
                let kcache = KeyCacheEntry {
                    start: SMALL_POOL_START + self.index as u64 * DICT_VSIZE + index as u64 * SMALL_CAPACITY as u64,
                    len: (data.len() + offset) as u64,
                    reserved: reservation as u64,
                    flags: kf,
                    age: 0,
                    descriptor_index,
                    clean: false,
                    data: Some(KeyCacheData::Small(KeySmallData{
                        clean: false,
                        data: alloc_data
                    }))
                };
                self.keys.insert(name.to_string(), kcache);
                self.key_count += 1;
            } else {
                log::info!("creating large key");
                // it didn't fit in the small pool, stick it in the big pool.
                let reservation = PageAlignedVa::from(
                    if alloc_hint.unwrap_or(0) > data.len() + offset {
                        alloc_hint.unwrap_or(0)
                    } else {
                        data.len() + offset
                    });
                let mut kf = KeyFlags(0);
                kf.set_valid(true);
                let descriptor_index = if let Some(di) = self.get_free_key_index() {
                    di
                } else {
                    return Err(Error::new(ErrorKind::OutOfMemory, "Ran out of key indices in dictionary"));
                };
                let kcache = KeyCacheEntry {
                    start: large_alloc_ptr.as_u64(),
                    len: (data.len() + offset) as u64,
                    reserved: reservation.as_u64(),
                    flags: kf,
                    age: 0,
                    descriptor_index,
                    clean: false,
                    data: None, // no caching implemented yet for large keys
                };
                self.keys.insert(name.to_string(), kcache);
                self.key_count += 1;
                // 1. allocate all the pages up to the reservation limit
                for vpage_addr in (large_alloc_ptr.as_u64()..large_alloc_ptr.as_u64() + reservation.as_u64()).step_by(VPAGE_SIZE) {
                    let pp = hw.try_fast_space_alloc().expect("out of disk space");
                    v2p_map.insert(VirtAddr::new(vpage_addr).unwrap(), pp);
                }
                // 2. Recurse. Now, the key should exist, and it should go through the "write the data out" section of the algorithm.
                return self.key_update(hw, v2p_map, cipher, name, data, offset, alloc_hint, truncate, large_alloc_ptr + reservation);
            }
        }
        Ok(large_alloc_ptr)
    }
    #[allow(dead_code)]
    pub fn key_contains(&mut self, name: &str) -> bool {
        self.keys.contains_key(&String::from(name))
    }

    fn rebuild_free_pool(&mut self) {
        self.small_pool_free.clear();
        for (index, ksp) in self.small_pool.iter().enumerate() {
            self.small_pool_free.push(KeySmallPoolOrd{index, avail: ksp.avail})
        }
    }
    /// Used to remove a key from the dictionary. If you call it with a non-existent key,
    /// the routine has no effect, and does not report an error. Small keys are not immediately
    /// overwritten in paranoid mode, but large keys are.
    pub fn key_remove(&mut self, hw: &mut PddbOs, v2p_map: &mut HashMap::<VirtAddr, PhysPage>, cipher: &Aes256GcmSiv,
        name_str: &str, paranoid: bool) {
        // this call makes sure we have a cache entry to operate on.
        self.ensure_key_entry(hw, v2p_map, cipher, name_str);
        let name = String::from(name_str);
        let mut need_rebuild = false;
        let mut need_free_key: Option<u32> = None;
        if let Some(kcache) = self.keys.get_mut(&name) {
            self.clean = false;
            if let Some(small_index) = small_storage_index_from_key(kcache, self.index) {
                // handle the small pool case
                let ksp = &mut self.small_pool[small_index];
                ksp.contents.swap_remove(ksp.contents.iter().position(|s| *s == name)
                    .expect("Small pool did not contain the element we expected"));
                assert!(kcache.reserved <= SMALL_CAPACITY as u64, "error in small key entry size");
                ksp.avail += kcache.reserved as u16;
                assert!(ksp.avail <= SMALL_CAPACITY as u16, "bookkeeping error in small pool capacity");
                ksp.clean = false; // this will also effectively cause the record to be deleted on disk once the small pool data is synchronized
                need_rebuild = true;
                kcache.clean = false;
                kcache.age = kcache.age.saturating_add(1);
                kcache.flags.set_valid(false);
            } else {
                // handle the large pool case
                // mark the entry as invalid and dirty; virtual space is one huge memory leak...
                kcache.clean = false;
                kcache.age = kcache.age.saturating_add(1);
                kcache.flags.set_valid(false);
                // ...but we remove the virtual pages from the page pool, effectively reclaiming the physical space.
                for vpage in kcache.large_pool_vpages() {
                    if let Some(pp) = v2p_map.remove(&vpage) {
                        if paranoid {
                            let mut noise = [0u8; PAGE_SIZE];
                            hw.trng_slice(&mut noise);
                            hw.patch_data(&noise, pp.page_number() * PAGE_SIZE as u32);
                        }
                        hw.fast_space_free(pp);
                    }
                }
            }
            need_free_key = Some(kcache.descriptor_index.get());
        }
        // free up the key index in the dictionary, if necessary
        if let Some(key_to_free) = need_free_key {
            self.put_free_key_index(key_to_free);
        }
        if need_rebuild {
            // no stable "retain" api, so we have to clear the heap and rebuild it https://github.com/rust-lang/rust/issues/71503
            self.rebuild_free_pool();
        }

        // we don't remove the cache entry, because it hasn't been synchronized to disk.
        // at this point:
        //   - in-memory representation will return an entry, but with its valid flag set to false.
        //   - disk still contains a key entry that claims we have a valid key
        // a call to sync is necessary to completely flush things, but, we don't sync every time we remove because it's inefficient.
    }
    /// used to remove a key from the dictionary, syncing 0's to the disk in the key's place
    /// sort of less relevant now that the large keys have a paranoid mode; probably this routine should actually
    /// be a higher-level function that catches the paranoid request and does an "update" of 0's to the key
    /// then does a disk sync and then calls remove
    pub fn key_erase(&mut self, _name: &str) {
        unimplemented!();
    }
    /// estimates the amount of space needed to sync the dict cache. Pass this to ensure_fast_space_alloc() before calling a sync.
    /// estimate can be inaccurate under pathological allocation conditions.
    pub(crate) fn alloc_estimate_small(&self) -> usize {
        let mut data_estimate = 0;
        let mut index_estimate = 0;
        for ksp in &self.small_pool {
            if !ksp.clean {
                for keyname in &ksp.contents {
                    let kce = self.keys.get(keyname).expect("data allocated but no index entry");
                    if kce.flags.unresolved() {
                        data_estimate += SMALL_CAPACITY - ksp.avail as usize;
                        index_estimate += 1;
                    }
                }
            }
        }
        let index_avail = DK_PER_VPAGE - self.keys.len() % DK_PER_VPAGE;
        let index_req = if index_estimate > index_avail {
            ((index_estimate - index_avail) / DK_PER_VPAGE) + 1
        } else {
            0
        };
        (data_estimate / VPAGE_SIZE) + 1 + index_req
    }
    /// Synchronize a given small pool key to disk. Make sure there is adequate space in the fastspace
    /// pool by using self.alloc_estimate_small + hw.ensure_fast_space_alloc. Following this call,
    /// one should call `dict_sync` and `pt_sync` as soon as possible to keep everything consistent.
    ///
    /// Observation: given the dictionary index and the small key pool index, we know exactly
    /// the virtual address of the data pool.
    pub(crate) fn sync_small_pool(&mut self, hw: &mut PddbOs, v2p_map: &mut HashMap::<VirtAddr, PhysPage>, cipher: &Aes256GcmSiv) {
        for (index, entry) in self.small_pool.iter_mut().enumerate() {
            if !entry.clean {
                let pool_vaddr = VirtAddr::new(self.index as u64 * SMALL_POOL_STRIDE + SMALL_POOL_START + index as u64 * SMALL_CAPACITY as u64).unwrap();
                let pp= v2p_map.entry(pool_vaddr).or_insert_with(||
                    hw.try_fast_space_alloc().expect("No free space to allocate small key storage"));
                pp.set_valid(true);

                // WARNING - we don't read back the journal number before loading data into the page!
                // we /could/ do that, but it incurs an expensive full-page decryption when we plan to nuke all the data.
                // I'm a little worried the implementation as-is is going to be too slow, so let's try the abbreviated method
                // and see how it fares. This incurs a risk that we lose data if we have a power outage or panic just after
                // the page is erased but before all the PTEs and pointers are synced.
                //
                // If it turns out this is an issue, here's how you'd fix it:
                //   1. decrypt the old page (if it exists) and extract the journal rev
                //   2. de-allocate the old phys page, returning it to the fastspace pool; it'll likely not be returned on the next step
                //   3. allocate a new page
                //   4. write data to the new page (which increments the old journal rev)
                //   5. sync the page tables
                // This implementation just skips to step 3.
                let mut page = [0u8; VPAGE_SIZE + size_of::<JournalType>()];
                let mut pool_offset = 0;
                // visit the entries in arbitrary order, but back them in optimally
                for key_name in &entry.contents {
                    let kcache = self.keys.get_mut(key_name).expect("data record without index");
                    kcache.start = pool_vaddr.get() + pool_offset as u64;
                    kcache.age = kcache.age.saturating_add(1);
                    kcache.clean = false;
                    kcache.flags.set_unresolved(false);
                    kcache.flags.set_valid(true);
                    if let Some(KeyCacheData::Small(data)) = kcache.data.as_mut() {
                        data.clean = true;
                        for (&src, dst) in data.data.iter()
                        .zip(page[size_of::<JournalType>() + pool_offset..size_of::<JournalType>() + pool_offset + kcache.reserved as usize].iter_mut())
                        {*dst = src;}
                    } else {
                        // we have a rule that all small keys, when cached, also carry their data: there should not be an index without data.
                        panic!("Incorrect data cache type for small key entry.");
                    }
                    pool_offset += kcache.reserved as usize;
                }
                // now commit the sector to disk
                hw.data_encrypt_and_patch_page(cipher, &self.aad, &mut page, &pp);
                entry.clean = true;
            }
        }
        // we now have a bunch of dirty kcache entries. You should call `dict_sync` shortly after this to synchronize those entries to disk.
    }

    /// No data cache to flush yet...large pool caches not implemented!
    pub(crate) fn sync_large_pool(&self) {
    }

    /// Finds the next available slot to store the key metadata (not the data itself). It also
    /// does bookkeeping to bound brute-force searches for keys within the dictionary's index space.
    pub(crate) fn get_free_key_index(&mut self) -> Option<NonZeroU32> {
        if let Some(free_key) = self.free_keys.pop() {
            let index = free_key.start;
            if free_key.run > 0 {
                self.free_keys.push(
                    FreeKeyRange {
                        start: index + 1,
                        run: free_key.run - 1,
                    }
                )
            }
            if index > self.last_disk_key_index {
                // if the new index is outside the currently known set, raise the search extent for the brute-force search
                self.last_disk_key_index = index + 1;
            }
            NonZeroU32::new(index as u32)
        } else {
            log::warn!("Ran out of dict index space");
            None
        }
    }
    /// Returns a key's metadata storage to the index pool.
    pub(crate) fn put_free_key_index(&mut self, index: u32) {
        let free_keys = std::mem::replace(&mut self.free_keys, BinaryHeap::<FreeKeyRange>::new());
        let free_key_vec = free_keys.into_sorted_vec();
        // this is a bit weird because we have three cases:
        // - the new key is more than 1 away from any element, in which case we insert it as a singleton (start = index, run = 0)
        // - the new key is adjacent to exactly once element, in which case we put it either on the top or bottom (merge into existing record)
        // - the new key is adjacent to exactly two elements, in which case we merge the new key and other two elements together, add its length to the new overall run
        let mut skip = false;
        for i in 0..free_key_vec.len() {
            if skip {
                // this happens when we merged into the /next/ record, and we reduced the total number of items by one
                skip = false;
                continue
            }
            match free_key_vec[i].compare_to(i as u32) {
                FreeKeyCases::LessThan => {
                    self.free_keys.push(FreeKeyRange{start: index as u32, run: 0});
                    break;
                }
                FreeKeyCases::LeftAdjacent => {
                    self.free_keys.push(FreeKeyRange{start: index as u32, run: free_key_vec[i].run + 1});
                }
                FreeKeyCases::Within => {
                    log::error!("Double-free error in free_keys()");
                    panic!("Double-free error in free_keys()");
                }
                FreeKeyCases::RightAdjacent => {
                    // see if we should merge to the right
                    if i + 1 < free_key_vec.len() {
                        if free_key_vec[i+1].compare_to(i as u32) == FreeKeyCases::LeftAdjacent {
                            self.free_keys.push(FreeKeyRange{
                                start: free_key_vec[i].start,
                                run: free_key_vec[i].run + free_key_vec[i+1].run + 2
                            });
                            skip = true
                        }
                    } else {
                        self.free_keys.push(FreeKeyRange { start: free_key_vec[i].start, run: free_key_vec[i].run + 1 })
                    }
                }
                FreeKeyCases::GreaterThan => {
                    self.free_keys.push(free_key_vec[i]);
                }
            }
        }
    }
}

/// Derives the index of a Small Pool storage block given the key cache entry and the dictionary index.
/// The index maps into the small_pool array, which itself maps 1:1 onto blocks inside the small pool
/// memory space.
pub(crate) fn small_storage_index_from_key(kcache: &KeyCacheEntry, dict_index: u32) -> Option<usize> {
    let storage_base = dict_index as u64 * SMALL_POOL_STRIDE + SMALL_POOL_START;
    let storage_end = storage_base + SMALL_POOL_STRIDE;
    if kcache.start >= storage_base && (kcache.start + kcache.reserved) < storage_end {
        let index_base = kcache.start - storage_base;
        Some(index_base as usize / SMALL_CAPACITY)
    } else {
        None
    }
}
#[derive(Debug)]
/// On-disk representation of the dictionary header. This structure is mainly for archival/unarchival
/// purposes. To "functionalize" a stored disk entry, it needs to be deserialized into a DictionaryCacheEntry.
#[repr(C, align(8))]
pub(crate) struct Dictionary {
    /// Reserved for flags on the record entry
    pub(crate) flags: DictFlags,
    /// Access count to the dicitionary
    pub(crate) age: u32,
    /// Number of keys in the dictionary
    pub(crate) num_keys: u32,
    /// Free index starting space. While this is a derived parameter, its value is recorded to avoid
    /// an expensive, long search operation during the creation of a dictionary cache record.
    pub(crate) free_key_index: u32,
    /// Name. Length should pad out the record to exactly 127 bytes.
    pub(crate) name: [u8; DICT_NAME_LEN],
}
impl Default for Dictionary {
    fn default() -> Dictionary {
        let mut flags = DictFlags(0);
        flags.set_valid(true);
        Dictionary { flags, age: 0, num_keys: 0, free_key_index: 1, name: [0; DICT_NAME_LEN] }
    }
}
impl Deref for Dictionary {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Dictionary as *const u8, core::mem::size_of::<Dictionary>())
                as &[u8]
        }
    }
}
impl DerefMut for Dictionary {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(self as *mut Dictionary as *mut u8, core::mem::size_of::<Dictionary>())
                as &mut [u8]
        }
    }
}

/// This structure "enforces" the 127-byte stride of dict/key vpage entries
#[derive(Copy, Clone)]
pub(crate) struct DictKeyEntry {
    pub(crate) data: [u8; DK_STRIDE],
}
impl Default for DictKeyEntry {
    fn default() -> DictKeyEntry {
        DictKeyEntry {
            data: [0; DK_STRIDE]
        }
    }
}

/// This structure helps to bookkeep which slices within a DictKey virtual page need to be updated
pub(crate) struct DictKeyVpage {
    pub(crate) elements: [Option<DictKeyEntry>; VPAGE_SIZE / DK_STRIDE],
}
impl<'a> Default for DictKeyVpage {
    fn default() -> DictKeyVpage {
        DictKeyVpage {
            elements: [None; VPAGE_SIZE / DK_STRIDE],
        }
    }
}


#[derive(PartialEq, Eq)]
pub(crate) enum FreeKeyCases {
    LeftAdjacent,
    RightAdjacent,
    Within,
    LessThan,
    GreaterThan,
}
#[derive(Eq, Copy, Clone)]
pub(crate) struct FreeKeyRange {
    /// This index should be free
    pub(crate) start: u32,
    /// Additional free keys after the start one. Run = 0 means just the start key is free, and the
    /// next one should be used. Run = 2 means {start, start+1} are free, etc.
    pub(crate) run: u32,
}
impl FreeKeyRange {
    pub(crate) fn compare_to(&self, index: u32) -> FreeKeyCases {
        if self.start > 1 && index < self.start - 1 {
            FreeKeyCases::LessThan
        } else if self.start > 0 && index == self.start - 1 {
            FreeKeyCases::LeftAdjacent
        } else if index >= self.start && index <= self.start + self.run {
            FreeKeyCases::Within
        } else if index == self.start + self.run + 1 {
            FreeKeyCases::RightAdjacent
        } else {
            FreeKeyCases::GreaterThan
        }
    }
}
impl Ord for FreeKeyRange {
    fn cmp(&self, other: &Self) -> Ordering {
        // note the reverse order -- so we can sort as a "min-heap"
        other.start.cmp(&self.start)
    }
}
impl PartialOrd for FreeKeyRange {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for FreeKeyRange {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start
    }
}

/// stashed copy of a decrypted page. The copy here must always match
/// what's actually on disk; do not mutate it and expect it to sync with the disk.
/// Remember to invalidate this if the data are
/// This is stored with the journal number on top.
/// What the four possibilities of cache vs pp mean:
/// Some(cache) & Some(cache_pp) -> valid cache and pp
/// None(cache) & Some(cache_pp) -> the page was allocated; but never used, or was erased (it's free for you to use it); alternately, it was corrupted
/// Some(cache) & None(cache_pp) -> invalid, internal error
/// None(cache) & None(cache_pp) -> the basis mapping didn't exist: we've never requested this page before.
pub(crate) struct PlaintextCache {
    /// a page of data, stored with the Journal rev on top
    pub(crate) data: Option<Vec::<u8>>,
    /// the page the cache corresponds to
    pub(crate) tag: Option<PhysPage>,
}
impl PlaintextCache {
    pub fn fill(&mut self, hw: &mut PddbOs, v2p_map: &HashMap::<VirtAddr, PhysPage>, cipher: &Aes256GcmSiv, aad: &[u8],
        req_vaddr: VirtAddr
    ) {
        if let Some(pp) = v2p_map.get(&req_vaddr) {
            let mut fill_needed = false;
            if let Some(tag) = self.tag {
                if tag.page_number() != pp.page_number() {
                    fill_needed = true;
                }
            } else if self.tag.is_none() {
                fill_needed = true;
            }
            if fill_needed {
                self.data = hw.data_decrypt_page(&cipher, &aad, pp);
                self.tag = Some(*pp);
            }
        } else {
            self.data = None;
            self.tag = None;
        }
    }
}