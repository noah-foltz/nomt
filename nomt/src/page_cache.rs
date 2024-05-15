use crate::{
    page_region::PageRegion,
    rw_pass_cell::{ReadPass, RegionContains, RwPassCell, RwPassDomain, WritePass},
    store::{Store, Transaction},
    Options,
};
use bitvec::prelude::*;
use crossbeam_queue::SegQueue;
use dashmap::{mapref::entry::Entry, DashMap};
use fxhash::FxBuildHasher;
use nomt_core::{
    page::DEPTH,
    page_id::{PageId, ROOT_PAGE_ID},
    trie::{LeafData, Node},
    trie_pos::{ChildNodeIndices, TriePosition},
};
use parking_lot::{Condvar, Mutex, MutexGuard, RwLock, RwLockWriteGuard};
use std::{cell::RefCell, fmt, mem, sync::Arc};
use threadpool::ThreadPool;

use self::cache_advisor::CacheAdvisor;

mod cache_advisor;
mod dll;

// Total number of nodes stored in one Page. It depends on the `DEPTH`
// of the rootless sub-binary tree stored in a page following this formula:
// (2^(DEPTH + 1)) - 2
pub const NODES_PER_PAGE: usize = (1 << DEPTH + 1) - 2;

/// Within the page, we also store a bitfield indicating whether leaf data is stored at a particular
/// location. This bitfield has '1' bits set for leaf data and '0' bits set for nodes.
pub const LEAF_META_BITFIELD_SLOT: usize = NODES_PER_PAGE;
/// This is the offset of the leaf meta bitfield in the page data.
pub const LEAF_DATA_BITFIELD_OFF: usize = LEAF_META_BITFIELD_SLOT * 32;

const PAGE_CACHE_SZ: usize = 1024 * 1024;

struct PageData {
    data: RwPassCell<Option<Vec<u8>>, PageId>,
}

impl PageData {
    /// Creates a page with the given data.
    fn pristine_with_data(domain: &RwPassDomain, page_id: PageId, data: Vec<u8>) -> Self {
        Self {
            data: domain.protect_with_id(Some(data), page_id),
        }
    }

    /// Creates an empty page.
    fn pristine_empty(domain: &RwPassDomain, page_id: PageId) -> Self {
        Self {
            data: domain.protect_with_id(None, page_id),
        }
    }

    fn node(&self, read_pass: &ReadPass<impl RegionContains<PageId>>, index: usize) -> Node {
        assert!(index < NODES_PER_PAGE, "index out of bounds");
        let data = self.data.read(read_pass);
        if let Some(data) = &*data {
            let start = index * 32;
            let end = start + 32;
            let mut node = [0; 32];
            node.copy_from_slice(&data[start..end]);
            node
        } else {
            Node::default()
        }
    }

    fn set_node(
        &self,
        write_pass: &mut WritePass<impl RegionContains<PageId>>,
        index: usize,
        node: Node,
    ) {
        assert!(index < NODES_PER_PAGE, "index out of bounds");
        let mut data = self.data.write(write_pass);
        let data = data.get_or_insert_with(|| vec![0; 4096]);
        let start = index * 32;
        let end = start + 32;
        data[start..end].copy_from_slice(&node);

        // clobbering the leaf data bit here means that if a user is building a tree
        // upwards (most algorithms do), they can overwrite the leaf children and then overwrite
        // the leaf without worrying about deleting the node they've just written.
        leaf_data_bits(data).set(index, false);
    }

    fn set_leaf_data(
        &self,
        write_pass: &mut WritePass<impl RegionContains<PageId>>,
        children: ChildNodeIndices,
        leaf_data: LeafData,
    ) {
        let left_index = children.left();
        assert!(left_index < NODES_PER_PAGE - 1, "index out of bounds");
        let mut data = self.data.write(write_pass);
        let data = data.get_or_insert_with(|| vec![0; 4096]);
        {
            let leaf_meta = leaf_data_bits(data);
            leaf_meta.set(left_index, true);
            leaf_meta.set(left_index + 1, true);
        }
        let start = left_index * 32;
        let end = start + 64;

        leaf_data.encode_into(&mut data[start..end]);
    }

    fn clear_leaf_data(
        &self,
        write_pass: &mut WritePass<impl RegionContains<PageId>>,
        children: ChildNodeIndices,
    ) {
        let left_index = children.left();
        assert!(left_index < NODES_PER_PAGE - 1, "index out of bounds");

        let mut data = self.data.write(write_pass);
        let data = data.get_or_insert_with(|| vec![0; 4096]);
        let (overwrite_l, overwrite_r) = {
            let leaf_meta = leaf_data_bits(data);
            (
                leaf_meta.replace(left_index, false),
                leaf_meta.replace(left_index + 1, false),
            )
        };

        let start = left_index * 32;
        let l_end = start + 32;
        let r_end = l_end + 32;

        if overwrite_l {
            data[start..l_end].copy_from_slice(&[0u8; 32]);
        }
        if overwrite_r {
            data[l_end..r_end].copy_from_slice(&[0u8; 32]);
        }
    }
}

fn leaf_data_bits(page: &mut [u8]) -> &mut BitSlice<u8, Msb0> {
    page[LEAF_DATA_BITFIELD_OFF..][..32].view_bits_mut::<Msb0>()
}

/// Checks whether a page is empty.
pub fn page_is_empty(page: &[u8]) -> bool {
    // 1. we assume the top layer of nodes are kept at index 0 and 1, respectively, and this
    //    is packed as the first two 32-byte slots.
    // 2. if both are empty, then the whole page is empty. this is because internal nodes
    //    with both children as terminals are not allowed to exist.
    &page[..64] == [0u8; 64].as_slice()
}

/// Tracks which nodes have changed within a page.
#[derive(Debug, Default, Clone)]
pub struct PageDiff {
    /// A bitfield indicating the number of updated slots
    updated_slots: BitArray<[u64; 2], Lsb0>,
}

impl PageDiff {
    /// Note that some 32-byte slot in the page data has changed.
    /// The acceptable range is 0..=LEAF_META_BITFIELD_SLOT
    pub fn set_changed(&mut self, slot_index: usize) {
        assert!(slot_index <= LEAF_META_BITFIELD_SLOT);
        self.updated_slots.set(slot_index, true);
    }
}

enum PageState {
    Inflight(Arc<InflightFetch>),
    Cached(Arc<PageData>),
}

/// A handle to the page.
///
/// Can be cloned cheaply.
#[derive(Clone)]
pub struct Page {
    inner: Arc<PageData>,
}

impl Page {
    /// Read out the node at the given index.
    pub fn node(&self, read_pass: &ReadPass<impl RegionContains<PageId>>, index: usize) -> Node {
        self.inner.node(read_pass, index)
    }

    /// Write the node at the given index.
    pub fn set_node(
        &self,
        write_pass: &mut WritePass<impl RegionContains<PageId>>,
        index: usize,
        node: Node,
    ) {
        self.inner.set_node(write_pass, index, node);
    }

    /// Write leaf data at two positions under a leaf node.
    pub fn set_leaf_data(
        &self,
        write_pass: &mut WritePass<impl RegionContains<PageId>>,
        children: ChildNodeIndices,
        leaf_data: LeafData,
    ) {
        self.inner.set_leaf_data(write_pass, children, leaf_data)
    }

    /// Clear leaf data at two child positions.
    pub fn clear_leaf_data(
        &self,
        write_pass: &mut WritePass<impl RegionContains<PageId>>,
        children: ChildNodeIndices,
    ) {
        self.inner.clear_leaf_data(write_pass, children)
    }
}

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Page").finish()
    }
}

/// Given a trie position and a current page corresponding to that trie position (None at the root)
/// along with a function for synchronously loading a new page, get the page and indices where the
/// leaf data for a leaf at `trie_pos` should be stored.
pub fn locate_leaf_data(
    trie_pos: &TriePosition,
    current_page: Option<&(PageId, Page)>,
    load: impl Fn(PageId) -> Page,
) -> (Page, PageId, ChildNodeIndices) {
    match current_page {
        None => {
            assert!(trie_pos.is_root());
            let page = load(ROOT_PAGE_ID);
            (page, ROOT_PAGE_ID, ChildNodeIndices::from_left(0))
        }
        Some((ref page_id, ref page)) => {
            let depth_in_page = trie_pos.depth_in_page();
            if depth_in_page == DEPTH {
                let child_page_id = page_id.child_page_id(trie_pos.child_page_index()).unwrap();
                let child_page = load(child_page_id.clone());
                (child_page, child_page_id, ChildNodeIndices::from_left(0))
            } else {
                (page.clone(), page_id.clone(), trie_pos.child_node_indices())
            }
        }
    }
}

/// Represents a fetch that is currently in progress.
///
/// A fetch can be in one of the following states:
/// - Scheduled. The fetch is scheduled for execution but has not started yet.
/// - Started. The db request has been issued but still waiting for the response.
/// - Completed. The page has been fetched and the waiters are notified with the fetched page.
struct InflightFetch {
    page: Mutex<Option<Page>>,
    ready: Condvar,
}

impl InflightFetch {
    fn new() -> Self {
        Self {
            page: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    /// Notifies all the waiting parties that the page has been fetched and destroys this handle.
    fn complete_and_notify(&self, p: Page) {
        let mut page = self.page.lock();
        if page.is_some() {
            return;
        }
        *page = Some(p);
        self.ready.notify_all();
    }

    /// Waits until the page is fetched and returns it.
    fn wait(&self) -> Page {
        let mut page = self.page.lock();
        loop {
            if let Some(ref page) = &*page {
                return page.clone();
            }
            self.ready.wait(&mut page);
        }
    }
}

enum PageStore {
    Real(Store),
    #[cfg(test)]
    Mock(DashMap<PageId, Vec<u8>>),
}

impl PageStore {
    fn load_page(&self, page_id: PageId) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            PageStore::Real(s) => s.load_page(page_id),
            #[cfg(test)]
            PageStore::Mock(map) => Ok(map.get(&page_id).map(|x| x.clone())),
        }
    }
}

/// The page-cache provides an in-memory layer between the user and the underlying DB.
/// It stores full pages and can be shared between threads.
#[derive(Clone)]
pub struct PageCache {
    shared: Arc<Shared>,
    /// The cache advisor. Tracks which pages are accessed and suggests pages for eviction.
    ///
    /// This structure is deliberately not placed in the shared state, as every handle has its own
    /// advisor. This is because the advisor has some thread-local state as optimization, although
    /// the most of its state is shared.
    ///
    /// The justification for the refcell is that we don't want to propagate mutability to the
    /// users of the cache.
    ev: RefCell<CacheAdvisor>,
}

struct Shared {
    page_rw_pass_domain: RwPassDomain,
    store: PageStore,
    /// The thread pool used for fetching pages from the store.
    ///
    /// Used for limiting the number of concurrent page fetches.
    fetch_tp: ThreadPool,
    /// The pages loaded from the store, possibly dirty.
    cached: DashMap<PageId, PageState, FxBuildHasher>,
    /// This lock must be held during the update phase. This is a simple way to ensure that eviction
    /// can only happen not during the update phase.
    update_rwl: RwLock<()>,
    /// The eviction queue.
    ///
    /// This is a queue of pages that are candidates for eviction.
    evict_queue: SegQueue<PageId>,
}

impl PageCache {
    /// Create a new `PageCache` atop the provided [`Store`].
    pub fn new(store: Store, o: &Options) -> Self {
        let fetch_tp = threadpool::Builder::new()
            .num_threads(o.fetch_concurrency)
            .thread_name("nomt-page-fetch".to_string())
            .build();
        Self {
            shared: Arc::new(Shared {
                page_rw_pass_domain: RwPassDomain::new(),
                cached: DashMap::with_hasher(FxBuildHasher::default()),
                store: PageStore::Real(store),
                fetch_tp,
                update_rwl: RwLock::new(()),
                evict_queue: SegQueue::new(),
            }),
            ev: RefCell::new(CacheAdvisor::new(PAGE_CACHE_SZ, 20)),
        }
    }

    /// Create a new `PageCache` with a mocked store for testing.
    #[cfg(test)]
    pub fn new_mocked(o: &Options) -> Self {
        let fetch_tp = threadpool::Builder::new()
            .num_threads(o.fetch_concurrency)
            .thread_name("nomt-page-fetch".to_string())
            .build();

        Self {
            shared: Arc::new(Shared {
                page_rw_pass_domain: RwPassDomain::new(),
                cached: DashMap::with_hasher(FxBuildHasher::default()),
                store: PageStore::Mock(DashMap::new()),
                fetch_tp,
                update_rwl: RwLock::new(()),
                evict_queue: SegQueue::new(),
            }),
            ev: RefCell::new(CacheAdvisor::new(PAGE_CACHE_SZ, 20)),
        }
    }

    /// Initiates retrieval of the page data at the given [`PageId`] asynchronously.
    ///
    /// If the page is already in the cache, this method does nothing. Otherwise, it fetches the
    /// page from the underlying store and caches it.
    pub fn prepopulate(&self, page_id: PageId) {
        self.accessed(&page_id);
        if let Entry::Vacant(v) = self.shared.cached.entry(page_id.clone()) {
            // Nope, then we need to fetch the page from the store.
            let inflight = Arc::new(InflightFetch::new());
            v.insert(PageState::Inflight(inflight.clone()));
            let task = {
                let shared = self.shared.clone();
                move || {
                    // the page fetch has been pre-empted in the meantime. avoid querying.
                    if Arc::strong_count(&inflight) == 1 {
                        return;
                    }

                    let entry = shared
                        .store
                        .load_page(page_id.clone())
                        .expect("db load failed") // TODO: handle the error
                        .map_or_else(
                            || {
                                PageData::pristine_empty(
                                    &shared.page_rw_pass_domain,
                                    page_id.clone(),
                                )
                            },
                            |data| {
                                PageData::pristine_with_data(
                                    &shared.page_rw_pass_domain,
                                    page_id.clone(),
                                    data,
                                )
                            },
                        );
                    let entry = Arc::new(entry);

                    // Unwrap: the operation was inserted above. It is scheduled for execution only
                    // once. It may removed only in the line below. Therefore, `None` is impossible.
                    let mut page_state_guard = shared.cached.get_mut(&page_id).unwrap();
                    let page_state = page_state_guard.value_mut();

                    if let PageState::Cached(_) = page_state {
                        // We race against pre-emption in the case other code pre-empts us by
                        // allocating an empty page.
                        return;
                    }

                    if let PageState::Inflight(inflight) =
                        mem::replace(page_state, PageState::Cached(entry.clone()))
                    {
                        inflight.complete_and_notify(Page { inner: entry });
                    }
                }
            };
            self.shared.fetch_tp.execute(task);
        }
    }

    /// Pre-empt a previously submitted prepopulation request on a best-effort basis by returning
    /// an empty page to all waiters. This should only be called when it is known that the page
    /// will definitely not exist.
    ///
    /// This is not guaranteed to cancel the request if it is already being processed by a
    /// DB thread.
    pub fn cancel_prepopulate(&self, page_id: PageId) {
        let Some(mut page_state_guard) = self.shared.cached.get_mut(&page_id) else {
            return;
        };
        let page_state = page_state_guard.value_mut();

        let page_data = {
            let PageState::Inflight(inflight) = page_state else {
                return;
            };
            let page_data = Arc::new(PageData::pristine_empty(
                &self.shared.page_rw_pass_domain,
                page_id.clone(),
            ));
            inflight.complete_and_notify(Page {
                inner: page_data.clone(),
            });
            page_data
        };
        *page_state = PageState::Cached(page_data);
    }

    /// Retrieves the page data at the given [`PageId`] synchronously.
    ///
    /// If the page is in the cache, it is returned immediately. If the page is not in the cache, it
    /// is fetched from the underlying store and returned. If `hint_fresh` is true, this immediately
    /// returns a blank page.
    ///
    /// This method is blocking, but doesn't suffer from the channel overhead.
    pub fn retrieve_sync(&self, page_id: PageId, hint_fresh: bool) -> Page {
        self.accessed(&page_id);
        let maybe_inflight = match self.shared.cached.entry(page_id.clone()) {
            Entry::Occupied(mut o) => {
                let page = o.get_mut();
                match page {
                    PageState::Cached(ref page) => {
                        return Page {
                            inner: page.clone(),
                        };
                    }
                    PageState::Inflight(ref inflight) => {
                        if hint_fresh {
                            // pre-empt stale fetch.

                            let page_data = Arc::new(PageData::pristine_empty(
                                &self.shared.page_rw_pass_domain,
                                page_id,
                            ));
                            let fresh_page = Page {
                                inner: page_data.clone(),
                            };

                            inflight.complete_and_notify(fresh_page.clone());
                            *page = PageState::Cached(page_data);
                            return fresh_page;
                        } else {
                            Some(inflight.clone())
                        }
                    }
                }
            }
            Entry::Vacant(v) => {
                if hint_fresh {
                    let page_data = Arc::new(PageData::pristine_empty(
                        &self.shared.page_rw_pass_domain,
                        page_id,
                    ));
                    let page = Page {
                        inner: page_data.clone(),
                    };
                    v.insert(PageState::Cached(page_data));
                    return page;
                }
                v.insert(PageState::Inflight(Arc::new(InflightFetch::new())));
                None
            }
        };

        // do not wait with dashmap lock held; deadlock
        if let Some(existing_inflight) = maybe_inflight {
            return existing_inflight.wait();
        }

        let entry = self
            .shared
            .store
            .load_page(page_id.clone())
            .expect("db load failed") // TODO: handle the error
            .map_or_else(
                || PageData::pristine_empty(&self.shared.page_rw_pass_domain, page_id.clone()),
                |data| {
                    PageData::pristine_with_data(
                        &self.shared.page_rw_pass_domain,
                        page_id.clone(),
                        data,
                    )
                },
            );
        let entry = Arc::new(entry);

        // UNWRAP: we inserted a value into the map which cannot have been evicted in the meantime.
        let mut page_state_guard = self.shared.cached.get_mut(&page_id).unwrap();
        let page_state = page_state_guard.value_mut();

        if let PageState::Cached(page_data) = page_state {
            // pre-empted by retrieve_sync (hint_fresh=true) on another thread
            return Page {
                inner: page_data.clone(),
            };
        }

        if let PageState::Inflight(inflight) =
            std::mem::replace(page_state, PageState::Cached(entry.clone()))
        {
            inflight.complete_and_notify(Page {
                inner: entry.clone(),
            });
        }
        Page { inner: entry }
    }

    fn accessed(&self, page_id: &PageId) {
        self.ev.borrow_mut().accessed(page_id.clone(), &self.shared.evict_queue);
        self.try_perform_evict();
    }

    fn try_perform_evict(&self) {
        // If we succeed in acquiring the READ lock, we are not in the update phase. That means
        // we can perform eviction safely.
        if let Some(_) = self.shared.update_rwl.try_read() {
            // Limit the amount of work here. We don't want to evict too many pages at once.
            let to_evict = std::cmp::max(128, self.shared.evict_queue.len());
            for _ in 0..to_evict {
                if let Some(page_id) = self.shared.evict_queue.pop() {
                    let Some((_, evicted)) = self.shared.cached.remove(&page_id) else { continue; };
                    match evicted {
                        PageState::Cached(_) => (),
                        PageState::Inflight(_) => {
                            panic!();
                        }
                    }
                }
            }
        }
    }

    /// Acquire a read pass for all pages in the cache.
    pub fn new_read_pass(&self) -> ReadPass<PageRegion> {
        self.shared
            .page_rw_pass_domain
            .new_read_pass()
            .with_region(PageRegion::universe())
    }

    /// Acquire a write pass for all pages in the cache.
    pub fn new_write_pass(&self) -> WritePass<PageRegion> {
        self.shared
            .page_rw_pass_domain
            .new_write_pass()
            .with_region(PageRegion::universe())
    }

    pub fn start_update_phase<'a>(&'a self) -> impl Drop + 'a {
        let update_rwl = self.shared.update_rwl.write();
        CommitGuard {
            cache: self,
            update_rwl,
        }
    }

    /// Flushes all the dirty pages into the underlying store.
    /// This takes a read pass.
    ///
    /// After the commit, all the dirty pages are cleared.
    pub fn commit(
        &self,
        page_diffs: impl IntoIterator<Item = (PageId, PageDiff)>,
        tx: &mut Transaction,
    ) {
        const FULL_PAGE_THRESHOLD: usize = 32;

        let read_pass = self.new_read_pass();
        for (page_id, page_diff) in page_diffs {
            if let Some(ref page) = self.shared.cached.get(&page_id) {
                match page.value() {
                    PageState::Cached(ref page) => {
                        let page_data = page.data.read(&read_pass);
                        if page_data.as_ref().map_or(true, |p| page_is_empty(&p[..])) {
                            tx.delete_page(page_id);
                            continue;
                        }

                        let Some(page_data) = page_data.as_ref() else {
                            continue;
                        };

                        let updated_count = page_diff.updated_slots.count_ones();
                        if updated_count >= FULL_PAGE_THRESHOLD {
                            tx.write_page(page_id, page_data);
                            continue;
                        }

                        let mut tagged_nodes = Vec::with_capacity(33 * updated_count);
                        for slot_index in page_diff.updated_slots.iter_ones() {
                            tagged_nodes.push(slot_index as u8);

                            tagged_nodes.extend(&page_data[slot_index * 32..][..32]);
                        }
                        tx.write_page_nodes(page_id, tagged_nodes);
                    }
                    PageState::Inflight(_) => {
                        panic!("dirty page is inflight");
                    }
                }
            }
        }
        drop(read_pass);
    }
}

struct CommitGuard<'a> {
    cache: &'a PageCache,
    update_rwl: RwLockWriteGuard<'a, ()>,
}

impl<'a> Drop for CommitGuard<'a> {
    fn drop(&mut self) {
        self.cache.try_perform_evict();
    }
}
