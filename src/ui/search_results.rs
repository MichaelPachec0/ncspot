use crate::application::ASYNC_RUNTIME;
use crate::command::Command;
use crate::commands::CommandResult;
use crate::config::SEARCH_TOP_ROWS_PER_KIND;
use crate::events::{Event, EventManager};
use crate::internal_api::search::Kind;
use crate::library::Library;
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::episode::Episode;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::top_item::{TopItem, top_items};
use crate::model::track::Track;
use crate::queue::Queue;
use crate::search_api::{FirstPage, NextPage, Rows, TabPage, TopDraft, TopRow, section_rows};
use crate::spotify::{Spotify, UriType};
use crate::spotify_api::WebApi;
use crate::spotify_url::SpotifyUrl;
use crate::traits::{ListItem, ViewExt};
use crate::ui::listview::ListView;
use crate::ui::pagination::{Pagination, Position};
use crate::ui::tabbedview::TabbedView;
use cursive::view::ViewWrapper;
use cursive::{Cursive, Vec2, View};
use parking_lot::RwLock;
use rspotify::model::SearchType;
use rspotify::model::search::SearchResult;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct SearchResultsView {
    search_term: String,
    results_tracks: Arc<RwLock<Vec<Track>>>,
    pagination_tracks: Pagination<Track>,
    results_albums: Arc<RwLock<Vec<Album>>>,
    pagination_albums: Pagination<Album>,
    results_artists: Arc<RwLock<Vec<Artist>>>,
    pagination_artists: Pagination<Artist>,
    results_playlists: Arc<RwLock<Vec<Playlist>>>,
    pagination_playlists: Pagination<Playlist>,
    results_shows: Arc<RwLock<Vec<Show>>>,
    pagination_shows: Pagination<Show>,
    results_episodes: Arc<RwLock<Vec<Episode>>>,
    pagination_episodes: Pagination<Episode>,
    /// The rows of the Top tab; `None` when the view has no Top tab.
    top: Option<Arc<RwLock<Vec<TopItem>>>>,
    /// Set by the search thread to ask for the move from Top to Tracks; applied and cleared
    /// on the next layout.
    to_tracks: Arc<AtomicBool>,
    /// Rows of each kind in the Top tab, from the config when the view was built.
    rows_per_kind: usize,
    tabs: TabbedView,
    spotify: Spotify,
    events: EventManager,
}

/// What a search term is, known before the tabs are built.
pub(crate) enum TermKind {
    Uri(UriType),
    Url(SpotifyUrl),
    Text,
}

pub(crate) fn term_kind(term: &str) -> TermKind {
    if let Ok(uri_type) = term.parse() {
        TermKind::Uri(uri_type)
    } else if let Some(url) = SpotifyUrl::from_url(term) {
        TermKind::Url(url)
    } else {
        TermKind::Text
    }
}

/// Whether a search for `term` (of kind `kind`) gets a Top tab: a text term that is not blank,
/// while the internal search is in use.
fn with_top(term: &str, kind: &TermKind, internal_search_in_use: bool) -> bool {
    matches!(kind, TermKind::Text) && !term.trim().is_empty() && internal_search_in_use
}

/// What `term` is, and whether its search view gets a Top tab with `api` as it stands now.
/// Sends nothing.
fn term_and_top(term: &str, api: &WebApi) -> (TermKind, bool) {
    let kind = term_kind(term);
    let top = with_top(term, &kind, api.internal_search_in_use());
    (kind, top)
}

/// The tab a lookup of `uri_type` shows its item in, in a view without a Top tab.
fn uri_tab(uri_type: UriType) -> usize {
    match uri_type {
        UriType::Track => 0,
        UriType::Album => 1,
        UriType::Artist => 2,
        UriType::Playlist => 3,
        UriType::Show => 4,
        UriType::Episode => 5,
    }
}

/// Rows of each kind the Top tab shows, from the config.
fn rows_per_kind(library: &Library) -> usize {
    library
        .cfg
        .values()
        .search_top_rows_per_kind
        .unwrap_or(SEARCH_TOP_ROWS_PER_KIND) as usize
}

/// The index of the Tracks tab in a view with a Top tab.
const TRACKS_AFTER_TOP: usize = 1;

/// Pure: the tab to move to when a move to Tracks was asked for, if any.
pub(crate) fn tracks_hop(asked: bool, moved_by_user: bool, has_top: bool) -> Option<usize> {
    (asked && !moved_by_user && has_top).then_some(TRACKS_AFTER_TOP)
}

/// The Top tab's part of a finished internal first page: settle the strays with
/// `fetch_strays`, write the rows and `sections` once, and ask for the move to Tracks when no
/// item row (a section row counts) came of it.
fn fill_top(
    mut top: TopDraft,
    sections: Vec<(Kind, Vec<TopRow>)>,
    fetch_strays: impl FnOnce(&mut TopDraft),
    rows: &RwLock<Vec<TopItem>>,
    to_tracks: &AtomicBool,
) {
    if top.has_strays() {
        fetch_strays(&mut top);
    }
    let items = top_items(top.rows(), sections, top.featured());
    if !items.iter().any(TopItem::is_item) {
        to_tracks.store(true, Ordering::Relaxed);
    }
    *rows.write() = items;
}

/// How the first page of a text search came back.
#[derive(Clone, Copy)]
pub(crate) enum FirstPageClass {
    Internal,
    NotInUse,
    Failed,
}

impl FirstPageClass {
    fn of(page: &FirstPage) -> Self {
        match page {
            FirstPage::Internal { .. } => Self::Internal,
            FirstPage::NotInUse => Self::NotInUse,
            FirstPage::Failed => Self::Failed,
        }
    }
}

/// What the background thread does with the first page, apart from filling the six tabs.
/// Returns whether the Web API search must run. A Top tab the internal search did not serve
/// asks for the move to Tracks and calls `trigger` right after, so the move does not wait
/// for the Web API search; `trigger` is not called otherwise.
fn after_first_page(
    page_is: FirstPageClass,
    has_top: bool,
    to_tracks: &AtomicBool,
    trigger: impl FnOnce(),
) -> bool {
    match page_is {
        FirstPageClass::Internal | FirstPageClass::Failed => false,
        FirstPageClass::NotInUse => {
            if has_top {
                to_tracks.store(true, Ordering::Relaxed);
                trigger();
            }
            true
        }
    }
}

/// The status bar text for a search whose first page failed. Never carries the term.
const SEARCH_FAILED_NOTE: &str = "Search failed, try again";

/// Tell the user that the first page failed, so empty tabs do not read as "no results".
fn report_failed_search(events: &EventManager) {
    events.send(Event::StatusNote(SEARCH_FAILED_NOTE.into()));
}

type SearchHandler<I> =
    Box<dyn Fn(&Spotify, &Arc<RwLock<Vec<I>>>, &str, usize, bool) -> u32 + Send + Sync>;

/// A tab's model rows from the rows of an internal search page.
trait FromRows: Sized {
    /// `None` when `rows` hold another kind than this tab shows.
    fn from_rows(rows: Rows) -> Option<Vec<Self>>;
}

macro_rules! from_rows {
    ($model:ty, $variant:ident, |$row:ident| $convert:expr) => {
        impl FromRows for $model {
            fn from_rows(rows: Rows) -> Option<Vec<Self>> {
                match rows {
                    Rows::$variant(rows) => Some(rows.into_iter().map(|$row| $convert).collect()),
                    _ => None,
                }
            }
        }
    };
}

from_rows!(Track, Tracks, |row| Track::from(&row));
from_rows!(Album, Albums, |row| Album::from(&row));
from_rows!(Artist, Artists, |row| Artist::from(&row));
from_rows!(Playlist, Playlists, |row| row);
from_rows!(Show, Shows, |row| Show::from(&row));
from_rows!(Episode, Episodes, |row| Episode::from(&row));

/// What a request for the next page of a tab gave, with the rows already converted.
pub(crate) enum NextOutcome<I> {
    Page {
        rows: Vec<I>,
        next_offset: Option<usize>,
        total: usize,
    },
    Failed,
    Ended,
}

/// Append `new` to `rows`, skipping items whose `id()` is already there.
/// Returns how many were added.
pub(crate) fn append_new<I: ListItem + Clone>(rows: &mut Vec<I>, new: Vec<I>) -> usize {
    let mut seen: HashSet<String> = rows.iter().filter_map(|row| row.id()).collect();
    let before = rows.len();
    for item in new {
        // An item without an id cannot be matched, so it is always kept.
        if item.id().is_none_or(|id| seen.insert(id)) {
            rows.push(item);
        }
    }
    rows.len() - before
}

/// Write the outcome of a next-page request into a tab: a page is appended and moves the
/// position, a failure keeps the offset so the same page can be asked again, and an end stops
/// paging at the rows shown.
pub(crate) fn apply_next_page<I: ListItem + Clone>(
    rows: &RwLock<Vec<I>>,
    position: &Position,
    page: NextOutcome<I>,
) {
    match page {
        NextOutcome::Page {
            rows: new,
            next_offset,
            total,
        } => {
            append_new(&mut rows.write(), new);
            position.set(next_offset, total);
        }
        NextOutcome::Failed => {}
        NextOutcome::Ended => {
            let shown = rows.read().len();
            position.set(None, shown);
        }
    }
}

/// The rows and paginator of one tab, cloned so a background thread can fill them.
#[derive(Clone)]
struct TabHandle<I: ListItem + Clone> {
    rows: Arc<RwLock<Vec<I>>>,
    pagination: Pagination<I>,
}

impl<I> TabHandle<I>
where
    I: FromRows + ListItem + Clone + Send + Sync + 'static,
{
    /// Show the first page of an internal search and page the rest by position. The paging
    /// callback asks for the next page with no lock held and writes it afterwards.
    /// A tab the answer has no page for stays empty.
    fn fill(
        &self,
        page: Option<TabPage>,
        kind: Kind,
        term: &str,
        spotify: &Spotify,
        events: &EventManager,
    ) {
        let Some(page) = page else {
            return;
        };
        *self.rows.write() = I::from_rows(page.rows).unwrap_or_default();

        let rows = self.rows.clone();
        let position = self.pagination.position();
        let term = term.to_owned();
        let spotify = spotify.clone();
        let events = events.clone();
        let callback = move |_: Arc<RwLock<Vec<I>>>| {
            let Some(offset) = position.next_offset() else {
                return;
            };
            let outcome = match spotify.api.search_next_page(kind, &term, offset) {
                NextPage::Page(page) => match I::from_rows(page.rows) {
                    Some(new) => NextOutcome::Page {
                        rows: new,
                        next_offset: page.next_offset,
                        total: page.total,
                    },
                    None => NextOutcome::Failed,
                },
                NextPage::Failed => NextOutcome::Failed,
                NextPage::Ended => NextOutcome::Ended,
            };
            apply_next_page(&rows, &position, outcome);
            events.trigger();
        };
        self.pagination
            .set_positioned(page.next_offset, page.total, Box::new(callback));
    }
}

/// Every tab's handles, moved into the thread that runs a text search.
struct SearchTabs {
    tracks: TabHandle<Track>,
    albums: TabHandle<Album>,
    artists: TabHandle<Artist>,
    playlists: TabHandle<Playlist>,
    shows: TabHandle<Show>,
    episodes: TabHandle<Episode>,
}

/// The body of a text search's thread once the first page is in: an internal page fills the
/// six tabs, calls `trigger`, then fills the Top tab (settling its strays with `fetch_strays`)
/// and calls `trigger` again; a page the internal search did not serve hands the tabs to
/// `search_web`; a failed one posts the failure note. `term`, `spotify` and `events` serve
/// the paging of the filled tabs and the note.
#[expect(
    clippy::too_many_arguments,
    reason = "the seams a test of the search thread needs, plus what the tabs page with"
)]
fn finish_text_search(
    page: FirstPage,
    tabs: SearchTabs,
    top_rows: Option<&RwLock<Vec<TopItem>>>,
    to_tracks: &AtomicBool,
    term: &str,
    rows_per_kind: usize,
    spotify: &Spotify,
    events: &EventManager,
    fetch_strays: impl FnOnce(&mut TopDraft),
    trigger: impl Fn(),
    search_web: impl FnOnce(SearchTabs),
) {
    let page_is = FirstPageClass::of(&page);
    match page {
        FirstPage::Internal {
            tabs: mut pages,
            top,
        } => {
            // Cut before the fills, which take the pages out of the map.
            let sections = if top_rows.is_some() {
                section_rows(&pages, rows_per_kind)
            } else {
                Vec::new()
            };
            tabs.tracks.fill(
                pages.remove(&Kind::Track),
                Kind::Track,
                term,
                spotify,
                events,
            );
            tabs.albums.fill(
                pages.remove(&Kind::Album),
                Kind::Album,
                term,
                spotify,
                events,
            );
            tabs.artists.fill(
                pages.remove(&Kind::Artist),
                Kind::Artist,
                term,
                spotify,
                events,
            );
            tabs.playlists.fill(
                pages.remove(&Kind::Playlist),
                Kind::Playlist,
                term,
                spotify,
                events,
            );
            tabs.shows
                .fill(pages.remove(&Kind::Show), Kind::Show, term, spotify, events);
            tabs.episodes.fill(
                pages.remove(&Kind::Episode),
                Kind::Episode,
                term,
                spotify,
                events,
            );
            trigger();
            if let Some(rows) = top_rows {
                fill_top(top, sections, fetch_strays, rows, to_tracks);
                trigger();
            }
        }
        FirstPage::NotInUse => {}
        FirstPage::Failed => report_failed_search(events),
    }
    if after_first_page(page_is, top_rows.is_some(), to_tracks, &trigger) {
        search_web(tabs);
    }
}

impl SearchResultsView {
    pub fn new(
        search_term: String,
        events: EventManager,
        queue: Arc<Queue>,
        library: Arc<Library>,
    ) -> Self {
        let (term, top) = term_and_top(&search_term, &queue.get_spotify().api);
        let mut view = Self::build(search_term, events, queue, library, top);
        view.run_search(term);
        view
    }

    /// Build the view and its tabs without starting the search.
    fn build(
        search_term: String,
        events: EventManager,
        queue: Arc<Queue>,
        library: Arc<Library>,
        with_top: bool,
    ) -> Self {
        let rows_per_kind = rows_per_kind(&library);
        let results_tracks = Arc::new(RwLock::new(Vec::new()));
        let results_albums = Arc::new(RwLock::new(Vec::new()));
        let results_artists = Arc::new(RwLock::new(Vec::new()));
        let results_playlists = Arc::new(RwLock::new(Vec::new()));
        let results_shows = Arc::new(RwLock::new(Vec::new()));
        let results_episodes = Arc::new(RwLock::new(Vec::new()));

        let list_tracks = ListView::new(results_tracks.clone(), queue.clone(), library.clone());
        let pagination_tracks = list_tracks.get_pagination().clone();
        let list_albums = ListView::new(results_albums.clone(), queue.clone(), library.clone());
        let pagination_albums = list_albums.get_pagination().clone();
        let list_artists = ListView::new(results_artists.clone(), queue.clone(), library.clone());
        let pagination_artists = list_artists.get_pagination().clone();
        let list_playlists =
            ListView::new(results_playlists.clone(), queue.clone(), library.clone());
        let pagination_playlists = list_playlists.get_pagination().clone();
        let list_shows = ListView::new(results_shows.clone(), queue.clone(), library.clone());
        let pagination_shows = list_shows.get_pagination().clone();
        let list_episodes = ListView::new(results_episodes.clone(), queue.clone(), library.clone());
        let pagination_episodes = list_episodes.get_pagination().clone();

        let mut tabs = TabbedView::new();
        let top = with_top.then(|| {
            let results_top = Arc::new(RwLock::new(Vec::new()));
            let list_top = ListView::new(results_top.clone(), queue.clone(), library.clone());
            tabs.add_tab("Top", list_top);
            results_top
        });
        tabs.add_tab("Tracks", list_tracks);
        tabs.add_tab("Albums", list_albums);
        tabs.add_tab("Artists", list_artists);
        tabs.add_tab("Playlists", list_playlists);
        tabs.add_tab("Shows", list_shows);
        tabs.add_tab("Episodes", list_episodes);

        Self {
            search_term,
            results_tracks,
            pagination_tracks,
            results_albums,
            pagination_albums,
            results_artists,
            pagination_artists,
            results_playlists,
            pagination_playlists,
            results_shows,
            pagination_shows,
            results_episodes,
            pagination_episodes,
            top,
            to_tracks: Arc::new(AtomicBool::new(false)),
            rows_per_kind,
            tabs,
            spotify: queue.get_spotify(),
            events,
        }
    }

    fn get_track(
        spotify: &Spotify,
        tracks: &Arc<RwLock<Vec<Track>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> u32 {
        if let Ok(results) = spotify.api.track(query) {
            let t = vec![(&results).into()];
            let mut r = tracks.write();
            *r = t;
            return 1;
        }
        0
    }

    fn search_track(
        spotify: &Spotify,
        tracks: &Arc<RwLock<Vec<Track>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> u32 {
        if let Ok(SearchResult::Tracks(results)) =
            spotify
                .api
                .search(SearchType::Track, query, 50, offset as u32)
        {
            let mut t = results.items.iter().map(|ft| ft.into()).collect();
            let mut r = tracks.write();

            if append {
                r.append(&mut t);
            } else {
                *r = t;
            }
            return results.total;
        }
        0
    }

    fn get_album(
        spotify: &Spotify,
        albums: &Arc<RwLock<Vec<Album>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> u32 {
        if let Ok(results) = spotify.api.album(query) {
            let a = vec![(&results).into()];
            let mut r = albums.write();
            *r = a;
            return 1;
        }
        0
    }

    fn search_album(
        spotify: &Spotify,
        albums: &Arc<RwLock<Vec<Album>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> u32 {
        if let Ok(SearchResult::Albums(results)) =
            spotify
                .api
                .search(SearchType::Album, query, 50, offset as u32)
        {
            let mut a = results.items.iter().map(|sa| sa.into()).collect();
            let mut r = albums.write();

            if append {
                r.append(&mut a);
            } else {
                *r = a;
            }
            return results.total;
        }
        0
    }

    fn get_artist(
        spotify: &Spotify,
        artists: &Arc<RwLock<Vec<Artist>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> u32 {
        if let Ok(results) = spotify.api.artist(query) {
            let a = vec![(&results).into()];
            let mut r = artists.write();
            *r = a;
            return 1;
        }
        0
    }

    fn search_artist(
        spotify: &Spotify,
        artists: &Arc<RwLock<Vec<Artist>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> u32 {
        if let Ok(SearchResult::Artists(results)) =
            spotify
                .api
                .search(SearchType::Artist, query, 50, offset as u32)
        {
            let mut a = results.items.iter().map(|fa| fa.into()).collect();
            let mut r = artists.write();

            if append {
                r.append(&mut a);
            } else {
                *r = a;
            }
            return results.total;
        }
        0
    }

    fn get_playlist(
        spotify: &Spotify,
        playlists: &Arc<RwLock<Vec<Playlist>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> u32 {
        if let Ok(result) = spotify.api.playlist(query).as_ref() {
            let pls = vec![result.into()];
            let mut r = playlists.write();
            *r = pls;
            return 1;
        }
        0
    }

    fn search_playlist(
        spotify: &Spotify,
        playlists: &Arc<RwLock<Vec<Playlist>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> u32 {
        if let Ok(SearchResult::Playlists(results)) =
            spotify
                .api
                .search(SearchType::Playlist, query, 50, offset as u32)
        {
            let mut pls = results.items.iter().map(|sp| sp.into()).collect();
            let mut r = playlists.write();

            if append {
                r.append(&mut pls);
            } else {
                *r = pls;
            }
            return results.total;
        }
        0
    }

    fn get_show(
        spotify: &Spotify,
        shows: &Arc<RwLock<Vec<Show>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> u32 {
        if let Ok(result) = spotify.api.show(query).as_ref() {
            let pls = vec![result.into()];
            let mut r = shows.write();
            *r = pls;
            return 1;
        }
        0
    }

    fn search_show(
        spotify: &Spotify,
        shows: &Arc<RwLock<Vec<Show>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> u32 {
        if let Ok(SearchResult::Shows(results)) =
            spotify
                .api
                .search(SearchType::Show, query, 50, offset as u32)
        {
            let mut pls = results.items.iter().map(|sp| sp.into()).collect();
            let mut r = shows.write();

            if append {
                r.append(&mut pls);
            } else {
                *r = pls;
            }
            return results.total;
        }
        0
    }

    fn get_episode(
        spotify: &Spotify,
        episodes: &Arc<RwLock<Vec<Episode>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> u32 {
        if let Ok(result) = spotify.api.episode(query).as_ref() {
            let e = vec![result.into()];
            let mut r = episodes.write();
            *r = e;
            return 1;
        }
        0
    }

    fn search_episode(
        spotify: &Spotify,
        episodes: &Arc<RwLock<Vec<Episode>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> u32 {
        if let Ok(SearchResult::Episodes(results)) =
            spotify
                .api
                .search(SearchType::Episode, query, 50, offset as u32)
        {
            let mut e = results.items.iter().map(|se| se.into()).collect();
            let mut r = episodes.write();

            if append {
                r.append(&mut e);
            } else {
                *r = e;
            }
            return results.total;
        }
        0
    }

    /// Run one search on its own thread, fill `results` and, for a search that pages, register
    /// a paginator that asks for more rows by row count.
    fn perform_search<I: ListItem + Clone>(
        spotify: Spotify,
        events: EventManager,
        handler: SearchHandler<I>,
        results: Arc<RwLock<Vec<I>>>,
        query: String,
        paginator: Option<Pagination<I>>,
    ) {
        // Taken before the thread starts, so the app does not look idle between the caller
        // letting go of its own guard and this search sending its request.
        let activity = crate::activity::ACTIVITY.begin();
        std::thread::spawn(move || {
            let _activity = activity;
            let total_items = handler(&spotify, &results, &query, 0, false) as usize;

            // register paginator if the API has more than one page of results
            if let Some(mut paginator) = paginator {
                let loaded_items = results.read().len();
                if total_items > loaded_items {
                    let events = events.clone();

                    // paginator callback
                    let cb = move |items: Arc<RwLock<Vec<I>>>| {
                        let offset = items.read().len();
                        handler(&spotify, &results, &query, offset, true);
                        events.trigger();
                    };
                    paginator.set(loaded_items, total_items, Box::new(cb));
                } else {
                    paginator.clear()
                }
            }
            events.trigger();
        });
    }

    /// `perform_search` for a single item looked up by id, which does not page.
    fn lookup<I: ListItem + Clone>(
        &self,
        handler: SearchHandler<I>,
        results: &Arc<RwLock<Vec<I>>>,
        query: &str,
    ) {
        Self::perform_search(
            self.spotify.clone(),
            self.events.clone(),
            handler,
            results.clone(),
            query.to_owned(),
            None,
        );
    }

    fn search_tabs(&self) -> SearchTabs {
        SearchTabs {
            tracks: TabHandle {
                rows: self.results_tracks.clone(),
                pagination: self.pagination_tracks.clone(),
            },
            albums: TabHandle {
                rows: self.results_albums.clone(),
                pagination: self.pagination_albums.clone(),
            },
            artists: TabHandle {
                rows: self.results_artists.clone(),
                pagination: self.pagination_artists.clone(),
            },
            playlists: TabHandle {
                rows: self.results_playlists.clone(),
                pagination: self.pagination_playlists.clone(),
            },
            shows: TabHandle {
                rows: self.results_shows.clone(),
                pagination: self.pagination_shows.clone(),
            },
            episodes: TabHandle {
                rows: self.results_episodes.clone(),
                pagination: self.pagination_episodes.clone(),
            },
        }
    }

    /// Search for `term` on a background thread: the internal API serves the first page of
    /// every tab when it is in use, today's six Web API searches run when it is not, and the
    /// tabs stay empty when it failed. A Top tab gets its rows after the six tabs are shown,
    /// and asks for the move to Tracks when it stays without an item row or the internal API
    /// was not used.
    fn search_text(&self, term: String) {
        // Taken here so the app does not look idle between the search command and the first
        // request; dropped once the tabs are written.
        let activity = crate::activity::ACTIVITY.begin();
        let tabs = self.search_tabs();
        let spotify = self.spotify.clone();
        let events = self.events.clone();
        let top_rows = self.top.clone();
        let to_tracks = self.to_tracks.clone();
        let rows_per_kind = self.rows_per_kind;

        std::thread::spawn(move || {
            let _activity = activity;
            let page = spotify.api.search_first_page(&term);
            finish_text_search(
                page,
                tabs,
                top_rows.as_deref(),
                &to_tracks,
                &term,
                rows_per_kind,
                &spotify,
                &events,
                |draft| spotify.api.search_top_strays(draft),
                || events.trigger(),
                |tabs| {
                    Self::refresh_web_token(&spotify);
                    Self::search_web_api(&spotify, &events, tabs, &term);
                },
            );
        });
    }

    /// Today's six Web API searches, one thread per tab.
    fn search_web_api(spotify: &Spotify, events: &EventManager, tabs: SearchTabs, term: &str) {
        Self::perform_search(
            spotify.clone(),
            events.clone(),
            Box::new(Self::search_track),
            tabs.tracks.rows,
            term.to_owned(),
            Some(tabs.tracks.pagination),
        );
        Self::perform_search(
            spotify.clone(),
            events.clone(),
            Box::new(Self::search_album),
            tabs.albums.rows,
            term.to_owned(),
            Some(tabs.albums.pagination),
        );
        Self::perform_search(
            spotify.clone(),
            events.clone(),
            Box::new(Self::search_artist),
            tabs.artists.rows,
            term.to_owned(),
            Some(tabs.artists.pagination),
        );
        Self::perform_search(
            spotify.clone(),
            events.clone(),
            Box::new(Self::search_playlist),
            tabs.playlists.rows,
            term.to_owned(),
            Some(tabs.playlists.pagination),
        );
        Self::perform_search(
            spotify.clone(),
            events.clone(),
            Box::new(Self::search_show),
            tabs.shows.rows,
            term.to_owned(),
            Some(tabs.shows.pagination),
        );
        Self::perform_search(
            spotify.clone(),
            events.clone(),
            Box::new(Self::search_episode),
            tabs.episodes.rows,
            term.to_owned(),
            Some(tabs.episodes.pagination),
        );
    }

    /// Check if a Web API token refresh is necessary before commencing multiple requests to
    /// avoid deadlock, as the parallel requests might simultaneously try to refresh the token.
    /// Blocks on the refresh.
    fn refresh_web_token(spotify: &Spotify) {
        spotify
            .api
            .update_token()
            .map(move |h| ASYNC_RUNTIME.get().unwrap().block_on(h).ok());
    }

    /// Show the item `id` of `uri_type` in its tab and select that tab.
    fn lookup_uri(&mut self, uri_type: UriType, id: &str) {
        match uri_type {
            UriType::Track => self.lookup(Box::new(Self::get_track), &self.results_tracks, id),
            UriType::Album => self.lookup(Box::new(Self::get_album), &self.results_albums, id),
            UriType::Artist => self.lookup(Box::new(Self::get_artist), &self.results_artists, id),
            UriType::Playlist => {
                self.lookup(Box::new(Self::get_playlist), &self.results_playlists, id)
            }
            UriType::Show => self.lookup(Box::new(Self::get_show), &self.results_shows, id),
            UriType::Episode => {
                self.lookup(Box::new(Self::get_episode), &self.results_episodes, id)
            }
        }
        self.tabs.set_selected(uri_tab(uri_type));
    }

    fn run_search(&mut self, term: TermKind) {
        match term {
            TermKind::Uri(uri_type) => {
                Self::refresh_web_token(&self.spotify);
                let query = self.search_term.clone();
                self.lookup_uri(uri_type, &query);
            }
            // A Spotify URL, like https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC
            TermKind::Url(url) => {
                Self::refresh_web_token(&self.spotify);
                self.lookup_uri(url.uri_type, &url.id);
            }
            TermKind::Text => self.search_text(self.search_term.clone()),
        }
    }
}

impl ViewWrapper for SearchResultsView {
    wrap_impl!(self.tabs: TabbedView);

    /// Apply a move to Tracks the search thread asked for, then lay the tabs out.
    fn wrap_layout(&mut self, size: Vec2) {
        let asked = self.to_tracks.swap(false, Ordering::Relaxed);
        if let Some(index) = tracks_hop(asked, self.tabs.moved_by_user(), self.top.is_some()) {
            self.tabs.set_selected(index);
        }
        self.tabs.layout(size);
    }
}

impl ViewExt for SearchResultsView {
    #[cfg(feature = "harness")]
    fn harness_state(&self) -> Option<crate::harness::HarnessState> {
        let mut state = self.tabs.harness_state()?;
        state.view = "search".into();
        Some(state)
    }

    fn title(&self) -> String {
        format!("Search: {}", self.search_term)
    }
    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        self.tabs.on_command(s, cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{MoveAmount, MoveMode};
    use crate::config::{Config, ConfigValues};
    use crate::model::playable::Playable;
    use crate::save_log::test_items;
    use crate::search_api::TopRow;
    use crate::ui::pagination::Position;
    use rspotify::model::{AlbumId, FullTrack, SimplifiedAlbum, TrackId, Type};
    use std::collections::HashMap;

    fn track(id: &str) -> Track {
        test_items::track(id, &[])
    }

    fn ids<I: ListItem>(rows: &[I]) -> Vec<String> {
        rows.iter().filter_map(|row| row.id()).collect()
    }

    #[test]
    fn append_skips_rows_already_shown() {
        let mut rows = vec![track("a"), track("b")];
        let added = append_new(&mut rows, vec![track("b"), track("c"), track("c")]);
        assert_eq!(added, 1);
        assert_eq!(ids(&rows), ["a", "b", "c"]);
    }

    #[test]
    fn append_to_an_empty_list_keeps_order() {
        let mut rows = Vec::new();
        assert_eq!(append_new(&mut rows, vec![track("x"), track("y")]), 2);
        assert_eq!(ids(&rows), ["x", "y"]);
    }

    #[test]
    fn a_failed_first_page_posts_a_note_without_the_term() {
        let events = EventManager::new_for_test();
        report_failed_search(&events);
        let notes: Vec<String> = events
            .msg_iter()
            .filter_map(|event| match event {
                Event::StatusNote(note) => Some(note),
                _ => None,
            })
            .collect();
        assert_eq!(notes, [SEARCH_FAILED_NOTE]);
    }

    #[test]
    fn a_failed_page_keeps_the_offset_and_an_ended_one_stops() {
        let rows = RwLock::new(vec![track("a")]);
        let pos = Position::default();
        pos.set(Some(50), 180);
        apply_next_page(&rows, &pos, NextOutcome::<Track>::Failed);
        assert_eq!(pos.next_offset(), Some(50));
        apply_next_page(
            &rows,
            &pos,
            NextOutcome::Page {
                rows: vec![track("a"), track("b")],
                next_offset: Some(100),
                total: 171,
            },
        );
        assert_eq!((rows.read().len(), pos.next_offset()), (2, Some(100)));
        apply_next_page(&rows, &pos, NextOutcome::<Track>::Ended);
        assert_eq!(pos.next_offset(), None);
    }

    fn setup() -> (EventManager, Arc<Queue>, Arc<Library>) {
        let cfg = Config::new_for_test();
        let ev = EventManager::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        let library = Library::new_for_test(ev.clone(), spotify.clone(), cfg.clone());
        let queue = Arc::new(Queue::new(spotify, cfg, library.clone()));
        (ev, queue, library)
    }

    fn view(with_top: bool) -> SearchResultsView {
        let (ev, queue, library) = setup();
        SearchResultsView::build("x".into(), ev, queue, library, with_top)
    }

    /// Like `view(true)`, with a config holding `values`.
    fn view_with_values(values: ConfigValues) -> SearchResultsView {
        let cfg = Config::new_for_test_with_values(values);
        let ev = EventManager::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        let library = Library::new_for_test(ev.clone(), spotify.clone(), cfg.clone());
        let queue = Arc::new(Queue::new(spotify, cfg, library.clone()));
        SearchResultsView::build("x".into(), ev, queue, library, true)
    }

    fn tab_names(view: &SearchResultsView) -> Vec<String> {
        view.tabs.names_for_test()
    }

    fn selected_name(view: &SearchResultsView) -> String {
        tab_names(view)[view.tabs.selected()].clone()
    }

    fn move_tab(view: &mut SearchResultsView, mode: MoveMode) {
        let mut siv = Cursive::new();
        view.on_command(&mut siv, &Command::Move(mode, MoveAmount::Integer(1)))
            .unwrap();
    }

    fn lay_out(view: &mut SearchResultsView) {
        view.layout(Vec2::new(80, 24));
    }

    const SIX: [&str; 6] = [
        "Tracks",
        "Albums",
        "Artists",
        "Playlists",
        "Shows",
        "Episodes",
    ];
    const URI_TYPES: [UriType; 6] = [
        UriType::Track,
        UriType::Album,
        UriType::Artist,
        UriType::Playlist,
        UriType::Show,
        UriType::Episode,
    ];

    /// An album row with nothing set but its id.
    fn album_row(id: &str) -> SimplifiedAlbum {
        SimplifiedAlbum {
            id: Some(AlbumId::from_id(id.to_string()).unwrap()),
            ..Default::default()
        }
    }

    /// The ids of `items`, `rule` for the rule row.
    fn item_ids(items: &[TopItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| i.id().unwrap_or_else(|| "rule".into()))
            .collect()
    }

    #[test]
    fn term_kind_tells_uri_url_and_text_apart() {
        assert!(matches!(
            term_kind("spotify:track:t1"),
            TermKind::Uri(UriType::Track)
        ));
        assert!(matches!(
            term_kind("spotify:user:u1:playlist:p1"),
            TermKind::Uri(UriType::Playlist)
        ));
        assert!(matches!(
            term_kind("https://open.spotify.com/album/b1"),
            TermKind::Url(SpotifyUrl {
                uri_type: UriType::Album,
                ..
            })
        ));
        assert!(matches!(term_kind("some words"), TermKind::Text));
        assert!(matches!(
            term_kind("https://example.com/album/b1"),
            TermKind::Text
        ));
    }

    #[test]
    fn tracks_hop_table() {
        assert_eq!(tracks_hop(true, false, true), Some(1));
        for (asked, moved, has_top) in [
            (false, false, true),
            (true, true, true),
            (true, false, false),
            (false, true, false),
        ] {
            assert_eq!(tracks_hop(asked, moved, has_top), None);
        }
    }

    #[test]
    fn a_view_built_with_top_has_seven_tabs_and_top_selected() {
        let view = view(true);
        let mut names = vec!["Top"];
        names.extend(SIX);
        assert_eq!(tab_names(&view), names);
        assert_eq!(selected_name(&view), "Top");
    }

    #[test]
    fn a_view_built_without_top_is_todays_six_tabs_on_tracks() {
        let view = view(false);
        assert_eq!(tab_names(&view), SIX);
        assert_eq!(selected_name(&view), "Tracks");
        assert!(view.top.is_none());
    }

    #[test]
    fn a_uri_search_selects_by_todays_index_in_a_six_tab_view() {
        let names = tab_names(&view(false));
        for (uri_type, name) in URI_TYPES.into_iter().zip(SIX) {
            assert_eq!(names[uri_tab(uri_type)], name);
        }
    }

    #[test]
    fn a_move_to_tracks_applies_once_on_layout_and_not_after_a_user_move() {
        let mut first = view(true);
        first.to_tracks.store(true, Ordering::Relaxed);
        lay_out(&mut first);
        assert_eq!(selected_name(&first), "Tracks");
        assert!(!first.to_tracks.load(Ordering::Relaxed));
        move_tab(&mut first, MoveMode::Left);
        lay_out(&mut first);
        assert_eq!(selected_name(&first), "Top", "the move applies once");

        let mut second = view(true);
        move_tab(&mut second, MoveMode::Right);
        move_tab(&mut second, MoveMode::Left);
        second.to_tracks.store(true, Ordering::Relaxed);
        lay_out(&mut second);
        assert_eq!(selected_name(&second), "Top");
        assert!(!second.to_tracks.load(Ordering::Relaxed));
    }

    #[test]
    fn a_move_asked_for_one_view_leaves_another_view_alone() {
        let first = view(true);
        let mut second = view(true);
        first.to_tracks.store(true, Ordering::Relaxed);
        lay_out(&mut second);
        assert_eq!(selected_name(&second), "Top");
    }

    #[test]
    fn a_late_answer_for_a_dropped_view_lands_in_that_views_state_only() {
        let dropped = view(true);
        let mut live = view(true);
        let tabs = dropped.search_tabs();
        let albums = dropped.results_albums.clone();
        let top = dropped.top.clone().unwrap();
        let flag = dropped.to_tracks.clone();
        let (term, spotify, events) = (
            dropped.search_term.clone(),
            dropped.spotify.clone(),
            dropped.events.clone(),
        );
        drop(dropped);
        // An empty Top tab: the answer asks the dropped view for the move to Tracks.
        finish_text_search(
            internal_page(TopDraft::default()),
            tabs,
            Some(&top),
            &flag,
            &term,
            0,
            &spotify,
            &events,
            |_| panic!("no stray to fetch"),
            || {},
            |_| panic!("no Web API search"),
        );
        assert_eq!(albums.read().len(), 1);
        assert!(top.read().is_empty());
        assert!(flag.load(Ordering::Relaxed));

        assert!(!live.to_tracks.load(Ordering::Relaxed));
        lay_out(&mut live);
        assert_eq!(selected_name(&live), "Top");
        assert!(live.results_albums.read().is_empty());
        assert!(live.results_playlists.read().is_empty());
        assert!(live.top.as_ref().unwrap().read().is_empty());
        assert_eq!(totals(&live), [None; 6]);
    }

    #[test]
    fn fill_top_writes_once_in_the_answers_order() {
        let draft = TopDraft::for_test(
            vec![
                Ok(TopRow::Playlist(test_items::playlist("p1"))),
                Err((Kind::Album, "spotify:album:b2".into())),
                Ok(TopRow::Playlist(test_items::playlist("p3"))),
            ],
            vec![test_items::playlist("f1")],
        );
        let rows = RwLock::new(Vec::new());
        let flag = AtomicBool::new(false);
        fill_top(
            draft,
            Vec::new(),
            |d| {
                assert!(rows.read().is_empty(), "written before the strays settled");
                d.settle(Kind::Album, Some(Rows::Albums(vec![album_row("b2")])));
            },
            &rows,
            &flag,
        );
        assert_eq!(item_ids(&rows.read()), ["p1", "b2", "p3", "rule", "f1"]);
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn fill_top_skips_the_fetch_without_strays() {
        let draft = TopDraft::for_test(
            vec![Ok(TopRow::Playlist(test_items::playlist("p1")))],
            Vec::new(),
        );
        let rows = RwLock::new(Vec::new());
        let flag = AtomicBool::new(false);
        fill_top(
            draft,
            Vec::new(),
            |_| panic!("no stray to fetch"),
            &rows,
            &flag,
        );
        assert_eq!(item_ids(&rows.read()), ["p1"]);
    }

    #[test]
    fn an_empty_top_tab_asks_for_the_move_to_tracks() {
        let draft = TopDraft::for_test(
            vec![Err((Kind::Album, "spotify:album:b2".into()))],
            Vec::new(),
        );
        let rows = RwLock::new(Vec::new());
        let flag = AtomicBool::new(false);
        fill_top(
            draft,
            Vec::new(),
            |d| d.settle(Kind::Album, None),
            &rows,
            &flag,
        );
        assert!(rows.read().is_empty());
        assert!(flag.load(Ordering::Relaxed));

        let draft = TopDraft::for_test(Vec::new(), vec![test_items::playlist("f1")]);
        let flag = AtomicBool::new(false);
        fill_top(draft, Vec::new(), |_| {}, &rows, &flag);
        assert!(
            !flag.load(Ordering::Relaxed),
            "a featured playlist is an item row"
        );
    }

    /// A track row with nothing set but the id and name.
    #[allow(deprecated)] // rspotify marks fields Spotify removed as deprecated; they must be set.
    fn track_row(id: &str) -> FullTrack {
        FullTrack {
            album: SimplifiedAlbum::default(),
            artists: Vec::new(),
            available_markets: Vec::new(),
            disc_number: 0,
            duration: chrono::Duration::zero(),
            explicit: false,
            external_ids: HashMap::new(),
            external_urls: HashMap::new(),
            href: None,
            id: Some(TrackId::from_id(id.to_string()).unwrap()),
            is_local: false,
            is_playable: None,
            linked_from: None,
            restrictions: None,
            name: id.to_string(),
            popularity: 0,
            preview_url: None,
            track_number: 0,
            r#type: Type::Track,
        }
    }

    #[test]
    fn fill_top_writes_the_sections_between_the_top_rows_and_featured() {
        let draft = TopDraft::for_test(
            vec![Ok(TopRow::Album(album_row("b1")))],
            vec![test_items::playlist("p9")],
        );
        let sections = vec![
            (
                Kind::Album,
                vec![
                    TopRow::Album(album_row("b1")),
                    TopRow::Album(album_row("b2")),
                ],
            ),
            (
                Kind::Playlist,
                vec![TopRow::Playlist(test_items::playlist("p1"))],
            ),
        ];
        let rows = RwLock::new(Vec::new());
        let flag = AtomicBool::new(false);
        fill_top(draft, sections, |_| {}, &rows, &flag);
        assert_eq!(
            item_ids(&rows.read()),
            ["b1", "rule", "b1", "b2", "rule", "p1", "rule", "p9"]
        );
        let items = rows.read();
        assert!(matches!(&items[1], TopItem::Rule(label) if label == "Albums"));
        assert!(matches!(&items[4], TopItem::Rule(label) if label == "Playlists"));
        assert!(matches!(&items[6], TopItem::Rule(label) if label == "Featured playlists"));
    }

    #[test]
    fn fill_top_shows_the_sections_when_the_stray_fetch_fails() {
        let draft = TopDraft::for_test(
            vec![Err((Kind::Album, "spotify:album:b2".into()))],
            Vec::new(),
        );
        let sections = vec![(Kind::Album, vec![TopRow::Album(album_row("b1"))])];
        let rows = RwLock::new(Vec::new());
        let flag = AtomicBool::new(false);
        fill_top(
            draft,
            sections,
            |d| d.settle(Kind::Album, None),
            &rows,
            &flag,
        );
        assert_eq!(item_ids(&rows.read()), ["rule", "b1"]);
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn a_top_tab_with_only_section_rows_does_not_ask_for_tracks() {
        let sections = vec![(Kind::Album, vec![TopRow::Album(album_row("b1"))])];
        let rows = RwLock::new(Vec::new());
        let flag = AtomicBool::new(false);
        fill_top(TopDraft::default(), sections, |_| {}, &rows, &flag);
        assert_eq!(item_ids(&rows.read()), ["rule", "b1"]);
        assert!(!flag.load(Ordering::Relaxed));
    }

    /// An internal first page of 5 tracks (t0..t4) and 2 albums (b0, b1), other kinds empty.
    fn tracks_and_albums_page() -> FirstPage {
        let tabs = Kind::ALL
            .into_iter()
            .map(|kind| {
                let rows = match kind {
                    Kind::Track => {
                        Rows::Tracks((0..5).map(|i| track_row(&format!("t{i}"))).collect())
                    }
                    Kind::Album => {
                        Rows::Albums((0..2).map(|i| album_row(&format!("b{i}"))).collect())
                    }
                    _ => Rows::empty(kind),
                };
                let page = TabPage {
                    rows,
                    next_offset: None,
                    total: 0,
                };
                (kind, page)
            })
            .collect();
        FirstPage::Internal {
            tabs,
            top: TopDraft::default(),
        }
    }

    /// Run `finish_text_search` on `view`'s handles for `page` with `rows_per_kind`.
    fn finish_with_count(view: &SearchResultsView, page: FirstPage, rows_per_kind: usize) {
        finish_text_search(
            page,
            view.search_tabs(),
            view.top.as_deref(),
            &view.to_tracks,
            &view.search_term,
            rows_per_kind,
            &view.spotify,
            &view.events,
            |_| panic!("no stray to fetch"),
            || {},
            |_| panic!("no Web API search"),
        );
    }

    #[test]
    fn the_text_search_cuts_the_sections_and_still_fills_every_tab_row() {
        let view = view(true);
        finish_with_count(&view, tracks_and_albums_page(), 3);
        assert_eq!(view.results_tracks.read().len(), 5);
        assert_eq!(view.results_albums.read().len(), 2);
        assert_eq!(
            item_ids(&view.top.as_ref().unwrap().read()),
            ["rule", "t0", "t1", "t2", "rule", "b0", "b1"]
        );
        assert!(!view.to_tracks.load(Ordering::Relaxed));
    }

    #[test]
    fn rows_per_kind_zero_gives_the_top_tab_without_sections() {
        let view = view(true);
        finish_with_count(&view, tracks_and_albums_page(), 0);
        assert_eq!(view.results_tracks.read().len(), 5);
        assert!(view.top.as_ref().unwrap().read().is_empty());
        assert!(view.to_tracks.load(Ordering::Relaxed));
    }

    #[test]
    fn a_search_view_takes_its_rows_per_kind_from_the_config() {
        let values = ConfigValues {
            search_top_rows_per_kind: Some(5),
            ..Default::default()
        };
        assert_eq!(view_with_values(values).rows_per_kind, 5);
        assert_eq!(view_with_values(ConfigValues::default()).rows_per_kind, 3);
    }

    #[test]
    fn after_first_page_table() {
        for (class, has_top, web, flag) in [
            (FirstPageClass::Internal, true, false, false),
            (FirstPageClass::Internal, false, false, false),
            (FirstPageClass::NotInUse, true, true, true),
            (FirstPageClass::NotInUse, false, true, false),
            (FirstPageClass::Failed, true, false, false),
            (FirstPageClass::Failed, false, false, false),
        ] {
            let to_tracks = AtomicBool::new(false);
            let triggers = std::cell::Cell::new(0);
            let trigger = || {
                assert!(
                    to_tracks.load(Ordering::Relaxed),
                    "triggered before the flag"
                );
                triggers.set(triggers.get() + 1);
            };
            assert_eq!(after_first_page(class, has_top, &to_tracks, trigger), web);
            assert_eq!(to_tracks.load(Ordering::Relaxed), flag);
            assert_eq!(
                triggers.get(),
                usize::from(flag),
                "a trigger exactly when asked"
            );
        }
    }

    #[test]
    fn enter_on_a_track_row_in_a_top_list_replaces_the_queue_with_that_track() {
        let (_ev, queue, library) = setup();
        queue.append(Playable::Track(test_items::playable_track("other")));
        let content = Arc::new(RwLock::new(vec![
            TopItem::Artist(test_items::artist("a1", "A", false)),
            TopItem::Track(test_items::playable_track("t1")),
            TopItem::Track(test_items::playable_track("t2")),
        ]));
        let mut list = ListView::new(content, queue.clone(), library);
        list.move_focus_to(1);
        let mut siv = Cursive::new();
        list.on_command(&mut siv, &Command::Play).unwrap();
        let ids: Vec<Option<String>> = queue.list().read().iter().map(|p| p.id()).collect();
        assert_eq!(ids, [Some("t1".to_string())]);
    }

    #[test]
    fn six_tabs_after_a_web_served_search_and_seven_again_after_an_internal_one() {
        let api = WebApi::new(&Config::new_for_test());
        let internal = || FirstPage::Internal {
            tabs: HashMap::new(),
            top: TopDraft::default(),
        };
        let top = |term: &str| with_top(term, &term_kind(term), api.internal_search_in_use());
        api.remember_first_page_for_test(&FirstPage::NotInUse);
        assert!(!top("x"));
        api.remember_first_page_for_test(&internal());
        assert!(top("x"));
        assert!(!top("spotify:track:t1"));
        api.remember_first_page_for_test(&FirstPage::Failed);
        assert!(top("x"));
    }

    /// An internal first page with `top` as its Top tab: one album and one playlist row, and
    /// every tab its own total (100 plus its place in `Kind::ALL`).
    fn internal_page(top: TopDraft) -> FirstPage {
        let tabs = Kind::ALL
            .into_iter()
            .enumerate()
            .map(|(at, kind)| {
                let rows = match kind {
                    Kind::Album => Rows::Albums(vec![album_row("b1")]),
                    Kind::Playlist => Rows::Playlists(vec![test_items::playlist("p1")]),
                    _ => Rows::empty(kind),
                };
                let page = TabPage {
                    rows,
                    next_offset: Some(50),
                    total: 100 + at,
                };
                (kind, page)
            })
            .collect();
        FirstPage::Internal { tabs, top }
    }

    /// The total of each of the six tabs of `view`, Tracks to Episodes.
    fn totals(view: &SearchResultsView) -> [Option<usize>; 6] {
        [
            view.pagination_tracks.max_content(),
            view.pagination_albums.max_content(),
            view.pagination_artists.max_content(),
            view.pagination_playlists.max_content(),
            view.pagination_shows.max_content(),
            view.pagination_episodes.max_content(),
        ]
    }

    /// The totals of `internal_page`.
    const INTERNAL_TOTALS: [Option<usize>; 6] = [
        Some(100),
        Some(101),
        Some(102),
        Some(103),
        Some(104),
        Some(105),
    ];

    /// Run the search thread's body for `page` on `view`'s handles and return its calls in
    /// order: `strays` (then `check_strays` runs), `trigger`, and `web` (`web, flag set` when
    /// the move to Tracks was asked for by then).
    fn finish(
        view: &SearchResultsView,
        page: FirstPage,
        check_strays: impl FnOnce(&mut TopDraft),
    ) -> Vec<&'static str> {
        let calls = std::cell::RefCell::new(Vec::new());
        finish_text_search(
            page,
            view.search_tabs(),
            view.top.as_deref(),
            &view.to_tracks,
            &view.search_term,
            0,
            &view.spotify,
            &view.events,
            |draft| {
                calls.borrow_mut().push("strays");
                check_strays(draft);
            },
            || calls.borrow_mut().push("trigger"),
            |_tabs| {
                let flag = view.to_tracks.load(Ordering::Relaxed);
                calls
                    .borrow_mut()
                    .push(if flag { "web, flag set" } else { "web" });
            },
        );
        calls.into_inner()
    }

    #[test]
    fn an_internal_page_fills_the_six_tabs_before_the_strays_and_the_top_rows_after() {
        let view = view(true);
        let draft = TopDraft::for_test(
            vec![
                Ok(TopRow::Playlist(test_items::playlist("p1"))),
                Err((Kind::Album, "spotify:album:b2".into())),
            ],
            Vec::new(),
        );
        let calls = finish(&view, internal_page(draft), |d| {
            assert_eq!(totals(&view), INTERNAL_TOTALS);
            assert_eq!(view.results_albums.read().len(), 1);
            assert_eq!(view.results_playlists.read().len(), 1);
            assert!(
                view.top.as_ref().unwrap().read().is_empty(),
                "Top rows written before the strays settled"
            );
            d.settle(Kind::Album, Some(Rows::Albums(vec![album_row("b2")])));
        });
        assert_eq!(calls, ["trigger", "strays", "trigger"]);
        assert_eq!(item_ids(&view.top.as_ref().unwrap().read()), ["p1", "b2"]);
        assert!(!view.to_tracks.load(Ordering::Relaxed));
    }

    #[test]
    fn an_internal_page_without_a_top_tab_never_fetches_strays() {
        let view = view(false);
        let draft = TopDraft::for_test(vec![Err((Kind::Album, "spotify:album:b2".into()))], vec![]);
        let calls = finish(&view, internal_page(draft), |_| panic!("no stray fetch"));
        assert_eq!(calls, ["trigger"]);
        assert_eq!(totals(&view), INTERNAL_TOTALS);
        assert!(!view.to_tracks.load(Ordering::Relaxed));
    }

    #[test]
    fn a_page_not_served_internally_runs_the_web_api_search_after_the_move_to_tracks() {
        let cases: [(bool, &[&str]); 2] =
            [(true, &["trigger", "web, flag set"]), (false, &["web"])];
        for (with_top, expected) in cases {
            let view = view(with_top);
            let calls = finish(&view, FirstPage::NotInUse, |_| panic!("no stray fetch"));
            assert_eq!(calls, expected, "with_top {with_top}");
            assert_eq!(view.to_tracks.load(Ordering::Relaxed), with_top);
            assert_eq!(totals(&view), [None; 6]);
        }
    }

    #[test]
    fn a_failed_page_runs_nothing_and_leaves_the_flag_alone() {
        for with_top in [true, false] {
            let view = view(with_top);
            let calls = finish(&view, FirstPage::Failed, |_| panic!("no stray fetch"));
            assert!(calls.is_empty(), "with_top {with_top}");
            assert!(!view.to_tracks.load(Ordering::Relaxed));
            assert_eq!(totals(&view), [None; 6]);
            let notes = view
                .events
                .msg_iter()
                .filter(|event| matches!(event, Event::StatusNote(_)))
                .count();
            assert_eq!(notes, 1, "with_top {with_top}");
        }
    }

    #[test]
    fn a_new_view_decides_its_tabs_from_the_term_and_the_web_api() {
        let top = |term: &str, api: &WebApi| term_and_top(term, api).1;
        let api = WebApi::new(&Config::new_for_test());
        assert!(matches!(term_and_top("x", &api), (TermKind::Text, true)));
        assert!(!top("spotify:track:t1", &api));
        assert!(!top("https://open.spotify.com/album/b1", &api));
        for blank in ["", "   ", "\t"] {
            assert!(!top(blank, &api), "{blank:?}");
        }
        // No session: the first page goes to the Web API, and the next view has six tabs.
        assert!(matches!(api.search_first_page("x"), FirstPage::NotInUse));
        assert!(!top("x", &api));
    }

    #[test]
    fn with_top_table() {
        for (term, in_use, top) in [
            ("some words", true, true),
            ("some words", false, false),
            ("spotify:track:t1", true, false),
            ("https://open.spotify.com/album/b1", true, false),
            ("", true, false),
            ("   ", true, false),
            ("\t", true, false),
        ] {
            assert_eq!(with_top(term, &term_kind(term), in_use), top, "{term:?}");
        }
    }
}
