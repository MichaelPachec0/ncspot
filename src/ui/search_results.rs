use crate::application::ASYNC_RUNTIME;
use crate::command::Command;
use crate::commands::CommandResult;
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
use crate::search_api::{FirstPage, NextPage, Rows, TabPage, TopDraft};
use crate::spotify::{Spotify, UriType};
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

/// Whether a search for `term` gets a Top tab: a text term while the internal search is in
/// use.
fn with_top(term: &TermKind, internal_search_in_use: bool) -> bool {
    matches!(term, TermKind::Text) && internal_search_in_use
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

/// The index of the Tracks tab in a view with a Top tab.
const TRACKS_AFTER_TOP: usize = 1;

/// Pure: the tab to move to when a move to Tracks was asked for, if any.
pub(crate) fn tracks_hop(asked: bool, moved_by_user: bool, has_top: bool) -> Option<usize> {
    (asked && !moved_by_user && has_top).then_some(TRACKS_AFTER_TOP)
}

/// The Top tab's part of a finished internal first page: settle the strays with
/// `fetch_strays`, write the rows once, and ask for the move to Tracks when no item row
/// came of it.
fn fill_top(
    mut top: TopDraft,
    fetch_strays: impl FnOnce(&mut TopDraft),
    rows: &RwLock<Vec<TopItem>>,
    to_tracks: &AtomicBool,
) {
    if top.has_strays() {
        fetch_strays(&mut top);
    }
    let items = top_items(top.rows(), top.featured());
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
/// asks for the move to Tracks.
fn after_first_page(page_is: FirstPageClass, has_top: bool, to_tracks: &AtomicBool) -> bool {
    match page_is {
        FirstPageClass::Internal | FirstPageClass::Failed => false,
        FirstPageClass::NotInUse => {
            if has_top {
                to_tracks.store(true, Ordering::Relaxed);
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

impl SearchResultsView {
    pub fn new(
        search_term: String,
        events: EventManager,
        queue: Arc<Queue>,
        library: Arc<Library>,
    ) -> Self {
        let term = term_kind(&search_term);
        let top = with_top(&term, queue.get_spotify().api.internal_search_in_use());
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

        std::thread::spawn(move || {
            let _activity = activity;
            let page = spotify.api.search_first_page(&term);
            let page_is = FirstPageClass::of(&page);
            match page {
                FirstPage::Internal {
                    tabs: mut pages,
                    top,
                } => {
                    let (term, spotify, events) = (&term, &spotify, &events);
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
                    events.trigger();
                    if let Some(rows) = &top_rows {
                        fill_top(top, |d| spotify.api.search_top_strays(d), rows, &to_tracks);
                        events.trigger();
                    }
                }
                FirstPage::NotInUse => {}
                FirstPage::Failed => report_failed_search(&events),
            }
            if after_first_page(page_is, top_rows.is_some(), &to_tracks) {
                Self::refresh_web_token(&spotify);
                Self::search_web_api(&spotify, &events, tabs, &term);
            }
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

    pub fn run_search(&mut self, term: TermKind) {
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
    use crate::config::Config;
    use crate::model::playable::Playable;
    use crate::save_log::test_items;
    use crate::search_api::TopRow;
    use crate::spotify_api::WebApi;
    use crate::ui::pagination::Position;
    use rspotify::model::{AlbumId, SimplifiedAlbum};
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
    fn a_dropped_view_ignores_a_late_answer() {
        let view = view(true);
        let flag = view.to_tracks.clone();
        let rows = view.top.clone().unwrap();
        drop(view);
        fill_top(TopDraft::default(), |_| {}, &rows, &flag);
        assert!(flag.load(Ordering::Relaxed));
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
        fill_top(draft, |_| panic!("no stray to fetch"), &rows, &flag);
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
        fill_top(draft, |d| d.settle(Kind::Album, None), &rows, &flag);
        assert!(rows.read().is_empty());
        assert!(flag.load(Ordering::Relaxed));

        let draft = TopDraft::for_test(Vec::new(), vec![test_items::playlist("f1")]);
        let flag = AtomicBool::new(false);
        fill_top(draft, |_| {}, &rows, &flag);
        assert!(
            !flag.load(Ordering::Relaxed),
            "a featured playlist is an item row"
        );
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
            assert_eq!(after_first_page(class, has_top, &to_tracks), web);
            assert_eq!(to_tracks.load(Ordering::Relaxed), flag);
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
        api.remember_first_page_for_test(&FirstPage::NotInUse);
        assert!(!with_top(&term_kind("x"), api.internal_search_in_use()));
        api.remember_first_page_for_test(&internal());
        assert!(with_top(&term_kind("x"), api.internal_search_in_use()));
        assert!(!with_top(
            &term_kind("spotify:track:t1"),
            api.internal_search_in_use()
        ));
        api.remember_first_page_for_test(&FirstPage::Failed);
        assert!(with_top(&term_kind("x"), api.internal_search_in_use()));
    }
}
