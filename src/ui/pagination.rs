use crate::library::Library;
use crate::traits::ListItem;
use crate::utils::OnDrop;
use log::debug;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct ApiPage<I> {
    pub offset: u32,
    pub total: u32,
    pub items: Vec<I>,
}
pub type FetchPageFn<I> = dyn Fn(u32) -> Option<ApiPage<I>> + Send + Sync;
pub struct ApiResult<I> {
    offset: Arc<RwLock<u32>>,
    limit: u32,
    pub total: u32,
    pub items: Arc<RwLock<Vec<I>>>,
    fetch_page: Arc<FetchPageFn<I>>,
    /// Set when any page request returned `None`, so callers can tell a failure from the end.
    failed: Arc<AtomicBool>,
}

impl<I: ListItem + Clone> ApiResult<I> {
    pub fn new(limit: u32, fetch_page: Arc<FetchPageFn<I>>) -> Self {
        let items = Arc::new(RwLock::new(Vec::new()));
        if let Some(first_page) = fetch_page(0) {
            debug!(
                "fetched first page, items: {}, total: {}",
                first_page.items.len(),
                first_page.total
            );
            items.write().extend(first_page.items);
            Self {
                offset: Arc::new(RwLock::new(first_page.offset)),
                limit,
                total: first_page.total,
                items,
                fetch_page: fetch_page.clone(),
                failed: Arc::new(AtomicBool::new(false)),
            }
        } else {
            Self {
                offset: Arc::new(RwLock::new(0)),
                limit,
                total: 0,
                items,
                fetch_page: fetch_page.clone(),
                failed: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    /// Whether any page request has failed.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    fn offset(&self) -> u32 {
        *self.offset.read()
    }

    pub fn at_end(&self) -> bool {
        (self.offset() + self.limit) >= self.total
    }

    pub fn apply_pagination(self, pagination: &Pagination<I>) {
        let total = self.total as usize;
        let fetched_items = self.items.read().len();
        pagination.set(
            fetched_items,
            total,
            Box::new(move |_| {
                self.next();
            }),
        )
    }

    /// Fetch pages until the end is reached or a page fails to load. Check `failed()` afterwards.
    pub fn fetch_all(&self) {
        while !self.at_end() {
            if self.next().is_none() {
                break;
            }
        }
    }

    pub fn next(&self) -> Option<Vec<I>> {
        let offset = self.offset() + self.limit;
        debug!("fetching next page at offset {offset}");
        if !self.at_end() {
            if let Some(next_page) = (self.fetch_page)(offset) {
                *self.offset.write() = next_page.offset;
                self.items.write().extend(next_page.items.clone());
                Some(next_page.items)
            } else {
                self.failed.store(true, Ordering::SeqCst);
                None
            }
        } else {
            debug!("paginator is at end");
            None
        }
    }
}

/// Run `fetch` on a background thread and fill `content` with its first page, so a view can be
/// built on the UI thread without waiting on the network or the rate limiter. Later pages are
/// appended to `content` through `pagination`. `redraw` runs once the first page is in.
pub fn fill_in_background<I, F, R>(
    content: Arc<RwLock<Vec<I>>>,
    pagination: Pagination<I>,
    fetch: F,
    redraw: R,
) where
    I: ListItem + Clone + Send + Sync + 'static,
    F: FnOnce() -> ApiResult<I> + Send + 'static,
    R: FnOnce() + Send + 'static,
{
    std::thread::spawn(move || {
        let result = fetch();
        let first_page = result.items.read().clone();
        let loaded = {
            let mut content = content.write();
            content.extend(first_page);
            content.len()
        };
        pagination.set(
            loaded,
            result.total as usize,
            Box::new(move |content| {
                if let Some(items) = result.next() {
                    content.write().extend(items);
                }
            }),
        );
        redraw();
    });
}

pub type Paginator<I> = Box<dyn Fn(Arc<RwLock<Vec<I>>>) + Send + Sync>;

/// Where a list that pages by position stands. Cloning shares the state.
///
/// The state is `Some((next_offset, total))` while the list pages by position and `None`
/// while it pages by row count. A paging callback captures a `Position` (never the
/// `Pagination`) and calls `set` while it runs: `Pagination::call` holds the callback lock for
/// reading meanwhile, so these methods take only this handle's own lock.
#[derive(Clone, Default)]
pub struct Position {
    state: Arc<RwLock<PositionState>>,
}

/// `Some((next_offset, total))` for a positioned list, `None` for row-count paging.
type PositionState = Option<(Option<usize>, usize)>;

impl Position {
    /// The offset of the next page, `None` when there is no more or the list does not page by
    /// position.
    pub fn next_offset(&self) -> Option<usize> {
        self.state.read().and_then(|(next_offset, _)| next_offset)
    }

    /// Set the next offset and the latest total. Takes only this handle's own lock.
    pub fn set(&self, next_offset: Option<usize>, total: usize) {
        *self.state.write() = Some((next_offset, total));
    }

    fn get(&self) -> PositionState {
        *self.state.read()
    }

    fn reset(&self) {
        *self.state.write() = None;
    }
}

/// Manages the loading of ListItems, to increase performance and decrease
/// memory usage.
///
/// A list pages either by row count (`set`: more while `loaded_content < max_content`) or by
/// position (`set_positioned`: more while the shared `position` has a next offset).
///
/// `loaded_content`: The amount of currently loaded items
/// `max_content`: The maximum amount of items, for row-count paging
/// `position`: Where a positioned list stands, shared by every clone
/// `callback`: TODO: document
/// `busy`: TODO: document
#[derive(Clone)]
pub struct Pagination<I: ListItem> {
    loaded_content: Arc<RwLock<usize>>,
    max_content: Arc<RwLock<Option<usize>>>,
    callback: Arc<RwLock<Option<Paginator<I>>>>,
    busy: Arc<AtomicBool>,
    position: Position,
}

impl<I: ListItem> Default for Pagination<I> {
    fn default() -> Self {
        Self {
            loaded_content: Arc::new(RwLock::new(0)),
            max_content: Arc::new(RwLock::new(None)),
            callback: Arc::new(RwLock::new(None)),
            busy: Arc::new(AtomicBool::new(false)),
            position: Position::default(),
        }
    }
}

impl<I: ListItem + Clone> Pagination<I> {
    pub fn clear(&mut self) {
        *self.max_content.write() = None;
        self.position.reset();
        *self.callback.write() = None;
    }
    pub fn set(&self, loaded_content: usize, max_content: usize, callback: Paginator<I>) {
        *self.loaded_content.write() = loaded_content;
        *self.max_content.write() = Some(max_content);
        self.position.reset();
        *self.callback.write() = Some(callback);
    }

    /// Page by position: more can be loaded while `next_offset` is `Some`.
    pub fn set_positioned(&self, next_offset: Option<usize>, total: usize, callback: Paginator<I>) {
        *self.max_content.write() = None;
        self.position.set(next_offset, total);
        *self.callback.write() = Some(callback);
    }

    /// The handle a paging callback uses to move or end paging.
    pub fn position(&self) -> Position {
        self.position.clone()
    }

    /// True while another page can be asked for.
    pub fn can_load_more(&self) -> bool {
        match self.position.get() {
            Some((next_offset, _)) => next_offset.is_some(),
            None => self.max_content().unwrap_or(0) > self.loaded_content(),
        }
    }

    /// Rows still to come, for the "more items" row.
    pub fn remaining(&self) -> usize {
        match self.position.get() {
            Some((Some(next_offset), total)) => total.saturating_sub(next_offset),
            Some((None, _)) => 0,
            None => self
                .max_content()
                .unwrap_or(0)
                .saturating_sub(self.loaded_content()),
        }
    }

    pub fn loaded_content(&self) -> usize {
        *self.loaded_content.read()
    }

    /// The total for a positioned list, the row count to reach otherwise.
    pub fn max_content(&self) -> Option<usize> {
        match self.position.get() {
            Some((_, total)) => Some(total),
            None => *self.max_content.read(),
        }
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn call(&self, content: &Arc<RwLock<Vec<I>>>, library: Arc<Library>) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let pagination = self.clone();
        let content = content.clone();
        // Created here and moved into the thread, so `busy` is cleared and the length
        // refreshed whether the paginator returns or panics.
        let guard = OnDrop::new({
            let pagination = pagination.clone();
            let content = content.clone();
            move || {
                *pagination.loaded_content.write() = content.read().len();
                pagination.busy.store(false, Ordering::SeqCst);
                library.trigger_redraw();
            }
        });
        std::thread::spawn(move || {
            let _guard = guard;
            let cb = pagination.callback.read();
            if let Some(ref cb) = *cb {
                debug!("calling paginator!");
                cb(content.clone());
            }
        });
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::model::playlist::Playlist;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn item(id: &str) -> Playlist {
        Playlist {
            id: id.to_string(),
            name: id.to_string(),
            owner_id: String::new(),
            owner_name: None,
            snapshot_id: String::new(),
            num_tracks: 0,
            tracks: None,
            collaborative: false,
            cover_url: None,
            tracks_unknown: false,
        }
    }

    fn always_failing(calls: Arc<AtomicUsize>) -> Arc<FetchPageFn<Playlist>> {
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        })
    }

    /// Three pages of one item each; page 1 fails when `fail_second` is set.
    fn three_pages(fail_second: bool) -> Arc<FetchPageFn<Playlist>> {
        Arc::new(move |offset| {
            if fail_second && offset == 1 {
                return None;
            }
            Some(ApiPage {
                offset,
                total: 3,
                items: vec![item(&offset.to_string())],
            })
        })
    }

    #[test]
    fn failed_first_page_marks_the_result_failed() {
        let result = ApiResult::new(1, always_failing(Arc::new(AtomicUsize::new(0))));
        assert!(result.failed());
        assert!(result.items.read().is_empty());
    }

    #[test]
    fn failed_next_page_marks_the_result_failed() {
        let result = ApiResult::new(1, three_pages(true));
        assert!(!result.failed());
        assert!(result.next().is_none());
        assert!(result.failed());
        assert!(!result.at_end());
    }

    #[test]
    fn complete_fetch_is_not_failed() {
        let result = ApiResult::new(1, three_pages(false));
        while !result.at_end() {
            assert!(result.next().is_some());
        }
        assert!(!result.failed());
        assert_eq!(result.items.read().len(), 3);
    }

    #[test]
    fn draining_loop_stops_on_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let fetch: Arc<FetchPageFn<Playlist>> = Arc::new(move |offset| {
            counter.fetch_add(1, Ordering::SeqCst);
            if offset == 0 {
                Some(ApiPage {
                    offset,
                    total: 3,
                    items: vec![item("0")],
                })
            } else {
                None
            }
        });
        let r = ApiResult::new(1, fetch);
        r.fetch_all();
        assert!(r.failed());
        assert!(calls.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn fill_in_background_returns_before_the_first_page() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (release, gate) = mpsc::channel::<()>();
        let (redrawn, redraw_seen) = mpsc::channel::<()>();
        let content: Arc<RwLock<Vec<Playlist>>> = Arc::new(RwLock::new(Vec::new()));
        let pagination = Pagination::default();

        fill_in_background(
            content.clone(),
            pagination.clone(),
            move || {
                gate.recv().unwrap();
                ApiResult::new(1, three_pages(false))
            },
            move || redrawn.send(()).unwrap(),
        );

        // The first page is still held back, so nothing is loaded yet.
        assert!(content.read().is_empty());
        assert_eq!(pagination.max_content(), None);

        release.send(()).unwrap();
        redraw_seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(content.read().len(), 1);
        assert_eq!(pagination.loaded_content(), 1);
        assert_eq!(pagination.max_content(), Some(3));

        // The paginator appends later pages to the view's own list.
        let callback = pagination.callback.read();
        callback.as_ref().unwrap()(content.clone());
        assert_eq!(content.read().len(), 2);
    }

    fn test_library() -> Arc<Library> {
        let cfg = crate::config::Config::new_for_test();
        let ev = crate::events::EventManager::new_for_test();
        let spotify = crate::spotify::Spotify::new_for_test(cfg.clone(), ev.clone());
        Library::new_for_test(ev, spotify, cfg)
    }

    fn wait_not_busy(pagination: &Pagination<Playlist>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pagination.is_busy() {
            assert!(
                std::time::Instant::now() < deadline,
                "busy was never cleared"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn call_clears_busy_when_the_paginator_panics() {
        let pagination: Pagination<Playlist> = Pagination::default();
        pagination.set(0, 3, Box::new(|_| panic!("boom")));
        let content = Arc::new(RwLock::new(vec![item("a"), item("b")]));
        pagination.call(&content, test_library());
        wait_not_busy(&pagination);
        assert_eq!(pagination.loaded_content(), 2);
    }

    #[test]
    fn call_clears_busy_without_a_callback() {
        let pagination: Pagination<Playlist> = Pagination::default();
        let content = Arc::new(RwLock::new(vec![item("a")]));
        pagination.call(&content, test_library());
        wait_not_busy(&pagination);
    }

    #[test]
    fn a_second_call_while_busy_starts_nothing() {
        let pagination: Pagination<Playlist> = Pagination::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let wait = parking_lot::Mutex::new(wait);
        let counted = calls.clone();
        pagination.set(
            0,
            3,
            Box::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = wait.lock().recv_timeout(std::time::Duration::from_secs(5));
            }),
        );
        let content = Arc::new(RwLock::new(vec![item("a")]));
        let library = test_library();
        pagination.call(&content, library.clone());
        assert!(pagination.is_busy());
        pagination.call(&content, library);
        release.send(()).unwrap();
        wait_not_busy(&pagination);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn row_count_paging_is_unchanged() {
        let p = Pagination::<Playlist>::default();
        assert!(!p.can_load_more());
        p.set(10, 25, Box::new(|_| {}));
        assert!(p.can_load_more());
        assert_eq!(p.remaining(), 15);
        assert_eq!(p.position().next_offset(), None);
    }

    #[test]
    fn positioned_paging_follows_the_next_offset_not_the_rows() {
        let p = Pagination::<Playlist>::default();
        p.set_positioned(Some(50), 180, Box::new(|_| {}));
        assert!(p.can_load_more());
        assert_eq!((p.remaining(), p.max_content()), (130, Some(180)));
        p.position().set(None, 171);
        assert!(!p.can_load_more());
        assert_eq!(p.remaining(), 0);
        p.set_positioned(Some(0), 40, Box::new(|_| {}));
        assert!(
            p.can_load_more(),
            "zero rows with a next offset still pages"
        );
    }

    #[test]
    fn a_callback_can_end_paging_without_deadlock() {
        let p = Pagination::<Playlist>::default();
        let pos = p.position(); // taken before set_positioned: the handle is shared, not replaced
        p.set_positioned(Some(50), 100, Box::new(move |_| pos.set(None, 100)));
        let content = Arc::new(RwLock::new(Vec::new()));
        p.call(&content, test_library());
        wait_not_busy(&p);
        assert!(!p.can_load_more());
        // The paginator still works afterwards.
        p.set(0, 1, Box::new(|_| {}));
        assert!(p.can_load_more());
    }

    #[test]
    fn clear_resets_the_position() {
        let mut p = Pagination::<Playlist>::default();
        let other = p.clone();
        let pos = p.position();
        p.set_positioned(Some(50), 100, Box::new(|_| {}));
        assert!(other.can_load_more(), "clones share the position");
        p.clear();
        assert!(!other.can_load_more());
        assert_eq!(pos.next_offset(), None);
    }
}
