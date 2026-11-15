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
use crate::model::track::Track;
use crate::queue::Queue;
use crate::search_api::{FirstPage, NextPage, Rows, TabPage};
use crate::spotify::{Spotify, UriType};
use crate::spotify_url::SpotifyUrl;
use crate::traits::{ListItem, ViewExt};
use crate::ui::listview::ListView;
use crate::ui::pagination::{Pagination, Position};
use crate::ui::tabbedview::TabbedView;
use cursive::Cursive;
use cursive::view::ViewWrapper;
use parking_lot::RwLock;
use rspotify::model::SearchType;
use rspotify::model::search::SearchResult;
use std::collections::HashSet;
use std::sync::Arc;

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
    tabs: TabbedView,
    spotify: Spotify,
    events: EventManager,
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
        let list_episodes = ListView::new(results_episodes.clone(), queue.clone(), library);
        let pagination_episodes = list_episodes.get_pagination().clone();

        let mut tabs = TabbedView::new();
        tabs.add_tab("Tracks", list_tracks);
        tabs.add_tab("Albums", list_albums);
        tabs.add_tab("Artists", list_artists);
        tabs.add_tab("Playlists", list_playlists);
        tabs.add_tab("Shows", list_shows);
        tabs.add_tab("Episodes", list_episodes);

        let mut view = Self {
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
            tabs,
            spotify: queue.get_spotify(),
            events,
        };

        view.run_search();
        view
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
    /// tabs stay empty when it failed.
    fn search_text(&self, term: String) {
        // Taken here so the app does not look idle between the search command and the first
        // request; dropped once the tabs are written.
        let activity = crate::activity::ACTIVITY.begin();
        let tabs = self.search_tabs();
        let spotify = self.spotify.clone();
        let events = self.events.clone();

        std::thread::spawn(move || {
            let _activity = activity;
            match spotify.api.search_first_page(&term) {
                FirstPage::Internal(mut pages) => {
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
                }
                FirstPage::NotInUse => {
                    Self::refresh_web_token(&spotify);
                    Self::search_web_api(&spotify, &events, tabs, &term);
                }
                FirstPage::Failed => report_failed_search(&events),
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

    pub fn run_search(&mut self) {
        let query = self.search_term.clone();

        // is the query a Spotify URI?
        if let Ok(uritype) = query.parse() {
            Self::refresh_web_token(&self.spotify);
            match uritype {
                UriType::Track => {
                    self.lookup(Box::new(Self::get_track), &self.results_tracks, &query);
                    self.tabs.set_selected(0);
                }
                UriType::Album => {
                    self.lookup(Box::new(Self::get_album), &self.results_albums, &query);
                    self.tabs.set_selected(1);
                }
                UriType::Artist => {
                    self.lookup(Box::new(Self::get_artist), &self.results_artists, &query);
                    self.tabs.set_selected(2);
                }
                UriType::Playlist => {
                    self.lookup(
                        Box::new(Self::get_playlist),
                        &self.results_playlists,
                        &query,
                    );
                    self.tabs.set_selected(3);
                }
                UriType::Show => {
                    self.lookup(Box::new(Self::get_show), &self.results_shows, &query);
                    self.tabs.set_selected(4);
                }
                UriType::Episode => {
                    self.lookup(Box::new(Self::get_episode), &self.results_episodes, &query);
                    self.tabs.set_selected(5);
                }
            }
        // Is the query a spotify URL?
        // https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC
        } else if let Some(url) = SpotifyUrl::from_url(&query) {
            Self::refresh_web_token(&self.spotify);
            match url.uri_type {
                UriType::Track => {
                    self.lookup(Box::new(Self::get_track), &self.results_tracks, &url.id);
                    self.tabs.set_selected(0);
                }
                UriType::Album => {
                    self.lookup(Box::new(Self::get_album), &self.results_albums, &url.id);
                    self.tabs.set_selected(1);
                }
                UriType::Artist => {
                    self.lookup(Box::new(Self::get_artist), &self.results_artists, &url.id);
                    self.tabs.set_selected(2);
                }
                UriType::Playlist => {
                    self.lookup(
                        Box::new(Self::get_playlist),
                        &self.results_playlists,
                        &url.id,
                    );
                    self.tabs.set_selected(3);
                }
                UriType::Show => {
                    self.lookup(Box::new(Self::get_show), &self.results_shows, &url.id);
                    self.tabs.set_selected(4);
                }
                UriType::Episode => {
                    self.lookup(Box::new(Self::get_episode), &self.results_episodes, &url.id);
                    self.tabs.set_selected(5);
                }
            }
        } else {
            self.search_text(query);
        }
    }
}

impl ViewWrapper for SearchResultsView {
    wrap_impl!(self.tabs: TabbedView);
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
    use crate::save_log::test_items;
    use crate::ui::pagination::Position;

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
}
