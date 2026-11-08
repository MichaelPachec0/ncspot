use crate::ext_traits::RwLockExt;
use crate::library::Library;
use crate::traits::ListItem;
use log::debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

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
            items.write().unwrap().extend(first_page.items);
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
        *self.offset.read().unwrap()
    }

    pub fn at_end(&self) -> bool {
        (self.offset() + self.limit) >= self.total
    }

    pub fn apply_pagination(self, pagination: &Pagination<I>) {
        let total = self.total as usize;
        let fetched_items = self.items.read().unwrap().len();
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
                *self.offset.write().unwrap() = next_page.offset;
                self.items.write().unwrap().extend(next_page.items.clone());
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
        let first_page = result.items.read_unpoisoned().clone();
        let loaded = {
            let mut content = content.write_unpoisoned();
            content.extend(first_page);
            content.len()
        };
        pagination.set(
            loaded,
            result.total as usize,
            Box::new(move |content| {
                if let Some(items) = result.next() {
                    content.write_unpoisoned().extend(items);
                }
            }),
        );
        redraw();
    });
}

pub type Paginator<I> = Box<dyn Fn(Arc<RwLock<Vec<I>>>) + Send + Sync>;

/// Manages the loading of ListItems, to increase performance and decrease
/// memory usage.
///
/// `loaded_content`: The amount of currently loaded items
/// `max_content`: The maximum amount of items
/// `callback`: TODO: document
/// `busy`: TODO: document
#[derive(Clone)]
pub struct Pagination<I: ListItem> {
    loaded_content: Arc<RwLock<usize>>,
    max_content: Arc<RwLock<Option<usize>>>,
    callback: Arc<RwLock<Option<Paginator<I>>>>,
    busy: Arc<RwLock<bool>>,
}

impl<I: ListItem> Default for Pagination<I> {
    fn default() -> Self {
        Self {
            loaded_content: Arc::new(RwLock::new(0)),
            max_content: Arc::new(RwLock::new(None)),
            callback: Arc::new(RwLock::new(None)),
            busy: Arc::new(RwLock::new(false)),
        }
    }
}

impl<I: ListItem + Clone> Pagination<I> {
    pub fn clear(&mut self) {
        *self.max_content.write().unwrap() = None;
        *self.callback.write().unwrap() = None;
    }
    pub fn set(&self, loaded_content: usize, max_content: usize, callback: Paginator<I>) {
        *self.loaded_content.write().unwrap() = loaded_content;
        *self.max_content.write().unwrap() = Some(max_content);
        *self.callback.write().unwrap() = Some(callback);
    }

    pub fn loaded_content(&self) -> usize {
        *self.loaded_content.read().unwrap()
    }

    pub fn max_content(&self) -> Option<usize> {
        *self.max_content.read().unwrap()
    }

    pub(crate) fn is_busy(&self) -> bool {
        *self.busy.read().unwrap()
    }

    pub fn call(&self, content: &Arc<RwLock<Vec<I>>>, library: Arc<Library>) {
        let pagination = self.clone();
        let content = content.clone();
        if !self.is_busy() {
            *self.busy.write().unwrap() = true;
            std::thread::spawn(move || {
                let cb = pagination.callback.read().unwrap();
                if let Some(ref cb) = *cb {
                    debug!("calling paginator!");
                    cb(content.clone());
                    *pagination.loaded_content.write().unwrap() = content.read().unwrap().len();
                    *pagination.busy.write().unwrap() = false;
                    library.trigger_redraw();
                }
            });
        }
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
        assert!(result.items.read().unwrap().is_empty());
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
        assert_eq!(result.items.read().unwrap().len(), 3);
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
        assert!(content.read().unwrap().is_empty());
        assert_eq!(pagination.max_content(), None);

        release.send(()).unwrap();
        redraw_seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(content.read().unwrap().len(), 1);
        assert_eq!(pagination.loaded_content(), 1);
        assert_eq!(pagination.max_content(), Some(3));

        // The paginator appends later pages to the view's own list.
        let callback = pagination.callback.read().unwrap();
        callback.as_ref().unwrap()(content.clone());
        assert_eq!(content.read().unwrap().len(), 2);
    }
}
