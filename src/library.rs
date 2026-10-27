use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::iter::Iterator;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use log::{debug, error, info, warn};
use rspotify::model::Id;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::config::{self, CACHE_VERSION, LibraryTab};
use crate::cover_prefetch;
use crate::events::EventManager;
use crate::internal_api::Area;
use crate::internal_api::collection::{SET_ARTIST, SET_COLLECTION, SET_LISTEN_LATER, SET_SHOW};
use crate::internal_api::error::InternalError;
use crate::internal_api::fallback::{Action, decide_after, run_with_fallback};
use crate::internal_api::playlist::{RootEntry, RootPlaylist};
use crate::library_collection::{
    Membership, Mode, fetch_membership, merge_followed, plan_sync, rebuild, sort_albums,
    sort_newest_first, with_artist,
};
use crate::library_sync::{
    SavedItem, SavedPage, Synced, any_added_at_missing, full_fetch_needed, sync_liked, sync_saved,
};
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::episode::Episode;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::track::Track;
use crate::playlist_edit;
use crate::playlist_items::UNAVAILABLE;
use crate::playlist_layout::{
    LayoutEntry, PLAYLIST_FETCH_SPACING, Spacer, layout_from, pinned_ids,
};
use crate::rate_limit::{MAX_SYNC_RESUMES, RESUME_MIN_DELAY, Stamp, random_jitter};
use crate::spotify::Spotify;
use crate::spotify_api::WebApi;

/// Keys of `UserState::library_full_fetch`.
const FULL_FETCH_TRACKS: &str = "tracks";
const FULL_FETCH_ALBUMS: &str = "albums";
const FULL_FETCH_SHOWS: &str = "shows";

/// Cached tracks database filename.
const CACHE_TRACKS: &str = "tracks.db";

/// Cached albums database filename.
const CACHE_ALBUMS: &str = "albums.db";

/// Cached artists database filename.
const CACHE_ARTISTS: &str = "artists.db";

/// Cached playlists database filename.
const CACHE_PLAYLISTS: &str = "playlists.db";

/// Cached Playlists tab layout (folders and pins) filename.
const CACHE_PLAYLIST_LAYOUT: &str = "playlist_layout.db";

/// The collection v2 set of pinned items.
const SET_PIN: &str = "ylpin";

/// Cached saved shows database filename.
const CACHE_SHOWS: &str = "shows.db";

/// Cached Your Episodes database filename.
const CACHE_EPISODES: &str = "episodes.db";

/// The user library with all their saved tracks, albums, playlists... High level interface to the
/// Spotify API used to manage items in the user library.
#[derive(Clone)]
pub struct Library {
    pub tracks: Arc<RwLock<Vec<Track>>>,
    pub albums: Arc<RwLock<Vec<Album>>>,
    pub artists: Arc<RwLock<Vec<Artist>>>,
    pub playlists: Arc<RwLock<Vec<Playlist>>>,
    /// The Playlists tab layout from the last complete internal sync; only meaningful while
    /// the internal API playlists area is usable.
    pub playlist_layout: Arc<RwLock<Vec<LayoutEntry>>>,
    pub shows: Arc<RwLock<Vec<Show>>>,
    /// Saved episodes (Your Episodes), filled only over the internal API.
    pub episodes: Arc<RwLock<Vec<Episode>>>,
    /// The Your Episodes tab is offered: the internal API library area is on.
    pub episodes_enabled: bool,
    pub is_done: Arc<RwLock<bool>>,
    /// Set while a library sync runs, so a second one (Shift+U twice, or during the startup
    /// sync) is skipped instead of doubling the API traffic.
    syncing: Arc<AtomicBool>,
    /// Set while an automatic resume of an incomplete sync is scheduled.
    resume_pending: Arc<AtomicBool>,
    /// Automatic resumes since the last complete sync.
    resume_count: Arc<AtomicU32>,
    pub user_id: Option<String>,
    pub display_name: Option<String>,
    ev: EventManager,
    spotify: Spotify,
    pub cfg: Arc<Config>,
}

/// Outcome of [Library::fetch_playlists] and [Library::fetch_playlists_internal].
struct PlaylistSync {
    /// The remote playlist listing was read to the end.
    listing_complete: bool,
    /// Every playlist that needed a track download got it.
    items_complete: bool,
}

impl Library {
    /// Create an empty library for use in tests. No cache is loaded and no API calls are made.
    #[cfg(test)]
    pub fn new_for_test(ev: EventManager, spotify: Spotify, cfg: Arc<Config>) -> Arc<Self> {
        Arc::new(Self {
            tracks: Arc::new(RwLock::new(Vec::new())),
            albums: Arc::new(RwLock::new(Vec::new())),
            artists: Arc::new(RwLock::new(Vec::new())),
            playlists: Arc::new(RwLock::new(Vec::new())),
            playlist_layout: Arc::new(RwLock::new(Vec::new())),
            shows: Arc::new(RwLock::new(Vec::new())),
            episodes: Arc::new(RwLock::new(Vec::new())),
            episodes_enabled: false,
            is_done: Arc::new(RwLock::new(false)),
            syncing: Arc::new(AtomicBool::new(false)),
            resume_pending: Arc::new(AtomicBool::new(false)),
            resume_count: Arc::new(AtomicU32::new(0)),
            user_id: None,
            display_name: None,
            ev,
            spotify,
            cfg,
        })
    }

    pub fn new(ev: EventManager, spotify: Spotify, cfg: Arc<Config>) -> Self {
        let current_user = spotify.api.current_user().ok();
        let user_id = current_user.as_ref().map(|u| u.id.id().to_string());
        let display_name = current_user.as_ref().and_then(|u| u.display_name.clone());

        let library = Self {
            tracks: Arc::new(RwLock::new(Vec::new())),
            albums: Arc::new(RwLock::new(Vec::new())),
            artists: Arc::new(RwLock::new(Vec::new())),
            playlists: Arc::new(RwLock::new(Vec::new())),
            playlist_layout: Arc::new(RwLock::new(Vec::new())),
            shows: Arc::new(RwLock::new(Vec::new())),
            episodes: Arc::new(RwLock::new(Vec::new())),
            episodes_enabled: spotify.api.internal_enabled(Area::Library),
            is_done: Arc::new(RwLock::new(false)),
            syncing: Arc::new(AtomicBool::new(false)),
            resume_pending: Arc::new(AtomicBool::new(false)),
            resume_count: Arc::new(AtomicU32::new(0)),
            user_id,
            display_name,
            ev,
            spotify,
            cfg,
        };

        library.update_library(false);
        library
    }

    /// Load cached items from the file at `cache_path` into the given `store`. True when a file
    /// was read and parsed; false when it is missing, unparseable or ignored for its version.
    fn load_cache<T: DeserializeOwned>(&self, cache_path: &Path, store: &mut Vec<T>) -> bool {
        let saved_cache_version = self.cfg.state().cache_version;
        if saved_cache_version < CACHE_VERSION {
            debug!(
                "Cache version for {cache_path:?} has changed from {saved_cache_version} to {CACHE_VERSION}, ignoring cache"
            );
            return false;
        }

        if let Ok(contents) = std::fs::read_to_string(cache_path) {
            debug!("loading cache from {}", cache_path.display());
            // Parse from in-memory string instead of directly from the file because it's faster.
            let parsed = serde_json::from_str::<Vec<_>>(&contents);
            match parsed {
                Ok(cache) => {
                    debug!(
                        "cache from {} loaded ({} items)",
                        cache_path.display(),
                        cache.len()
                    );
                    store.clear();
                    store.extend(cache);

                    // force refresh of UI (if visible)
                    self.trigger_redraw();
                    return true;
                }
                Err(e) => {
                    error!("can't parse cache: {e}");
                }
            }
        }
        false
    }

    /// Save the items from `store` in the file at `cache_path`. False (logged) on failure.
    fn save_cache<T: Serialize>(&self, cache_path: &Path, store: &[T]) -> bool {
        let written = File::create(cache_path)
            .map_err(|e| e.to_string())
            .and_then(|file| serde_json::to_writer(file, store).map_err(|e| e.to_string()));
        match written {
            Ok(()) => true,
            Err(e) => {
                error!("could not write cache {}: {e}", cache_path.display());
                false
            }
        }
    }

    /// The stored collection v2 sync token of `set`, if any.
    fn sync_token(&self, set: &str) -> Option<String> {
        self.cfg.state().collection_sync.get(set).cloned()
    }

    /// Remember the collection v2 sync token of `set`.
    fn set_sync_token(&self, set: &str, token: String) {
        self.cfg.with_state_mut(|state| {
            state.collection_sync.insert(set.to_string(), token.clone());
        });
    }

    /// Forget the collection v2 sync token of `set`.
    fn clear_sync_token(&self, set: &str) {
        self.cfg.with_state_mut(|state| {
            state.collection_sync.remove(set);
        });
    }

    /// Delete the playlist with the given `id` if it exists.
    pub fn delete_playlist(&self, id: &str) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        let position = self
            .playlists
            .read()
            .unwrap()
            .iter()
            .position(|i| i.id == id);

        if let Some(position) = position
            && self.spotify.api.delete_playlist(id).is_ok()
        {
            self.playlists.write().unwrap().remove(position);
            self.save_cache(
                &config::cache_path(CACHE_PLAYLISTS),
                &self.playlists.read().unwrap(),
            );
            #[cfg(feature = "mpris")]
            self.spotify
                .send_mpris(crate::mpris::MprisCommand::EmitPlaylistChanged(Some(
                    id.to_string(),
                )));
        }
    }

    /// Set the playlist with `id` to contain only `tracks`. If the playlist already contains
    /// tracks, they will be removed. Update the cache to match the new state.
    pub fn overwrite_playlist(&self, id: &str, tracks: &[Playable]) {
        debug!("saving {} tracks to list {}", tracks.len(), id);
        let api = &self.spotify.api;
        let known = self.known_playables();
        let web_used = Cell::new(false);
        let written = write_through(
            api,
            Area::Playlists,
            || {
                let stored = self
                    .playlists
                    .read()
                    .unwrap()
                    .iter()
                    .find(|p| p.id == id)
                    .cloned();
                // Not a stored playlist: nothing to verify against, use the Web API.
                let mut playlist = stored.ok_or(InternalError::NotFound)?;
                playlist_edit::overwrite(api, &mut playlist, tracks, &known).map_err(|err| {
                    if let Some(note) = err.note() {
                        warn!("{note}");
                    }
                    InternalError::from(err)
                })?;
                self.playlist_update(&playlist);
                Ok(())
            },
            || {
                web_used.set(true);
                api.overwrite_playlist(id, tracks);
                true
            },
        );

        if written && web_used.get() && self.fetch_playlists(false).listing_complete {
            self.save_cache(
                &config::cache_path(CACHE_PLAYLISTS),
                &self.playlists.read().unwrap(),
            );
        }
    }

    /// Create a playlist with the given `name` and add `tracks` to it.
    pub fn save_playlist(&self, name: &str, tracks: &[Playable]) {
        debug!("saving {} tracks to new list {}", tracks.len(), name);
        match self.spotify.api.create_playlist(name, None, None) {
            Ok(id) => self.overwrite_playlist(&id, tracks),
            Err(_) => error!("could not create new playlist.."),
        }
    }

    /// Update the local library and its cache on disk; with `force_full` every set is
    /// downloaded again instead of synced from its changes. Does nothing if a sync is already
    /// running. If the sync is left incomplete by a rate limit, a resume (not forced) is
    /// scheduled for when the limit has passed.
    pub fn update_library(&self, force_full: bool) {
        if self.start_sync() {
            self.run_update(force_full);
        }
    }

    /// The body of [Library::update_library], on a background thread; the caller holds the
    /// sync slot from [Library::start_sync].
    fn run_update(&self, force_full: bool) {
        let library = self.clone();
        thread::spawn(move || {
            let _guard = SyncGuard(library.syncing.clone());
            let sync_start = Stamp::now();

            let t_collection = {
                let library = library.clone();
                thread::spawn(move || library.sync_collection(force_full))
            };
            let t_artists = {
                let library = library.clone();
                thread::spawn(move || library.sync_artists(force_full))
            };
            let t_playlists = {
                let library = library.clone();
                thread::spawn(move || library.sync_playlists(force_full))
            };
            let t_shows = {
                let library = library.clone();
                thread::spawn(move || library.sync_shows(force_full))
            };
            let t_episodes = {
                let library = library.clone();
                thread::spawn(move || library.sync_episodes(force_full))
            };

            let collection_complete = t_collection.join().unwrap();
            let artists = t_artists.join().unwrap();
            let artists_complete = library.finish_artists(artists);

            let playlists_complete = t_playlists.join().unwrap();
            let shows_complete = t_shows.join().unwrap();
            let episodes_complete = t_episodes.join().unwrap();

            *library.is_done.write().unwrap() = true;
            library.ev.trigger();

            let complete = collection_complete
                && artists_complete
                && playlists_complete
                && shows_complete
                && episodes_complete;
            library.schedule_resume(complete, sync_start);
        });
    }

    /// Sync only what the library tab `tab` shows, from its changes, on a background thread.
    /// Like [Library::update_library] it takes the one sync slot and clears `is_done` while it
    /// runs, but it schedules no resume.
    pub fn update_tab(&self, tab: &LibraryTab) -> TabUpdate {
        let Some(set) = tab_set(tab, self.episodes_enabled) else {
            return TabUpdate::NoSet;
        };
        if !self.start_sync() {
            return TabUpdate::Busy;
        }
        info!("library tab update: {set:?}");

        let library = self.clone();
        thread::spawn(move || {
            let _guard = SyncGuard(library.syncing.clone());
            match set {
                SyncSet::Collection => {
                    library.sync_collection(false);
                    // The artists of the liked songs follow the collection.
                    library.save_artists();
                }
                SyncSet::Artists => {
                    let artists = library.sync_artists(false);
                    library.finish_artists(artists);
                }
                SyncSet::Playlists => {
                    library.sync_playlists(false);
                }
                SyncSet::Shows => {
                    library.sync_shows(false);
                }
                SyncSet::Episodes => {
                    library.sync_episodes(false);
                }
            }
            *library.is_done.write().unwrap() = true;
            library.ev.trigger();
        });
        TabUpdate::Started
    }

    /// Take the one sync slot and clear `is_done`; false (logged) if a sync already runs.
    /// The sync thread releases the slot with a [SyncGuard].
    fn start_sync(&self) -> bool {
        if self
            .syncing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            info!("library sync already running, skipped");
            return false;
        }
        *self.is_done.write().unwrap() = false;
        true
    }

    /// Load the liked songs and saved albums caches and sync the `collection` set. Returns
    /// whether the sync was complete.
    fn sync_collection(&self, force_full: bool) -> bool {
        let valid = self.load_cache(
            &config::cache_path(CACHE_TRACKS),
            self.tracks.write().unwrap().as_mut(),
        ) & self.load_cache(
            &config::cache_path(CACHE_ALBUMS),
            self.albums.write().unwrap().as_mut(),
        );
        self.fetch_collection(valid, force_full)
    }

    /// Load the artists cache and sync the followed artists. Finish with
    /// [Library::finish_artists] once the liked songs are in.
    fn sync_artists(&self, force_full: bool) -> ArtistsSync {
        let valid = self.load_cache(
            &config::cache_path(CACHE_ARTISTS),
            self.artists.write().unwrap().as_mut(),
        );
        self.fetch_artists_any(valid, force_full)
    }

    /// Rebuild the artists of the liked songs into the artists store and save `artists.db`.
    /// False (logged) if the save failed.
    fn save_artists(&self) -> bool {
        self.populate_artists();
        self.save_cache(
            &config::cache_path(CACHE_ARTISTS),
            &self.artists.read().unwrap(),
        )
    }

    /// Save the artists and finish the `artist` set of a [Library::sync_artists]. Returns
    /// whether the artists sync was complete.
    fn finish_artists(&self, artists: ArtistsSync) -> bool {
        let artists_saved = self.save_artists();
        // The token is stored only once the cache it describes is on disk.
        let synced = matches!(artists.outcome, SetOutcome::Synced(_));
        let artists_complete = artists.fetched && (artists_saved || !synced);
        self.apply_finish(
            SET_ARTIST,
            finish_set(
                artists.outcome,
                artists_saved,
                artists.mode,
                artists.cache_valid,
            ),
            artists.now,
        );
        artists_complete
    }

    /// Load the playlists cache (and the layout, over the internal API), sync the playlists
    /// and prefetch their covers. Returns whether the sync was complete.
    fn sync_playlists(&self, force_full: bool) -> bool {
        self.load_cache(
            &config::cache_path(CACHE_PLAYLISTS),
            self.playlists.write().unwrap().as_mut(),
        );
        // With the area off the layout file is ignored, not deleted.
        let internal = self.spotify.api.internal_usable(Area::Playlists);
        if internal {
            self.load_cache(
                &config::cache_path(CACHE_PLAYLIST_LAYOUT),
                self.playlist_layout.write().unwrap().as_mut(),
            );
        }
        // The internal sync saves its own caches.
        let internal_sync = internal
            .then(|| self.fetch_playlists_internal(force_full))
            .flatten();
        let sync = internal_sync.unwrap_or_else(|| {
            let sync = self.fetch_playlists(force_full);
            if sync.listing_complete {
                self.save_cache(
                    &config::cache_path(CACHE_PLAYLISTS),
                    &self.playlists.read().unwrap(),
                );
            }
            sync
        });
        if !cfg!(test) {
            let urls: Vec<String> = self
                .playlists
                .read()
                .unwrap()
                .iter()
                .filter_map(|p| p.cover_url.clone())
                .collect();
            cover_prefetch::spawn_prefetch(cover_prefetch::missing_covers(&urls, |p| p.exists()));
        }
        sync.listing_complete && sync.items_complete
    }

    /// Load the shows cache (internal API only) and sync the saved shows. Returns whether the
    /// sync was complete.
    fn sync_shows(&self, force_full: bool) -> bool {
        // With the area off nothing is loaded: the Web fetcher runs as before.
        let valid = self.spotify.api.internal_enabled(Area::Library)
            && self.load_cache(
                &config::cache_path(CACHE_SHOWS),
                self.shows.write().unwrap().as_mut(),
            );
        self.fetch_shows_any(valid, force_full)
    }

    /// Load the Your Episodes cache and sync it; nothing to do while Your Episodes is off.
    /// Returns whether the sync was complete.
    fn sync_episodes(&self, force_full: bool) -> bool {
        if !self.episodes_enabled {
            return true;
        }
        let valid = self.load_cache(
            &config::cache_path(CACHE_EPISODES),
            self.episodes.write().unwrap().as_mut(),
        );
        self.fetch_episodes(valid, force_full)
    }

    /// After a sync that started at `sync_start`: reset the resume count if it was complete,
    /// or schedule one resume if it was left incomplete by a rate limit.
    fn schedule_resume(&self, complete: bool, sync_start: Stamp) {
        if complete {
            self.resume_count.store(0, Ordering::SeqCst);
            return;
        }
        let rate_limited = self.spotify.api.rate_limited_since(sync_start);
        let count = self.resume_count.load(Ordering::SeqCst);
        let pending = self.resume_pending.load(Ordering::SeqCst);
        if !should_schedule_resume(complete, rate_limited, count, pending) {
            if !rate_limited {
                info!("library sync incomplete, not rate limited: no automatic resume");
            } else if count >= MAX_SYNC_RESUMES {
                warn!(
                    "library sync still incomplete after {MAX_SYNC_RESUMES} automatic resumes, \
                     waiting for a manual update"
                );
            } else {
                info!("library sync resume already pending");
            }
            return;
        }
        if self
            .resume_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            info!("library sync resume already pending");
            return;
        }

        let longest = self
            .spotify
            .api
            .rate_limit_status()
            .into_iter()
            .map(|(_, remaining)| remaining)
            .max()
            .unwrap_or_default();
        let delay = resume_delay(longest, random_jitter());
        info!(
            "library sync incomplete after a rate limit, resuming in {}s (attempt {}/{MAX_SYNC_RESUMES})",
            delay.as_secs(),
            count + 1
        );

        let library = self.clone();
        thread::spawn(move || {
            thread::sleep(delay);
            // While another sync (a tab update) holds the slot, this resume stays the one
            // pending: wait for the slot instead of being dropped, without counting the wait.
            loop {
                let count = library.resume_count.load(Ordering::SeqCst);
                match resume_attempt(library.start_sync(), count) {
                    Some(count) => {
                        library.resume_pending.store(false, Ordering::SeqCst);
                        library.resume_count.store(count, Ordering::SeqCst);
                        library.run_update(false);
                        break;
                    }
                    None => {
                        info!(
                            "library sync resume waiting {}s for the running sync",
                            RESUME_BUSY_RETRY.as_secs()
                        );
                        thread::sleep(RESUME_BUSY_RETRY);
                    }
                }
            }
        });
    }

    /// Returns the current Unix time and whether the list stored under `key` needs a full
    /// fetch now (always when `forced`), logging when it does. The state lock is released
    /// before returning.
    fn full_fetch_check(&self, key: &str, what: &str, forced: bool) -> (i64, bool) {
        let now = chrono::Utc::now().timestamp();
        let last = self.cfg.state().library_full_fetch.get(key).copied();
        let full = full_fetch_needed(forced, last, now);
        if forced {
            debug!("{what}: full refresh (requested)");
        } else if full {
            debug!("{what}: full refresh (cache older than 24h)");
        }
        (now, full)
    }

    /// Record that the list stored under `key` was fully fetched at `now` (Unix seconds).
    /// Persisted with the rest of the user state on quit.
    fn record_full_fetch(&self, key: &str, now: i64) {
        self.cfg.with_state_mut(|state| {
            state.library_full_fetch.insert(key.to_string(), now);
        });
    }

    /// Carry out a set's [Finish]: store or clear its token, and record a full fetch at `now`
    /// under every key of [full_fetch_keys].
    fn apply_finish(&self, set: &str, finish: Finish, now: i64) {
        match finish.token {
            TokenAction::Store(token) => self.set_sync_token(set, token),
            TokenAction::Clear => self.clear_sync_token(set),
            TokenAction::Keep => {}
        }
        if finish.record_full {
            for key in full_fetch_keys(set) {
                self.record_full_fetch(key, now);
            }
        }
    }

    /// Fetch the saved shows from the web API and save them to the local library, unless they
    /// are unchanged (always with `force_full`). Returns false, leaving the stored shows
    /// unchanged, if a page failed.
    fn fetch_shows(&self, force_full: bool) -> bool {
        debug!("loading shows");
        let stored_ids: Vec<Option<String>> = self
            .shows
            .read()
            .unwrap()
            .iter()
            .map(|show| Some(show.id.clone()))
            .collect();
        let (now, full) = self.full_fetch_check(FULL_FETCH_SHOWS, "shows", force_full);
        let synced = sync_saved(&stored_ids, full, |offset| {
            debug!("shows page at offset {offset}");
            let page = self.spotify.api.get_saved_shows(offset as u32).ok()?;
            Some(SavedPage {
                total: page.total as usize,
                has_next: page.next.is_some(),
                items: page
                    .items
                    .iter()
                    .map(|saved| SavedItem {
                        id: Some(saved.show.id.id().to_string()),
                        added_at: DateTime::parse_from_rfc3339(&saved.added_at)
                            .ok()
                            .map(|date| date.with_timezone(&Utc)),
                        value: Show::from(&saved.show),
                    })
                    .collect(),
            })
        });
        match synced {
            Synced::Full(shows) => {
                *self.shows.write().unwrap() = shows;
                self.record_full_fetch(FULL_FETCH_SHOWS, now);
                true
            }
            Synced::Unchanged => {
                debug!("shows unchanged");
                true
            }
            Synced::Prepended { .. } => unreachable!("sync_saved never prepends"),
            Synced::Failed => {
                error!("Failed to fetch shows.");
                false
            }
        }
    }

    /// Fetch the playlists from the web API and save them to the local library. This synchronizes
    /// the local version with the remote, pruning removed playlists in the process. Reports
    /// whether the remote listing was complete (if not, the cache must not be saved as
    /// authoritative) and whether every playlist that needed it had its tracks loaded (if not,
    /// the cached copies were kept and the sync is incomplete). With `force_full` every listed
    /// playlist is downloaded again, unchanged ones too.
    fn fetch_playlists(&self, force_full: bool) -> PlaylistSync {
        debug!("loading playlists");
        let known_at_start: Vec<String> = self
            .playlists
            .read()
            .unwrap()
            .iter()
            .map(|p| p.id.clone())
            .collect();
        // Read every listing page first, so a rate limit during the track
        // downloads below can never leave the listing incomplete.
        let mut listed: Vec<Playlist> = Vec::new();
        let lists_page = self.spotify.api.current_user_playlist();
        let mut lists_batch = Some(lists_page.items.read().unwrap().clone());
        while let Some(lists) = lists_batch {
            listed.extend(lists);
            lists_batch = lists_page.next();
        }
        let list_order: Vec<String> = listed.iter().map(|p| p.id.clone()).collect();

        let mut items_complete = true;
        for (index, remote) in listed.iter().enumerate() {
            // Download without holding the store lock; only the merge locks.
            let current = !needs_download(&self.playlists.read().unwrap(), remote, force_full);
            let mut downloaded = if current {
                None
            } else {
                info!("updating playlist {} (index: {})", remote.name, index);
                let mut playlist = remote.clone();
                playlist.tracks = None;
                let tracks = playlist
                    .load_tracks(&self.spotify)
                    .then(|| playlist.tracks.unwrap_or_default());
                if tracks.is_none() {
                    warn!(
                        "failed to load tracks for playlist {}, keeping cached copy",
                        remote.name
                    );
                }
                tracks
            };
            items_complete &= merge_listed(
                &mut self.playlists.write().unwrap(),
                std::slice::from_ref(remote),
                force_full,
                |_| downloaded.take(),
            );
            // trigger redraw
            self.trigger_redraw();
        }

        let listing_complete = !lists_page.failed();
        if listing_complete {
            reconcile_playlists(
                &mut self.playlists.write().unwrap(),
                &known_at_start,
                &list_order,
                true,
            );
        } else {
            warn!("playlist list incomplete, keeping cached playlists");
        }

        // trigger redraw
        self.trigger_redraw();

        // Notify MPRIS clients that the playlist set has been refreshed.
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitPlaylistChanged(None));

        PlaylistSync {
            listing_complete,
            items_complete,
        }
    }

    /// Sync the playlists from the internal API rootlist: changed playlists are downloaded
    /// [PLAYLIST_FETCH_SPACING] apart and a rate limit stops the remaining downloads. On a
    /// complete rootlist the store is pruned and sorted, and `playlists.db` and
    /// `playlist_layout.db` are saved together. With `force_full` every listed playlist is
    /// downloaded again, unchanged ones too. `None` means: use the Web path for this sync.
    fn fetch_playlists_internal(&self, force_full: bool) -> Option<PlaylistSync> {
        let api = &self.spotify.api;
        debug!("loading playlists over the internal API");
        let rootlist = match api.playlist_rootlist() {
            Ok(rootlist) => rootlist,
            Err(err) => {
                let action = decide_after(&err);
                if action != Action::Fail {
                    // The Web path runs this sync: the tab is flat for this session. The
                    // layout file is kept for the next complete internal sync.
                    self.playlist_layout.write().unwrap().clear();
                }
                return match action {
                    Action::LatchThenWeb => {
                        info!("playlists: rootlist failed ({err}), latching to the Web API");
                        api.latch_area(Area::Playlists, &err);
                        api.count_fallback(Area::Playlists);
                        None
                    }
                    Action::WebOnce => {
                        info!("playlists: rootlist failed ({err}), using the Web API this time");
                        api.count_fallback(Area::Playlists);
                        None
                    }
                    Action::Fail => {
                        warn!("playlists: rootlist failed ({err}), keeping cached playlists");
                        Some(PlaylistSync {
                            listing_complete: false,
                            items_complete: false,
                        })
                    }
                };
            }
        };
        // A stop at the page cap leaves the listing incomplete: nothing may be pruned.
        let listing_complete = rootlist.read >= rootlist.length;

        let listed: Vec<(Playlist, i32)> = rootlist
            .entries
            .iter()
            .filter_map(|entry| match entry {
                RootEntry::Playlist(p) => Some((listed_playlist(p), p.status)),
                _ => None,
            })
            .collect();
        let known_at_start: Vec<String> = self
            .playlists
            .read()
            .unwrap()
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let mut known = self.known_playables();
        let mut spacer = Spacer::new(PLAYLIST_FETCH_SPACING);
        let mut stopped = false;
        let mut items_complete = true;

        for (index, (listed_remote, status)) in listed.iter().enumerate() {
            let remote = &self.with_head_snapshot(listed_remote, *status, force_full, &mut stopped);
            // Download without holding the store lock; only the merge locks.
            let current = !needs_download(&self.playlists.read().unwrap(), remote, force_full);
            let mut downloaded = if current {
                None
            } else {
                debug!("updating playlist at index {index}");
                self.sync_playlist_download(remote, *status, &mut spacer, &mut stopped, &known)
            };
            if let Some(tracks) = &downloaded {
                for playable in tracks {
                    remember(&mut known, playable);
                }
            }
            let downloaded_any = downloaded.is_some();
            let changed = {
                let mut store = self.playlists.write().unwrap();
                let before = store
                    .iter()
                    .find(|p| p.id == remote.id)
                    .map(|p| (p.name.clone(), p.cover_url.clone(), p.num_tracks));
                let merged =
                    merge_listed(&mut store, std::slice::from_ref(remote), force_full, |_| {
                        downloaded.take()
                    });
                // A 403 playlist can never be downloaded; it does not make the sync incomplete.
                if *status != FORBIDDEN {
                    items_complete &= merged;
                }
                let refreshed = (
                    remote.name.clone(),
                    remote.cover_url.clone(),
                    remote.num_tracks,
                );
                downloaded_any || before.as_ref().is_none_or(|b| current && *b != refreshed)
            };
            if changed {
                self.trigger_redraw();
            }
        }

        if listing_complete {
            let pinned = match api.collection_page_all(Area::Playlists, SET_PIN) {
                Ok(listing) => pinned_ids(&listing.items),
                Err(err) => {
                    debug!("playlists: pins not read ({err}), keeping the previous pins");
                    self.playlist_layout
                        .read()
                        .unwrap()
                        .iter()
                        .filter_map(|entry| match entry {
                            LayoutEntry::Pinned(id) => Some(id.clone()),
                            _ => None,
                        })
                        .collect()
                }
            };
            let order: Vec<String> = listed.iter().map(|(p, _)| p.id.clone()).collect();
            reconcile_playlists(
                &mut self.playlists.write().unwrap(),
                &known_at_start,
                &order,
                true,
            );
            let layout = layout_from(&rootlist, &pinned);
            self.save_cache(
                &config::cache_path(CACHE_PLAYLISTS),
                &self.playlists.read().unwrap(),
            );
            self.save_cache(&config::cache_path(CACHE_PLAYLIST_LAYOUT), &layout);
            *self.playlist_layout.write().unwrap() = layout;
        } else {
            warn!("playlist rootlist incomplete, keeping cached playlists");
        }

        self.trigger_redraw();

        // Notify MPRIS clients that the playlist set has been refreshed.
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitPlaylistChanged(None));

        Some(PlaylistSync {
            listing_complete,
            items_complete,
        })
    }

    /// `listed` with the snapshot it is compared and stored with, see [listed_snapshot]. The
    /// head of a playlist the user does not own is read (not spaced) unless `force_full`, the
    /// entry is 403 or a rate limit has set `stopped`; a rate limited read sets `stopped`.
    fn with_head_snapshot(
        &self,
        listed: &Playlist,
        status: i32,
        force_full: bool,
        stopped: &mut bool,
    ) -> Playlist {
        let owned = is_owned(&listed.owner_id, self.user_id.as_deref());
        if owned || force_full || status == FORBIDDEN {
            return listed.clone();
        }
        let head = if *stopped {
            None
        } else {
            match self.spotify.api.playlist_head(&listed.id) {
                Ok(head) => Some(head.snapshot_id),
                Err(InternalError::RateLimited) => {
                    warn!("playlists: rate limited reading a head, stopping the head reads");
                    *stopped = true;
                    None
                }
                Err(err) => {
                    debug!("playlists: a head was not read ({err}), using the cached snapshot");
                    None
                }
            }
        };
        let cached = self
            .playlists
            .read()
            .unwrap()
            .iter()
            .find(|p| p.id == listed.id)
            .map(|p| p.snapshot_id.clone());
        let mut playlist = listed.clone();
        playlist.snapshot_id = listed_snapshot(
            &listed.snapshot_id,
            owned,
            head.as_deref(),
            cached.as_deref(),
        );
        playlist
    }

    /// One changed playlist's tracks for the internal sync, through [sync_download]. A
    /// protocol change latches the area and loads this playlist (and, the area now latched,
    /// the rest of the sync) over the Web API. Failures leave the playlist to the next sync.
    fn sync_playlist_download(
        &self,
        remote: &Playlist,
        status: i32,
        spacer: &mut Spacer,
        stopped: &mut bool,
        known: &HashMap<String, Playable>,
    ) -> Option<Vec<Playable>> {
        let api = &self.spotify.api;
        let web = || {
            let mut playlist = remote.clone();
            playlist.tracks = None;
            playlist
                .load_tracks(&self.spotify)
                .then(|| playlist.tracks.unwrap_or_default())
                .ok_or_else(|| InternalError::Transient("Web API download failed".into()))
        };
        let result = sync_download(
            spacer,
            stopped,
            status,
            Instant::now(),
            thread::sleep,
            || {
                if !api.internal_usable(Area::Playlists) {
                    return web();
                }
                match api.playlist_playables(&remote.id, known) {
                    Err(err @ InternalError::ProtocolChanged(_)) => {
                        api.latch_area(Area::Playlists, &err);
                        api.count_fallback(Area::Playlists);
                        web()
                    }
                    other => other,
                }
            },
        )?;
        match result {
            Ok(tracks) => Some(tracks),
            Err(InternalError::RateLimited) => {
                warn!("playlists: rate limited, leaving the remaining downloads to a resume");
                None
            }
            Err(err) => {
                warn!("playlists: a playlist download failed ({err}), keeping the cached copy");
                None
            }
        }
    }

    /// Track and episode metadata already known locally, by URI: the saved tracks and
    /// episodes and the tracks of every cached playlist, placeholders left out.
    pub(crate) fn known_playables(&self) -> HashMap<String, Playable> {
        let mut known = HashMap::new();
        for track in self.tracks.read().unwrap().iter() {
            remember(&mut known, &Playable::Track(track.clone()));
        }
        for episode in self.episodes.read().unwrap().iter() {
            remember(&mut known, &Playable::Episode(episode.clone()));
        }
        for playlist in self.playlists.read().unwrap().iter() {
            for playable in playlist.tracks.iter().flatten() {
                remember(&mut known, playable);
            }
        }
        known
    }

    /// Fetch the artists from the web API and save them to the local library. Returns false,
    /// leaving the stored items unchanged, if a page failed.
    fn fetch_artists(&self) -> bool {
        let mut artists: Vec<Artist> = Vec::new();
        let mut last: Option<&str> = None;
        let mut i = 0u32;

        loop {
            let page = self.spotify.api.current_user_followed_artists(last);
            debug!("artists page: {i}");
            i += 1;
            if page.is_err() {
                error!("Failed to fetch artists.");
                return false;
            }
            let page = page.unwrap();

            artists.extend(page.items.iter().map(|fa| fa.into()));

            if page.next.is_some() {
                last = artists.last().unwrap().id.as_deref();
            } else {
                break;
            }
        }

        let mut store = self.artists.write().unwrap();

        for mut artist in artists {
            let pos = store.iter().position(|a| a.id == artist.id);
            if let Some(i) = pos {
                store[i].is_followed = true;
                continue;
            }

            artist.is_followed = true;

            store.push(artist);
        }
        true
    }

    /// Add the artist with `id` and `name` to the user library, but don't sync with the API.
    /// This does not add if there is already an artist with `id`.
    fn insert_artist(&self, id: String, name: String) {
        let mut artists = self.artists.write().unwrap();

        if !artists
            .iter()
            .any(|a| a.id.as_ref().is_some_and(|value| *value == id))
        {
            let mut artist = Artist::new(id.to_string(), name.to_string());
            artist.tracks = Some(Vec::new());
            artists.push(artist);
        }
    }

    /// Sync liked songs and saved albums, which share the `collection` set and its token.
    /// Saves each cache that changed (or was not valid at load); the token is stored only if
    /// the writes that were needed worked. `force_full` downloads both lists again.
    fn fetch_collection(&self, cache_valid: bool, force_full: bool) -> bool {
        let api = &self.spotify.api;
        // A token describes its cache only: without a valid cache it must not survive a sync
        // that ends without a new one.
        if !cache_valid {
            self.clear_sync_token(SET_COLLECTION);
        }
        let token = self.sync_token(SET_COLLECTION);
        let usable = api.internal_usable(Area::Library);
        let (now, full_due) = if usable {
            self.full_fetch_check(SET_COLLECTION, "collection", force_full)
        } else {
            (0, false)
        };
        let mode = plan_sync(usable, full_due, token.is_some(), cache_valid);
        // Mode::Web never goes through `outcome`: an area that is simply off must not latch.
        let mut changes = Changes::default();
        let outcome = if mode == Mode::Web {
            SetOutcome::Web
        } else {
            let result =
                self.collection_internal(mode, token.as_deref())
                    .map(|(token, internal)| {
                        changes = internal;
                        token
                    });
            outcome(api, SET_COLLECTION, result)
        };
        let fetched = match outcome {
            SetOutcome::Web => {
                // Two Web fetchers, both always run, in parallel.
                let (tracks, albums) = thread::scope(|scope| {
                    let tracks = scope.spawn(|| self.fetch_tracks(force_full));
                    let albums = scope.spawn(|| self.fetch_albums(force_full));
                    (tracks.join().unwrap(), albums.join().unwrap())
                });
                changes = Changes {
                    tracks: tracks == Fetch::Changed,
                    albums: albums == Fetch::Changed,
                };
                tracks != Fetch::Failed && albums != Fetch::Failed
            }
            SetOutcome::Incomplete => false,
            SetOutcome::Synced(_) => true,
        };
        // A skipped write counts as saved: the file on disk already matches the store.
        let tracks_saved = !cache_write_needed(cache_valid, changes.tracks)
            || self.save_cache(
                &config::cache_path(CACHE_TRACKS),
                &self.tracks.read().unwrap(),
            );
        let albums_saved = !cache_write_needed(cache_valid, changes.albums)
            || self.save_cache(
                &config::cache_path(CACHE_ALBUMS),
                &self.albums.read().unwrap(),
            );
        let saved = tracks_saved && albums_saved;
        let synced = matches!(outcome, SetOutcome::Synced(_));
        self.apply_finish(
            SET_COLLECTION,
            finish_set(outcome, saved, mode, cache_valid),
            now,
        );
        fetched && (saved || !synced)
    }

    /// Sync the `collection` set over the internal API into the in-memory tracks and albums.
    /// Returns the new sync token, to store once the caches are written, and which stores
    /// changed.
    fn collection_internal(
        &self,
        mode: Mode,
        token: Option<&str>,
    ) -> Result<(String, Changes), InternalError> {
        let api = &self.spotify.api;
        let fetched = fetch_membership(api, SET_COLLECTION, mode, token)?;
        let refetch = mode == Mode::Full;
        let track_members = fetched.membership.only("track");
        let album_members = fetched.membership.only("album");
        let tracks_now = self.tracks.read().unwrap().clone();
        let mut tracks = rebuild(&tracks_now, &track_members, refetch, |u| {
            api.library_tracks(u)
        })?;
        let albums_now = self.albums.read().unwrap().clone();
        let mut albums: Vec<Album> = rebuild(&albums_now, &album_members, refetch, |u| {
            api.library_albums(u)
        })?
        .into_iter()
        .map(with_artist)
        .collect();
        sort_newest_first(&mut tracks, |t| t.added_at);
        sort_albums(&mut albums);
        info!(
            "collection: {}, {} tracks, {} albums",
            describe_sync(mode, &[&track_members, &album_members]),
            tracks.len(),
            albums.len()
        );
        *self.tracks.write().unwrap() = tracks;
        *self.albums.write().unwrap() = albums;
        // An empty delta leaves a store exactly as it was.
        let changes = Changes {
            tracks: !track_members.is_empty_delta(),
            albums: !album_members.is_empty_delta(),
        };
        Ok((fetched.sync_token, changes))
    }

    /// Sync the followed artists from the `artist` set into the store. The caller saves
    /// `artists.db` and then finishes the set with the returned [ArtistsSync]. `force_full`
    /// downloads the set again.
    fn fetch_artists_any(&self, cache_valid: bool, force_full: bool) -> ArtistsSync {
        let api = &self.spotify.api;
        if !cache_valid {
            self.clear_sync_token(SET_ARTIST);
        }
        let token = self.sync_token(SET_ARTIST);
        let usable = api.internal_usable(Area::Library);
        let (now, full_due) = if usable {
            self.full_fetch_check(SET_ARTIST, "artists", force_full)
        } else {
            (0, false)
        };
        let mode = plan_sync(usable, full_due, token.is_some(), cache_valid);
        let outcome = if mode == Mode::Web {
            SetOutcome::Web
        } else {
            let result = (|| {
                let fetched = fetch_membership(api, SET_ARTIST, mode, token.as_deref())?;
                let members = fetched.membership.only("artist");
                let followed_now: Vec<Artist> = self
                    .artists
                    .read()
                    .unwrap()
                    .iter()
                    .filter(|a| a.is_followed)
                    .cloned()
                    .collect();
                let followed = rebuild(&followed_now, &members, mode == Mode::Full, |u| {
                    api.library_artists(u)
                })?;
                info!(
                    "artists: {}, {} followed",
                    describe_sync(mode, &[&members]),
                    followed.len()
                );
                merge_followed(&mut self.artists.write().unwrap(), followed);
                Ok::<String, InternalError>(fetched.sync_token)
            })();
            outcome(api, SET_ARTIST, result)
        };
        let fetched = match outcome {
            SetOutcome::Synced(_) => true,
            SetOutcome::Incomplete => false,
            SetOutcome::Web => self.fetch_artists(),
        };
        ArtistsSync {
            fetched,
            outcome,
            mode,
            cache_valid,
            now,
        }
    }

    /// Sync the saved shows from the `show` set, caching them in `shows.db`. Returns whether
    /// the sync was complete. `force_full` downloads the set again.
    fn fetch_shows_any(&self, cache_valid: bool, force_full: bool) -> bool {
        let api = &self.spotify.api;
        if !cache_valid {
            self.clear_sync_token(SET_SHOW);
        }
        let token = self.sync_token(SET_SHOW);
        let usable = api.internal_usable(Area::Library);
        let (now, full_due) = if usable {
            self.full_fetch_check(SET_SHOW, "shows", force_full)
        } else {
            (0, false)
        };
        let mode = plan_sync(usable, full_due, token.is_some(), cache_valid);
        let outcome = if mode == Mode::Web {
            SetOutcome::Web
        } else {
            let result = (|| {
                let fetched = fetch_membership(api, SET_SHOW, mode, token.as_deref())?;
                let members = fetched.membership.only("show");
                let shows_now = self.shows.read().unwrap().clone();
                let mut shows = rebuild(&shows_now, &members, mode == Mode::Full, |u| {
                    api.library_shows(u)
                })?;
                sort_newest_first(&mut shows, |s| s.added_at);
                info!(
                    "shows: {}, {} shows",
                    describe_sync(mode, &[&members]),
                    shows.len()
                );
                *self.shows.write().unwrap() = shows;
                Ok::<String, InternalError>(fetched.sync_token)
            })();
            outcome(api, SET_SHOW, result)
        };
        let (complete, saved) = match outcome {
            SetOutcome::Synced(_) => {
                let saved = self.save_cache(
                    &config::cache_path(CACHE_SHOWS),
                    &self.shows.read().unwrap(),
                );
                (saved, saved)
            }
            SetOutcome::Incomplete => (false, false),
            SetOutcome::Web => (self.fetch_shows(force_full), false),
        };
        self.apply_finish(SET_SHOW, finish_set(outcome, saved, mode, cache_valid), now);
        complete
    }

    /// Sync Your Episodes from the `listenlater` set, caching them in `episodes.db`. There is
    /// no Web fetcher: with the area unusable or the session falling back to the Web API the
    /// cache stays as it is. Returns whether the sync was complete. `force_full` downloads the
    /// set again.
    fn fetch_episodes(&self, cache_valid: bool, force_full: bool) -> bool {
        let api = &self.spotify.api;
        if !api.internal_usable(Area::Library) {
            return true;
        }
        if !cache_valid {
            self.clear_sync_token(SET_LISTEN_LATER);
        }
        let token = self.sync_token(SET_LISTEN_LATER);
        let (now, full_due) = self.full_fetch_check(SET_LISTEN_LATER, "episodes", force_full);
        let mode = plan_sync(true, full_due, token.is_some(), cache_valid);
        let result = (|| {
            let fetched = fetch_membership(api, SET_LISTEN_LATER, mode, token.as_deref())?;
            let members = fetched.membership.only("episode");
            let episodes_now = self.episodes.read().unwrap().clone();
            let mut episodes = rebuild(&episodes_now, &members, mode == Mode::Full, |u| {
                api.library_episodes(u)
            })?;
            sort_newest_first(&mut episodes, |e| e.added_at);
            info!(
                "episodes: {}, {} episodes",
                describe_sync(mode, &[&members]),
                episodes.len()
            );
            *self.episodes.write().unwrap() = episodes;
            Ok::<String, InternalError>(fetched.sync_token)
        })();
        let outcome = outcome(api, SET_LISTEN_LATER, result);
        let (complete, saved) = match outcome {
            SetOutcome::Synced(_) => {
                let saved = self.save_cache(
                    &config::cache_path(CACHE_EPISODES),
                    &self.episodes.read().unwrap(),
                );
                (saved, saved)
            }
            SetOutcome::Incomplete => (false, false),
            SetOutcome::Web => (true, false),
        };
        self.apply_finish(
            SET_LISTEN_LATER,
            finish_set(outcome, saved, mode, cache_valid),
            now,
        );
        complete
    }

    /// Fetch the saved albums from the web API and store them in the local library, unless they
    /// are unchanged (always with `force_full`). On a failed page the stored albums stay as they
    /// were.
    fn fetch_albums(&self, force_full: bool) -> Fetch {
        let (stored_ids, incomplete) = {
            let albums = self.albums.read().unwrap();
            (
                albums
                    .iter()
                    .map(|album| album.id.clone())
                    .collect::<Vec<_>>(),
                // An album saved in ncspot has no `added_at` and no API data (its tracks);
                // download the list once so it gets them.
                any_added_at_missing(albums.iter().map(|album| album.added_at)),
            )
        };
        let (now, full) = self.full_fetch_check(FULL_FETCH_ALBUMS, "albums", force_full);
        let synced = sync_saved(&stored_ids, full || incomplete, |offset| {
            debug!("albums page at offset {offset}");
            let page = self
                .spotify
                .api
                .current_user_saved_albums(offset as u32)
                .ok()?;
            Some(SavedPage {
                total: page.total as usize,
                has_next: page.next.is_some(),
                items: page
                    .items
                    .iter()
                    .map(|saved| SavedItem {
                        id: Some(saved.album.id.id().to_string()),
                        added_at: Some(saved.added_at),
                        value: with_artist(Album::from(saved)),
                    })
                    .collect(),
            })
        });
        let mut albums = match synced {
            Synced::Full(albums) => albums,
            Synced::Unchanged => {
                debug!("albums unchanged");
                return Fetch::Unchanged;
            }
            Synced::Prepended { .. } => unreachable!("sync_saved never prepends"),
            Synced::Failed => {
                error!("Failed to fetch albums.");
                return Fetch::Failed;
            }
        };

        sort_albums(&mut albums);

        *self.albums.write().unwrap() = albums;
        self.record_full_fetch(FULL_FETCH_ALBUMS, now);
        Fetch::Changed
    }

    /// Fetch the liked songs from the web API and save them in the local library. Only new
    /// likes are downloaded when nothing else changed, unless `force_full`. On a failed page the
    /// stored songs stay as they were.
    fn fetch_tracks(&self, force_full: bool) -> Fetch {
        let cached: Vec<Track> = self.tracks.read().unwrap().clone();
        let (now, full) = self.full_fetch_check(FULL_FETCH_TRACKS, "liked songs", force_full);
        let synced = sync_liked(
            &cached,
            full,
            |track: &Track| track.id.clone(),
            |offset| {
                debug!("tracks page at offset {offset}");
                let page = self
                    .spotify
                    .api
                    .current_user_saved_tracks(offset as u32)
                    .ok()?;
                Some(SavedPage {
                    total: page.total as usize,
                    has_next: page.next.is_some(),
                    items: page
                        .items
                        .iter()
                        .map(|saved| SavedItem {
                            id: saved.track.id.as_ref().map(|id| id.id().to_string()),
                            added_at: Some(saved.added_at),
                            value: Track::from(saved),
                        })
                        .collect(),
                })
            },
        );
        match synced {
            Synced::Prepended { items, new_count } => {
                debug!("liked songs: {new_count} new");
                *self.tracks.write().unwrap() = items;
                Fetch::Changed
            }
            Synced::Full(items) => {
                *self.tracks.write().unwrap() = items;
                self.record_full_fetch(FULL_FETCH_TRACKS, now);
                Fetch::Changed
            }
            Synced::Unchanged => Fetch::Unchanged,
            Synced::Failed => {
                error!("Failed to fetch tracks.");
                Fetch::Failed
            }
        }
    }

    fn populate_artists(&self) {
        // Remove old unfollowed artists
        {
            let mut artists = self.artists.write().unwrap();
            *artists = artists.iter().filter(|a| a.is_followed).cloned().collect();
        }

        // Add artists that aren't followed but have saved tracks
        {
            let tracks = self.tracks.read().unwrap();
            let mut track_artists: Vec<(&String, &String)> = tracks
                .iter()
                .flat_map(|t| t.artist_ids.iter().zip(t.artists.iter()))
                .collect();
            track_artists.dedup_by(|a, b| a.0 == b.0);

            for (id, name) in track_artists.iter() {
                self.insert_artist(id.to_string(), name.to_string());
            }
        }

        let mut artists = self.artists.write().unwrap();
        let mut lookup: HashMap<String, Option<usize>> = HashMap::new();

        // Make sure only saved tracks are played when playing artists
        for artist in artists.iter_mut() {
            artist.tracks = Some(Vec::new());
        }

        artists.sort_unstable_by(|a, b| {
            let a_cmp = a.name.strip_prefix("The ").unwrap_or(&a.name);
            let b_cmp = b.name.strip_prefix("The ").unwrap_or(&b.name);

            a_cmp.partial_cmp(b_cmp).unwrap()
        });

        // Add saved tracks to artists
        {
            let tracks = self.tracks.read().unwrap();
            for track in tracks.iter() {
                for artist_id in &track.artist_ids {
                    let index = if let Some(i) = lookup.get(artist_id).cloned() {
                        i
                    } else {
                        let i = artists
                            .iter()
                            .position(|a| &a.id.clone().unwrap_or_default() == artist_id);
                        lookup.insert(artist_id.clone(), i);
                        i
                    };

                    if let Some(i) = index {
                        let artist = artists.get_mut(i).unwrap();
                        if artist.tracks.is_none() {
                            artist.tracks = Some(Vec::new());
                        }

                        if let Some(tracks) = artist.tracks.as_mut() {
                            if tracks.iter().any(|t| t.id == track.id) {
                                continue;
                            }

                            tracks.push(track.clone());
                        }
                    }
                }
            }
        }
    }

    /// If there is a local version of the playlist, update it and rewrite the cache.
    pub fn playlist_update(&self, updated: &Playlist) {
        {
            let mut playlists = self.playlists.write().unwrap();
            if let Some(playlist) = playlists.iter_mut().find(|p| p.id == updated.id) {
                *playlist = updated.clone();
            }
        }

        self.save_cache(
            &config::cache_path(CACHE_PLAYLISTS),
            &self.playlists.read().unwrap(),
        );
    }

    /// Check whether `track` is saved in the user's library.
    pub fn is_saved_track(&self, track: &Playable) -> bool {
        if !*self.is_done.read().unwrap() {
            return false;
        }

        let tracks = self.tracks.read().unwrap();
        tracks.iter().any(|t| t.id == track.id())
    }

    /// Save `tracks` to the user's library.
    pub fn save_tracks(&self, tracks: &[&Track]) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        let ids: Vec<&str> = tracks.iter().filter_map(|t| t.id.as_deref()).collect();
        let uris: Vec<String> = tracks
            .iter()
            .filter(|t| t.id.is_some())
            .map(|t| t.uri.clone())
            .collect();
        let api = &self.spotify.api;
        if !write_through(
            api,
            Area::Library,
            || api.collection_write(SET_COLLECTION, &uris, false),
            || api.current_user_saved_tracks_add(ids).is_ok(),
        ) {
            return;
        }

        {
            let mut store = self.tracks.write().unwrap();
            let mut i = 0;
            for track in tracks {
                if store.iter().any(|t| t.id == track.id) {
                    continue;
                }

                store.insert(i, (*track).clone());
                i += 1;
            }
        }

        self.populate_artists();

        self.save_cache(
            &config::cache_path(CACHE_TRACKS),
            &self.tracks.read().unwrap(),
        );
        self.save_cache(
            &config::cache_path(CACHE_ARTISTS),
            &self.artists.read().unwrap(),
        );
    }

    /// Remove `tracks` from the user's library.
    pub fn unsave_tracks(&self, tracks: &[&Track]) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        let ids: Vec<&str> = tracks.iter().filter_map(|t| t.id.as_deref()).collect();
        let uris: Vec<String> = tracks
            .iter()
            .filter(|t| t.id.is_some())
            .map(|t| t.uri.clone())
            .collect();
        let api = &self.spotify.api;
        if !write_through(
            api,
            Area::Library,
            || api.collection_write(SET_COLLECTION, &uris, true),
            || api.current_user_saved_tracks_delete(ids).is_ok(),
        ) {
            return;
        }

        {
            let mut store = self.tracks.write().unwrap();
            *store = store
                .iter()
                .filter(|t| !tracks.iter().any(|tt| t.id == tt.id))
                .cloned()
                .collect();
        }

        self.populate_artists();

        self.save_cache(
            &config::cache_path(CACHE_TRACKS),
            &self.tracks.read().unwrap(),
        );
        self.save_cache(
            &config::cache_path(CACHE_ARTISTS),
            &self.artists.read().unwrap(),
        );
    }

    /// Check whether `album` is saved to the user's library.
    pub fn is_saved_album(&self, album: &Album) -> bool {
        if !*self.is_done.read().unwrap() {
            return false;
        }

        let albums = self.albums.read().unwrap();
        albums.iter().any(|a| a.id == album.id)
    }

    /// Save `album` to the user's library.
    pub fn save_album(&self, album: &Album) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        if let Some(ref album_id) = album.id {
            let api = &self.spotify.api;
            let uris = vec![format!("spotify:album:{album_id}")];
            if !write_through(
                api,
                Area::Library,
                || api.collection_write(SET_COLLECTION, &uris, false),
                || {
                    api.current_user_saved_albums_add(vec![album_id.as_str()])
                        .is_ok()
                },
            ) {
                return;
            }
        }

        {
            let mut store = self.albums.write().unwrap();
            if !store.iter().any(|a| a.id == album.id) {
                store.insert(0, album.clone());

                // resort list of albums
                sort_albums(&mut store);
            }
        }

        self.save_cache(
            &config::cache_path(CACHE_ALBUMS),
            &self.albums.read().unwrap(),
        );
    }

    /// Remove `album` from the user's library.
    pub fn unsave_album(&self, album: &Album) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        if let Some(ref album_id) = album.id {
            let api = &self.spotify.api;
            let uris = vec![format!("spotify:album:{album_id}")];
            if !write_through(
                api,
                Area::Library,
                || api.collection_write(SET_COLLECTION, &uris, true),
                || {
                    api.current_user_saved_albums_delete(vec![album_id.as_str()])
                        .is_ok()
                },
            ) {
                return;
            }
        }

        {
            let mut store = self.albums.write().unwrap();
            *store = store.iter().filter(|a| a.id != album.id).cloned().collect();
        }

        self.save_cache(
            &config::cache_path(CACHE_ALBUMS),
            &self.albums.read().unwrap(),
        );
    }

    /// Check whether the user follows `artist`.
    pub fn is_followed_artist(&self, artist: &Artist) -> bool {
        if !*self.is_done.read().unwrap() {
            return false;
        }

        let artists = self.artists.read().unwrap();
        artists.iter().any(|a| a.id == artist.id && a.is_followed)
    }

    /// Follow `artist` as the logged in user.
    pub fn follow_artist(&self, artist: &Artist) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        if let Some(ref artist_id) = artist.id {
            let api = &self.spotify.api;
            let uris = vec![format!("spotify:artist:{artist_id}")];
            if !write_through(
                api,
                Area::Library,
                || api.collection_write(SET_ARTIST, &uris, false),
                || api.user_follow_artists(vec![artist_id.as_str()]).is_ok(),
            ) {
                return;
            }
        }

        {
            let mut store = self.artists.write().unwrap();
            if let Some(i) = store.iter().position(|a| a.id == artist.id) {
                store[i].is_followed = true;
            } else {
                let mut artist = artist.clone();
                artist.is_followed = true;
                store.push(artist);
            }
        }

        self.populate_artists();

        self.save_cache(
            &config::cache_path(CACHE_ARTISTS),
            &self.artists.read().unwrap(),
        );
    }

    /// Unfollow `artist` as the logged in user.
    pub fn unfollow_artist(&self, artist: &Artist) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        if let Some(ref artist_id) = artist.id {
            let api = &self.spotify.api;
            let uris = vec![format!("spotify:artist:{artist_id}")];
            if !write_through(
                api,
                Area::Library,
                || api.collection_write(SET_ARTIST, &uris, true),
                || api.user_unfollow_artists(vec![artist_id.as_str()]).is_ok(),
            ) {
                return;
            }
        }

        {
            let mut store = self.artists.write().unwrap();
            if let Some(i) = store.iter().position(|a| a.id == artist.id) {
                store[i].is_followed = false;
            }
        }

        self.populate_artists();

        self.save_cache(
            &config::cache_path(CACHE_ARTISTS),
            &self.artists.read().unwrap(),
        );
    }

    /// Check whether `playlist` is saved in the user's library.
    pub fn is_saved_playlist(&self, playlist: &Playlist) -> bool {
        if !*self.is_done.read().unwrap() {
            return false;
        }

        let playlists = self.playlists.read().unwrap();
        playlists.iter().any(|p| p.id == playlist.id)
    }

    /// Check whether `playlist` is in the library but not created by the library's owner.
    pub fn is_followed_playlist(&self, playlist: &Playlist) -> bool {
        self.user_id
            .as_ref()
            .map(|id| id != &playlist.owner_id)
            .unwrap_or(false)
    }

    /// Add `playlist` to the user's library by following it as the logged in user.
    pub fn follow_playlist(&self, mut playlist: Playlist) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        let follow_playlist_result = self.spotify.api.user_playlist_follow_playlist(&playlist.id);

        if follow_playlist_result.is_err() {
            return;
        }

        playlist.load_tracks(&self.spotify);

        let already_present = {
            let mut store = self.playlists.write().unwrap();
            if !store.iter().any(|p| p.id == playlist.id) {
                store.insert(0, playlist.clone());
                false
            } else {
                true
            }
        };

        if !already_present {
            self.save_cache(
                &config::cache_path(CACHE_PLAYLISTS),
                &self.playlists.read().unwrap(),
            );

            #[cfg(feature = "mpris")]
            self.spotify
                .send_mpris(crate::mpris::MprisCommand::EmitPlaylistChanged(Some(
                    playlist.id.clone(),
                )));
        }
    }

    /// Check whether `show` is already in the user's library.
    pub fn is_saved_show(&self, show: &Show) -> bool {
        if !*self.is_done.read().unwrap() {
            return false;
        }

        let shows = self.shows.read().unwrap();
        shows.iter().any(|s| s.id == show.id)
    }

    /// Whether `episode` is in Your Episodes; None while Your Episodes is off or loading.
    pub fn is_saved_episode(&self, episode: &Episode) -> Option<bool> {
        if !self.episodes_enabled || !*self.is_done.read().unwrap() {
            return None;
        }
        Some(
            self.episodes
                .read()
                .unwrap()
                .iter()
                .any(|e| e.id == episode.id),
        )
    }

    /// Add `episode` to Your Episodes.
    pub fn save_episode(&self, episode: &Episode) {
        if self.is_saved_episode(episode) != Some(false)
            || !episode_write(&self.spotify.api, std::slice::from_ref(&episode.uri), false)
        {
            return;
        }
        let mut episode = episode.clone();
        episode.added_at = Some(chrono::Utc::now());
        self.episodes.write().unwrap().insert(0, episode);
        self.save_cache(
            &config::cache_path(CACHE_EPISODES),
            &self.episodes.read().unwrap(),
        );
    }

    /// Remove `episode` from Your Episodes.
    pub fn unsave_episode(&self, episode: &Episode) {
        if self.is_saved_episode(episode) != Some(true)
            || !episode_write(&self.spotify.api, std::slice::from_ref(&episode.uri), true)
        {
            return;
        }
        self.episodes
            .write()
            .unwrap()
            .retain(|e| e.id != episode.id);
        self.save_cache(
            &config::cache_path(CACHE_EPISODES),
            &self.episodes.read().unwrap(),
        );
    }

    /// Save the `show` to the user's library.
    pub fn save_show(&self, show: &Show) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        let api = &self.spotify.api;
        let uris = vec![show.uri.clone()];
        if write_through(
            api,
            Area::Library,
            || api.collection_write(SET_SHOW, &uris, false),
            || api.save_shows(&[show.id.as_str()]).is_ok(),
        ) {
            {
                let mut store = self.shows.write().unwrap();
                if !store.iter().any(|s| s.id == show.id) {
                    store.insert(0, show.clone());
                }
            }
            self.save_cache(
                &config::cache_path(CACHE_SHOWS),
                &self.shows.read().unwrap(),
            );
        }
    }

    /// Remove the `show` from the user's library.
    pub fn unsave_show(&self, show: &Show) {
        if !*self.is_done.read().unwrap() {
            return;
        }

        let api = &self.spotify.api;
        let uris = vec![show.uri.clone()];
        if write_through(
            api,
            Area::Library,
            || api.collection_write(SET_SHOW, &uris, true),
            || api.unsave_shows(&[show.id.as_str()]).is_ok(),
        ) {
            {
                let mut store = self.shows.write().unwrap();
                *store = store.iter().filter(|s| s.id != show.id).cloned().collect();
            }
            self.save_cache(
                &config::cache_path(CACHE_SHOWS),
                &self.shows.read().unwrap(),
            );
        }
    }

    /// Force redraw the user interface.
    pub fn trigger_redraw(&self) {
        self.ev.trigger();
    }
}

/// Whether the store already holds `remote` at the same snapshot with its tracks loaded, so
/// no track download is needed.
fn is_current(store: &[Playlist], remote: &Playlist) -> bool {
    store
        .iter()
        .any(|p| p.id == remote.id && p.snapshot_id == remote.snapshot_id && p.tracks.is_some())
}

/// Whether `owner_id` is the logged-in user; an unknown user owns nothing.
fn is_owned(owner_id: &str, user_id: Option<&str>) -> bool {
    Some(owner_id) == user_id
}

/// The snapshot a listed playlist is compared and stored with. Owned playlists use the free
/// rootlist revision; for the others the rootlist revision is not the head, so the head read
/// decides, and a failed head read keeps the cached copy current for this sync.
fn listed_snapshot(
    rootlist: &str,
    owned: bool,
    head: Option<&str>,
    cached: Option<&str>,
) -> String {
    if owned {
        return rootlist.to_string();
    }
    head.or(cached).unwrap_or(rootlist).to_string()
}

/// Whether the tracks of `remote` must be downloaded: always with `force_full`, otherwise
/// unless the store already holds it [is_current].
fn needs_download(store: &[Playlist], remote: &Playlist, force_full: bool) -> bool {
    force_full || !is_current(store, remote)
}

/// Merge the `listed` playlists into `store`, in order. An unchanged, loaded playlist only
/// refreshes its cover, name and track count, unless `force_full`; otherwise `download` fetches
/// the tracks. A failed download keeps an existing copy's tracks (old snapshot, so the next sync
/// retries; an unchanged copy still gets the refresh) or stores the listed playlist unloaded.
/// Returns false if any download failed.
fn merge_listed(
    store: &mut Vec<Playlist>,
    listed: &[Playlist],
    force_full: bool,
    mut download: impl FnMut(&Playlist) -> Option<Vec<Playable>>,
) -> bool {
    let refresh = |local: &mut Playlist, remote: &Playlist| {
        local.cover_url = remote.cover_url.clone();
        local.name = remote.name.clone();
        local.num_tracks = remote.num_tracks;
    };
    let mut items_complete = true;
    for remote in listed {
        let pos = store.iter().position(|p| p.id == remote.id);
        let current = pos.filter(|&i| is_current(std::slice::from_ref(&store[i]), remote));
        if let Some(i) = current
            && !force_full
        {
            refresh(&mut store[i], remote);
            continue;
        }
        match download(remote) {
            Some(tracks) => {
                let mut updated = remote.clone();
                updated.tracks = Some(tracks);
                match pos {
                    Some(i) => store[i] = updated,
                    None => store.push(updated),
                }
            }
            None => {
                items_complete = false;
                if let Some(i) = current {
                    refresh(&mut store[i], remote);
                } else if pos.is_none() {
                    let mut unloaded = remote.clone();
                    unloaded.tracks = None;
                    store.push(unloaded);
                }
            }
        }
    }
    items_complete
}

/// The per-playlist status of a rootlist entry the user may not read.
const FORBIDDEN: i32 = 403;

/// A rootlist playlist as a store entry, tracks not loaded.
fn listed_playlist(root: &RootPlaylist) -> Playlist {
    Playlist {
        id: root.id.clone(),
        name: root.name.clone(),
        owner_id: root.owner_id.clone(),
        owner_name: None,
        snapshot_id: root.snapshot_id.clone(),
        num_tracks: root.length,
        tracks: None,
        collaborative: root.collaborative,
        cover_url: root.cover_url.clone(),
    }
}

/// Add `playable` to `known` under its URI, unless it is an "Unavailable" placeholder (so an
/// item that comes back is fetched again) or the URI is already known.
fn remember(known: &mut HashMap<String, Playable>, playable: &Playable) {
    let placeholder = matches!(
        playable,
        Playable::Track(t) if t.title == UNAVAILABLE && t.is_playable != Some(true)
    );
    if !placeholder {
        known
            .entry(playable.uri())
            .or_insert_with(|| playable.clone());
    }
}

/// One playlist download of a sync through `fetch`. Nothing is requested for a 403 entry or
/// once a rate limit has set `stopped`; otherwise `sleep` waits out the `spacer` gap at
/// `now` first, and a `RateLimited` answer sets `stopped`.
fn sync_download(
    spacer: &mut Spacer,
    stopped: &mut bool,
    status: i32,
    now: Instant,
    sleep: impl FnOnce(Duration),
    fetch: impl FnOnce() -> Result<Vec<Playable>, InternalError>,
) -> Option<Result<Vec<Playable>, InternalError>> {
    if status == FORBIDDEN || *stopped {
        return None;
    }
    let wait = spacer.next_wait(now);
    if !wait.is_zero() {
        sleep(wait);
    }
    let result = fetch();
    if matches!(result, Err(InternalError::RateLimited)) {
        *stopped = true;
    }
    Some(result)
}

/// Remove local playlists that existed when the sync started (`known_at_start`) but are missing
/// from `remote_ids`, and sort the rest in remote order. Only done when the remote listing was
/// complete; a partial listing must never delete cached playlists, and a playlist created while
/// the sync was running must survive.
fn reconcile_playlists(
    local: &mut Vec<Playlist>,
    known_at_start: &[String],
    remote_ids: &[String],
    listing_complete: bool,
) {
    if !listing_complete {
        return;
    }
    local.retain(|p| !known_at_start.contains(&p.id) || remote_ids.contains(&p.id));
    local.sort_by_key(|p| remote_ids.iter().position(|x| x == &p.id));
}

/// One part of the library sync, as a library tab updates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncSet {
    /// Liked songs and saved albums (one `collection` set), then the artists of the songs.
    Collection,
    Artists,
    Playlists,
    Shows,
    Episodes,
}

/// What [Library::update_tab] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabUpdate {
    /// The tab's sync runs in the background.
    Started,
    /// Another sync runs; nothing was started.
    Busy,
    /// The tab has nothing to sync.
    NoSet,
}

/// The part of the library sync that updates `tab`; None for Browse, and for Your Episodes
/// while it is off.
fn tab_set(tab: &LibraryTab, episodes_enabled: bool) -> Option<SyncSet> {
    match tab {
        LibraryTab::Tracks | LibraryTab::Albums => Some(SyncSet::Collection),
        LibraryTab::Artists => Some(SyncSet::Artists),
        LibraryTab::Playlists => Some(SyncSet::Playlists),
        LibraryTab::Podcasts => Some(SyncSet::Shows),
        LibraryTab::Episodes => episodes_enabled.then_some(SyncSet::Episodes),
        LibraryTab::Browse => None,
    }
}

/// Clears a library's `syncing` flag when dropped, so the flag is released even if the sync
/// thread panics.
struct SyncGuard(Arc<AtomicBool>);

impl Drop for SyncGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// How long a due resume waits before trying again while another sync holds the slot.
const RESUME_BUSY_RETRY: Duration = Duration::from_secs(10);

/// One try of a due resume at resume count `count`: with the sync slot `slot_taken` the
/// resume runs as attempt `count + 1` (Some); otherwise it stays pending and tries again
/// later, the count unchanged (None).
fn resume_attempt(slot_taken: bool, count: u32) -> Option<u32> {
    slot_taken.then_some(count + 1)
}

/// Whether to schedule an automatic resume after a sync: only if it was incomplete because of
/// a rate limit, the resume cap is not reached, and no resume is already pending.
fn should_schedule_resume(complete: bool, rate_limited: bool, count: u32, pending: bool) -> bool {
    !complete && rate_limited && count < MAX_SYNC_RESUMES && !pending
}

/// Delay before a resumed sync: the longest open rate-limit window, but at least
/// [RESUME_MIN_DELAY], plus `jitter`.
fn resume_delay(longest_remaining: Duration, jitter: Duration) -> Duration {
    longest_remaining.max(RESUME_MIN_DELAY) + jitter
}

/// The followed artists' sync, finished by the coordinator once `artists.db` is saved.
struct ArtistsSync {
    /// The artists were fetched (internally or over the Web API).
    fetched: bool,
    outcome: SetOutcome,
    mode: Mode,
    cache_valid: bool,
    /// When the sync started (Unix seconds), for the full-fetch record.
    now: i64,
}

/// What a Web fetcher of a saved list did to its store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fetch {
    /// A page failed; the store is as it was.
    Failed,
    /// The store is current and was not touched.
    Unchanged,
    /// The store was replaced or extended.
    Changed,
}

/// Which stores of the `collection` set changed during a sync.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Changes {
    tracks: bool,
    albums: bool,
}

/// Whether a store's cache file must be written: when the store changed, or when the file was
/// not valid at load (for example an older version's), so that it gets replaced. A full fetch
/// replaces its store and so counts as a change.
fn cache_write_needed(cache_valid: bool, changed: bool) -> bool {
    changed || !cache_valid
}

/// What a set's internal sync did.
enum SetOutcome {
    /// Synced; the new token, to store once the cache file is written.
    Synced(String),
    /// Rate limited or transient: keep the cache, the set is incomplete.
    Incomplete,
    /// Use the Web API fetcher for this set (the area may have been latched).
    Web,
}

/// What happens to a set's sync token after its sync.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenAction {
    /// The set synced and its cache is on disk: this token describes that cache.
    Store(String),
    /// No token may describe the cache now on disk.
    Clear,
    /// The old token still describes the (valid) cache.
    Keep,
}

/// The token action for a finished set sync, and whether to record its full fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finish {
    token: TokenAction,
    /// An internal Full of the set synced and its cache was saved.
    record_full: bool,
}

/// Decide a set's token after its sync: a token is only kept or stored when it describes the
/// cache file. `saved` is whether the set's cache files were written after a `Synced` sync
/// (ignored otherwise); `cache_valid` is whether the cache was valid at load. A set that did
/// not sync over an invalid cache (the store started empty) must leave no token, or the next
/// sync would apply a delta to that empty cache.
fn finish_set(outcome: SetOutcome, saved: bool, mode: Mode, cache_valid: bool) -> Finish {
    let (token, record_full) = match outcome {
        SetOutcome::Synced(token) if saved => (TokenAction::Store(token), mode == Mode::Full),
        SetOutcome::Synced(_) => (TokenAction::Clear, false),
        SetOutcome::Incomplete | SetOutcome::Web if cache_valid => (TokenAction::Keep, false),
        SetOutcome::Incomplete | SetOutcome::Web => (TokenAction::Clear, false),
    };
    Finish { token, record_full }
}

/// The `library_full_fetch` keys an internal Full of `set` refreshes: its own, and those of
/// the Web fetchers for the same lists, so a later Web fallback takes their incremental route
/// instead of a full re-download.
fn full_fetch_keys(set: &str) -> &'static [&'static str] {
    match set {
        SET_COLLECTION => &[SET_COLLECTION, FULL_FETCH_TRACKS, FULL_FETCH_ALBUMS],
        SET_SHOW => &[SET_SHOW, FULL_FETCH_SHOWS],
        SET_ARTIST => &[SET_ARTIST],
        SET_LISTEN_LATER => &[SET_LISTEN_LATER],
        _ => &[],
    }
}

/// How a set was synced, for the log: from what was actually fetched, not from the planned
/// mode (a refused delta pages the set). Changes are summed over `parts`.
fn describe_sync(mode: Mode, parts: &[&Membership]) -> String {
    let (mut added, mut removed, mut listed) = (0, 0, false);
    for part in parts {
        match part {
            Membership::Changes {
                added: a,
                removed: r,
            } => {
                added += a.len();
                removed += r.len();
            }
            Membership::Listing(_) => listed = true,
        }
    }
    if listed {
        let kind = if mode == Mode::Full { "Full" } else { "Repage" };
        kind.to_string()
    } else {
        format!("Delta, added {added}, removed {removed}")
    }
}

/// Map the result of a set's internal sync to what to do next, per the fallback rules.
fn outcome(api: &WebApi, set: &str, result: Result<String, InternalError>) -> SetOutcome {
    match result {
        Ok(token) => SetOutcome::Synced(token),
        Err(err) => match decide_after(&err) {
            Action::LatchThenWeb => {
                info!("{set}: internal sync failed ({err}), latching to the Web API");
                api.latch_area(Area::Library, &err);
                api.count_fallback(Area::Library);
                SetOutcome::Web
            }
            Action::WebOnce => {
                info!("{set}: internal sync failed ({err}), using the Web API this time");
                api.count_fallback(Area::Library);
                SetOutcome::Web
            }
            Action::Fail => {
                warn!("{set}: {err}");
                SetOutcome::Incomplete
            }
        },
    }
}

/// An episode write. Episodes have no Web API fallback: any failure fails the action with a
/// warning; a protocol change still latches the area.
fn episode_write(api: &WebApi, uris: &[String], removed: bool) -> bool {
    match api.collection_write(SET_LISTEN_LATER, uris, removed) {
        Ok(()) => true,
        Err(err) => {
            if let InternalError::ProtocolChanged(_) = err {
                api.latch_area(Area::Library, &err);
            }
            warn!("Could not update Your Episodes: {err}");
            false
        }
    }
}

/// One write in `area`: the internal API first, then the Web API per the fallback classes.
/// True when one of them wrote; the caller updates the store and `.db` only then.
pub(crate) fn write_through(
    api: &WebApi,
    area: Area,
    internal: impl FnOnce() -> Result<(), InternalError>,
    web: impl FnOnce() -> bool,
) -> bool {
    let skip = !api.internal_usable(area);
    run_with_fallback(
        skip,
        || {
            internal().inspect_err(|err| {
                if decide_after(err) == Action::Fail {
                    warn!("Could not update the {}: {err}", area.name());
                }
            })
        },
        || {
            if !skip {
                api.count_fallback(area);
            }
            web().then_some(())
        },
        |err| api.latch_area(area, err),
    )
    .is_some()
}

#[cfg(test)]
mod test {
    use super::*;

    fn playlist(id: &str) -> Playlist {
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

    fn listed(id: &str, snap: &str) -> Playlist {
        Playlist {
            snapshot_id: snap.into(),
            ..playlist(id)
        }
    }

    #[test]
    fn unchanged_valid_cache_is_not_rewritten() {
        assert!(!cache_write_needed(true, false));
    }

    #[test]
    fn changed_store_is_written() {
        assert!(cache_write_needed(true, true));
        assert!(cache_write_needed(false, true));
    }

    #[test]
    fn invalid_cache_is_replaced_even_if_unchanged() {
        assert!(cache_write_needed(false, false));
    }

    #[test]
    fn owned_playlists_use_the_rootlist_snapshot() {
        assert_eq!(
            listed_snapshot("root", true, Some("head"), Some("old")),
            "root"
        );
    }

    #[test]
    fn other_playlists_use_the_head_then_the_cache() {
        assert_eq!(
            listed_snapshot("root", false, Some("head"), Some("old")),
            "head"
        );
        assert_eq!(listed_snapshot("root", false, None, Some("old")), "old");
        assert_eq!(listed_snapshot("root", false, None, None), "root");
    }

    #[test]
    fn ownership_needs_a_known_matching_user() {
        assert!(is_owned("me", Some("me")));
        assert!(!is_owned("spotify", Some("me")));
        assert!(!is_owned("me", None));
    }

    #[test]
    fn merge_listed_stores_failed_downloads_unloaded() {
        let mut store = vec![Playlist {
            tracks: Some(vec![]),
            ..listed("a", "1")
        }];
        let complete = merge_listed(
            &mut store,
            &[listed("a", "2"), listed("b", "1"), listed("c", "1")],
            false,
            |p| (p.id == "c").then(Vec::new),
        );
        assert!(!complete);
        let a = store.iter().find(|p| p.id == "a").unwrap();
        assert_eq!(
            a.snapshot_id, "1",
            "failed download keeps the cached copy and its snapshot"
        );
        let b = store.iter().find(|p| p.id == "b").unwrap();
        assert!(
            b.tracks.is_none(),
            "listed but not downloaded is still stored"
        );
        assert!(store.iter().find(|p| p.id == "c").unwrap().tracks.is_some());
    }

    #[test]
    fn merge_listed_skips_unchanged_loaded_playlists() {
        let mut store = vec![Playlist {
            tracks: Some(vec![]),
            ..listed("a", "1")
        }];
        let mut calls = 0;
        assert!(merge_listed(&mut store, &[listed("a", "1")], false, |_| {
            calls += 1;
            None
        }));
        assert_eq!(calls, 0);
    }

    #[test]
    fn a_forced_full_downloads_an_unchanged_loaded_playlist() {
        let mut store = vec![Playlist {
            tracks: Some(vec![]),
            ..listed("a", "1")
        }];
        let remote = listed("a", "1");
        assert!(!needs_download(&store, &remote, false));
        assert!(needs_download(&store, &remote, true));
        let mut calls = 0;
        let fresh = crate::playlist_items::placeholder("spotify:track:x");
        assert!(merge_listed(
            &mut store,
            std::slice::from_ref(&remote),
            true,
            |_| {
                calls += 1;
                Some(vec![fresh.clone()])
            }
        ));
        assert_eq!(calls, 1);
        assert_eq!(store[0].tracks.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn a_failed_forced_download_keeps_the_copy_and_refreshes_its_listing() {
        let mut store = vec![Playlist {
            tracks: Some(vec![]),
            ..listed("a", "1")
        }];
        let remote = Playlist {
            name: "renamed".into(),
            num_tracks: 7,
            ..listed("a", "1")
        };
        assert!(!merge_listed(
            &mut store,
            std::slice::from_ref(&remote),
            true,
            |_| None
        ));
        assert_eq!(store[0].tracks.as_ref().map(Vec::len), Some(0));
        assert_eq!(store[0].name, "renamed");
        assert_eq!(store[0].num_tracks, 7);
    }

    #[test]
    fn a_forced_full_is_due_right_after_a_full_fetch() {
        let library = test_library();
        let now = chrono::Utc::now().timestamp();
        library.record_full_fetch(SET_COLLECTION, now);
        assert!(
            !library
                .full_fetch_check(SET_COLLECTION, "collection", false)
                .1
        );
        assert!(
            library
                .full_fetch_check(SET_COLLECTION, "collection", true)
                .1
        );
    }

    #[test]
    fn a_forced_full_plans_a_full_sync_over_a_token_and_a_valid_cache() {
        let now = 1_000_000;
        let due = full_fetch_needed(true, Some(now), now);
        assert_eq!(plan_sync(true, due, true, true), Mode::Full);
        let due = full_fetch_needed(false, Some(now), now);
        assert_eq!(plan_sync(true, due, true, true), Mode::Delta);
    }

    fn ids(list: &[Playlist]) -> Vec<&str> {
        list.iter().map(|p| p.id.as_str()).collect()
    }

    fn ids_owned(list: &[Playlist]) -> Vec<String> {
        list.iter().map(|p| p.id.clone()).collect()
    }

    #[test]
    fn incomplete_listing_keeps_all_cached_playlists() {
        let mut local = vec![playlist("a"), playlist("b"), playlist("c")];
        let known = ids_owned(&local);
        reconcile_playlists(&mut local, &known, &["a".to_string()], false);
        assert_eq!(ids(&local), ["a", "b", "c"]);
    }

    #[test]
    fn complete_listing_prunes_and_sorts() {
        let mut local = vec![playlist("a"), playlist("b"), playlist("c")];
        let known = ids_owned(&local);
        reconcile_playlists(
            &mut local,
            &known,
            &["c".to_string(), "a".to_string()],
            true,
        );
        assert_eq!(ids(&local), ["c", "a"]);
    }

    #[test]
    fn playlist_created_during_sync_is_not_pruned() {
        let mut local = vec![playlist("a"), playlist("b"), playlist("new")];
        let known = vec!["a".to_string(), "b".to_string()];
        reconcile_playlists(&mut local, &known, &["a".to_string()], true);
        assert_eq!(ids(&local), ["new", "a"]);
    }

    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::config::Config;
    use crate::events::EventManager;
    use crate::rate_limit::{MAX_SYNC_RESUMES, RESUME_MIN_DELAY};
    use crate::spotify::Spotify;

    #[test]
    fn resume_only_for_an_incomplete_rate_limited_sync() {
        assert!(should_schedule_resume(false, true, 0, false));
        assert!(!should_schedule_resume(true, true, 0, false));
        assert!(!should_schedule_resume(false, false, 0, false));
    }

    #[test]
    fn resume_only_one_pending_at_a_time() {
        assert!(!should_schedule_resume(false, true, 0, true));
    }

    #[test]
    fn a_busy_slot_retries_the_resume_without_counting_it() {
        assert_eq!(resume_attempt(false, 1), None);
        assert_eq!(resume_attempt(true, 1), Some(2));
    }

    #[test]
    fn resume_stops_at_the_cap() {
        assert!(should_schedule_resume(
            false,
            true,
            MAX_SYNC_RESUMES - 1,
            false
        ));
        assert!(!should_schedule_resume(
            false,
            true,
            MAX_SYNC_RESUMES,
            false
        ));
    }

    #[test]
    fn resume_delay_never_below_the_minimum() {
        let jitter = Duration::from_millis(250);
        assert_eq!(
            resume_delay(Duration::ZERO, jitter),
            RESUME_MIN_DELAY + jitter
        );
        assert_eq!(
            resume_delay(Duration::from_secs(5), jitter),
            RESUME_MIN_DELAY + jitter
        );
        assert_eq!(
            resume_delay(Duration::from_secs(90), jitter),
            Duration::from_secs(90) + jitter
        );
    }

    #[test]
    fn sync_guard_releases_the_flag_on_panic() {
        let flag = Arc::new(AtomicBool::new(true));
        let in_thread = flag.clone();
        let result = std::thread::spawn(move || {
            let _guard = SyncGuard(in_thread);
            panic!("fetch thread failed");
        })
        .join();
        assert!(result.is_err());
        assert!(!flag.load(Ordering::SeqCst));
    }

    fn test_library() -> Arc<Library> {
        let cfg = Config::new_for_test();
        cfg.with_state_mut(|s| s.cache_version = CACHE_VERSION);
        let ev = EventManager::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        Library::new_for_test(ev, spotify, cfg)
    }

    #[test]
    fn episode_writes_have_no_web_fallback() {
        let api = WebApi::new(&crate::config::Config::new_for_test());
        // No session: NoSession, which for other sets would use the Web API.
        assert!(!episode_write(
            &api,
            &["spotify:episode:e".to_string()],
            false
        ));
        assert_eq!(
            api.area_counts(Area::Library).fallback,
            0,
            "no fallback counted"
        );
        assert!(api.internal_usable(Area::Library));
    }

    #[test]
    fn episode_save_state_is_hidden_while_episodes_are_off() {
        let mut library = test_library();
        Arc::get_mut(&mut library).unwrap().episodes_enabled = false;
        let episode = Episode {
            id: "e".into(),
            uri: "spotify:episode:e".into(),
            duration: 0,
            name: String::new(),
            description: String::new(),
            release_date: String::new(),
            cover_url: None,
            added_at: None,
            list_index: 0,
        };
        assert_eq!(library.is_saved_episode(&episode), None);
    }

    #[test]
    fn episode_store_is_unchanged_when_the_write_fails() {
        let mut library = test_library();
        Arc::get_mut(&mut library).unwrap().episodes_enabled = true;
        *library.is_done.write().unwrap() = true;
        let episode = |id: &str| Episode {
            id: id.into(),
            uri: format!("spotify:episode:{id}"),
            duration: 0,
            name: String::new(),
            description: String::new(),
            release_date: String::new(),
            cover_url: None,
            added_at: None,
            list_index: 0,
        };
        library.save_episode(&episode("new"));
        assert!(library.episodes.read().unwrap().is_empty());
        library.episodes.write().unwrap().push(episode("kept"));
        library.unsave_episode(&episode("kept"));
        let store = library.episodes.read().unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].id, "kept");
    }

    #[test]
    fn sync_tokens_are_stored_and_cleared_per_set() {
        let library = test_library();
        assert_eq!(library.sync_token("artist"), None);
        library.set_sync_token("artist", "t1".into());
        library.set_sync_token("show", "t2".into());
        assert_eq!(library.sync_token("artist").as_deref(), Some("t1"));
        library.clear_sync_token("artist");
        assert_eq!(library.sync_token("artist"), None);
        assert_eq!(library.sync_token("show").as_deref(), Some("t2"));
    }

    #[test]
    fn cache_results_report_success() {
        let library = test_library();
        let dir = std::env::temp_dir().join(format!("ncspot-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.db");
        assert!(library.save_cache(&path, &[1u32, 2]));
        let mut store: Vec<u32> = Vec::new();
        assert!(library.load_cache(&path, &mut store));
        assert_eq!(store, vec![1, 2]);
        assert!(!library.load_cache(&dir.join("missing.db"), &mut store));
        assert!(!library.save_cache(&dir.join("no-such-dir").join("x.db"), &[1u32]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_through_follows_the_fallback_classes() {
        use std::cell::Cell;
        let api = WebApi::new(&crate::config::Config::new_for_test());
        let web = Cell::new(0);
        let call = |r: Result<(), InternalError>, web_ok: bool| {
            write_through(
                &api,
                Area::Library,
                || r,
                || {
                    web.set(web.get() + 1);
                    web_ok
                },
            )
        };
        assert!(call(Ok(()), false));
        assert_eq!(web.get(), 0);
        assert!(
            !call(Err(InternalError::RateLimited), true),
            "rate limit fails the action"
        );
        assert!(!call(Err(InternalError::Transient("x".into())), true));
        assert_eq!(
            web.get(),
            0,
            "no Web API call for rate limits or transient errors"
        );
        assert!(call(Err(InternalError::NoSession), true));
        assert!(
            !call(Err(InternalError::Denied), false),
            "a failed Web call fails the action"
        );
        assert_eq!(web.get(), 2);
        assert!(api.internal_usable(Area::Library));
        assert!(call(
            Err(InternalError::ProtocolChanged("400".into())),
            true
        ));
        assert!(
            !api.internal_usable(Area::Library),
            "a protocol change latches the area"
        );
        assert_eq!(web.get(), 3);
        // Latched: straight to the Web API, the internal call never runs.
        assert!(write_through(
            &api,
            Area::Library,
            || panic!("internal ran while latched"),
            || true
        ));
        assert_eq!(api.area_counts(Area::Library).fallback, 3);
    }

    #[test]
    fn write_through_follows_the_fallback_classes_for_playlists() {
        use std::cell::Cell;
        let api = WebApi::new(&crate::config::Config::new_for_test());
        let web = Cell::new(0);
        let call = |r: Result<(), InternalError>, web_ok: bool| {
            write_through(
                &api,
                Area::Playlists,
                || r,
                || {
                    web.set(web.get() + 1);
                    web_ok
                },
            )
        };
        assert!(call(Ok(()), false));
        assert_eq!(web.get(), 0);
        assert!(
            !call(Err(InternalError::RateLimited), true),
            "rate limit fails the action"
        );
        assert!(!call(Err(InternalError::Transient("x".into())), true));
        assert_eq!(
            web.get(),
            0,
            "no Web API call for rate limits or transient errors"
        );
        assert!(call(Err(InternalError::NoSession), true));
        assert!(
            !call(Err(InternalError::Denied), false),
            "a failed Web call fails the action"
        );
        assert_eq!(web.get(), 2);
        assert!(api.internal_usable(Area::Playlists));
        assert!(call(
            Err(InternalError::ProtocolChanged("400".into())),
            true
        ));
        assert!(
            !api.internal_usable(Area::Playlists),
            "a protocol change latches the area"
        );
        assert_eq!(web.get(), 3);
        // Latched: straight to the Web API, the internal call never runs.
        assert!(write_through(
            &api,
            Area::Playlists,
            || panic!("internal ran while latched"),
            || true
        ));
        assert_eq!(api.area_counts(Area::Playlists).fallback, 3);
    }

    #[test]
    fn append_with_the_web_api_unreachable_leaves_the_tracks_unchanged() {
        let library = test_library();
        let mut list = playlist("p");
        list.tracks = Some(Vec::new());
        let spotify = library.spotify.clone();
        let new = [Playable::Track(Track {
            id: Some("t".into()),
            uri: "spotify:track:t".into(),
            title: "t".into(),
            track_number: 1,
            disc_number: 1,
            duration: 1000,
            artists: Vec::new(),
            artist_ids: Vec::new(),
            album: None,
            album_id: None,
            album_artists: Vec::new(),
            cover_url: None,
            url: String::new(),
            added_at: None,
            list_index: 0,
            is_local: false,
            is_playable: Some(true),
        })];
        list.append_tracks(&new, &spotify, &library);
        assert_eq!(list.tracks.as_ref().map(Vec::len), Some(0));
    }

    #[test]
    fn outcome_follows_the_fallback_classes() {
        let api = WebApi::new(&crate::config::Config::new_for_test());
        assert!(
            matches!(outcome(&api, "artist", Ok("t".into())), SetOutcome::Synced(t) if t == "t")
        );
        assert!(matches!(
            outcome(&api, "artist", Err(InternalError::RateLimited)),
            SetOutcome::Incomplete
        ));
        assert!(matches!(
            outcome(&api, "artist", Err(InternalError::Transient("x".into()))),
            SetOutcome::Incomplete
        ));
        assert!(matches!(
            outcome(&api, "artist", Err(InternalError::NoSession)),
            SetOutcome::Web
        ));
        assert!(api.internal_usable(Area::Library));
        assert!(matches!(
            outcome(
                &api,
                "artist",
                Err(InternalError::ProtocolChanged("400".into()))
            ),
            SetOutcome::Web
        ));
        assert!(
            !api.internal_usable(Area::Library),
            "a protocol change latches the area"
        );
        assert_eq!(api.area_counts(Area::Library).fallback, 2);
    }

    #[test]
    fn describe_sync_goes_by_the_membership_not_the_mode() {
        use crate::internal_api::collection::Item;
        let item = |uri: &str| Item {
            uri: uri.to_string(),
            added_at: 0,
        };
        let changes = Membership::Changes {
            added: vec![item("spotify:artist:a"), item("spotify:artist:b")],
            removed: vec!["spotify:artist:c".to_string()],
        };
        assert_eq!(
            describe_sync(Mode::Delta, &[&changes, &changes]),
            "Delta, added 4, removed 2"
        );
        let listing = Membership::Listing(vec![item("spotify:artist:a")]);
        assert_eq!(describe_sync(Mode::Delta, &[&listing]), "Repage");
        assert_eq!(describe_sync(Mode::Repage, &[&listing]), "Repage");
        assert_eq!(describe_sync(Mode::Full, &[&listing]), "Full");
    }

    #[test]
    fn a_second_sync_is_skipped_while_one_runs() {
        let library = test_library();
        library.syncing.store(true, Ordering::SeqCst);
        *library.is_done.write().unwrap() = true;

        library.update_library(true);

        assert!(*library.is_done.read().unwrap());
        assert!(library.syncing.load(Ordering::SeqCst));
    }

    #[test]
    fn each_tab_updates_its_own_set() {
        let all = [
            (LibraryTab::Tracks, Some(SyncSet::Collection)),
            (LibraryTab::Albums, Some(SyncSet::Collection)),
            (LibraryTab::Artists, Some(SyncSet::Artists)),
            (LibraryTab::Playlists, Some(SyncSet::Playlists)),
            (LibraryTab::Podcasts, Some(SyncSet::Shows)),
            (LibraryTab::Episodes, Some(SyncSet::Episodes)),
            (LibraryTab::Browse, None),
        ];
        for (tab, set) in all {
            assert_eq!(tab_set(&tab, true), set, "{tab:?}");
        }
        assert_eq!(tab_set(&LibraryTab::Episodes, false), None);
        assert_eq!(
            tab_set(&LibraryTab::Tracks, false),
            Some(SyncSet::Collection)
        );
    }

    #[test]
    fn a_tab_update_is_skipped_while_a_sync_runs() {
        let library = test_library();
        library.syncing.store(true, Ordering::SeqCst);
        *library.is_done.write().unwrap() = true;

        assert_eq!(library.update_tab(&LibraryTab::Tracks), TabUpdate::Busy);

        assert!(*library.is_done.read().unwrap());
        assert!(library.syncing.load(Ordering::SeqCst));
    }

    #[test]
    fn a_tab_without_a_set_starts_nothing() {
        let library = test_library();
        *library.is_done.write().unwrap() = true;

        assert_eq!(library.update_tab(&LibraryTab::Browse), TabUpdate::NoSet);
        assert_eq!(library.update_tab(&LibraryTab::Episodes), TabUpdate::NoSet);

        assert!(*library.is_done.read().unwrap());
        assert!(!library.syncing.load(Ordering::SeqCst));
    }

    fn synced(t: &str) -> SetOutcome {
        SetOutcome::Synced(t.to_string())
    }

    #[test]
    fn a_synced_and_saved_set_stores_its_token() {
        for mode in [Mode::Delta, Mode::Repage, Mode::Full] {
            for valid in [true, false] {
                assert_eq!(
                    finish_set(synced("t"), true, mode, valid),
                    Finish {
                        token: TokenAction::Store("t".into()),
                        record_full: mode == Mode::Full,
                    }
                );
            }
        }
    }

    #[test]
    fn a_synced_set_whose_save_failed_clears_its_token() {
        for mode in [Mode::Delta, Mode::Repage, Mode::Full] {
            assert_eq!(
                finish_set(synced("t"), false, mode, true),
                Finish {
                    token: TokenAction::Clear,
                    record_full: false,
                }
            );
        }
    }

    #[test]
    fn an_unsynced_set_keeps_its_token_only_over_a_valid_cache() {
        for mode in [Mode::Web, Mode::Delta, Mode::Repage, Mode::Full] {
            for saved in [true, false] {
                let outcomes: [fn() -> SetOutcome; 2] =
                    [|| SetOutcome::Incomplete, || SetOutcome::Web];
                for outcome in outcomes {
                    assert_eq!(
                        finish_set(outcome(), saved, mode, false),
                        Finish {
                            token: TokenAction::Clear,
                            record_full: false,
                        },
                        "an invalid cache written without a new token must not keep the old one"
                    );
                    assert_eq!(
                        finish_set(outcome(), saved, mode, true),
                        Finish {
                            token: TokenAction::Keep,
                            record_full: false,
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn an_internal_full_also_records_the_web_keys() {
        assert_eq!(
            full_fetch_keys(SET_COLLECTION),
            &[SET_COLLECTION, FULL_FETCH_TRACKS, FULL_FETCH_ALBUMS]
        );
        assert_eq!(full_fetch_keys(SET_SHOW), &[SET_SHOW, FULL_FETCH_SHOWS]);
        assert_eq!(full_fetch_keys(SET_ARTIST), &[SET_ARTIST]);
        assert_eq!(full_fetch_keys(SET_LISTEN_LATER), &[SET_LISTEN_LATER]);
    }

    #[test]
    fn error_classes_map_to_token_actions() {
        let api = WebApi::new(&crate::config::Config::new_for_test());
        let cases = [
            (Err(InternalError::RateLimited), TokenAction::Keep),
            (Err(InternalError::Transient("x".into())), TokenAction::Keep),
            (Err(InternalError::NoSession), TokenAction::Keep),
            (Err(InternalError::NotFound), TokenAction::Keep),
            (Err(InternalError::Denied), TokenAction::Keep),
            (Ok("t".to_string()), TokenAction::Store("t".into())),
        ];
        for (result, valid_action) in cases {
            let unsynced = result.is_err();
            let o = outcome(&api, "artist", result.clone());
            assert_eq!(finish_set(o, true, Mode::Repage, true).token, valid_action);
            let o = outcome(&api, "artist", result);
            let invalid_action = if unsynced {
                TokenAction::Clear
            } else {
                TokenAction::Store("t".into())
            };
            assert_eq!(
                finish_set(o, true, Mode::Repage, false).token,
                invalid_action
            );
        }
    }

    #[test]
    fn a_full_fetch_is_recorded_under_every_key_of_the_set() {
        let library = test_library();
        library.apply_finish(
            SET_COLLECTION,
            Finish {
                token: TokenAction::Store("t".into()),
                record_full: true,
            },
            42,
        );
        assert_eq!(library.sync_token(SET_COLLECTION).as_deref(), Some("t"));
        let state = library.cfg.state();
        for key in [SET_COLLECTION, FULL_FETCH_TRACKS, FULL_FETCH_ALBUMS] {
            assert_eq!(state.library_full_fetch.get(key), Some(&42), "{key}");
        }
        assert_eq!(state.library_full_fetch.get(FULL_FETCH_SHOWS), None);
        drop(state);
        library.apply_finish(
            SET_COLLECTION,
            Finish {
                token: TokenAction::Keep,
                record_full: false,
            },
            43,
        );
        assert_eq!(library.sync_token(SET_COLLECTION).as_deref(), Some("t"));
        library.apply_finish(
            SET_COLLECTION,
            Finish {
                token: TokenAction::Clear,
                record_full: false,
            },
            44,
        );
        assert_eq!(library.sync_token(SET_COLLECTION), None);
        assert_eq!(
            library.cfg.state().library_full_fetch.get(SET_COLLECTION),
            Some(&42)
        );
    }

    #[test]
    fn internal_playlist_sync_without_a_session_uses_the_web_path() {
        let library = test_library();
        *library.playlist_layout.write().unwrap() = vec![LayoutEntry::Playlist("p".into())];
        assert!(library.spotify.api.internal_usable(Area::Playlists));
        assert!(library.fetch_playlists_internal(false).is_none());
        assert!(library.playlist_layout.read().unwrap().is_empty());
        assert_eq!(library.spotify.api.area_counts(Area::Playlists).fallback, 1);
        assert!(library.spotify.api.internal_usable(Area::Playlists));
    }

    #[test]
    fn sync_downloads_are_spaced_and_skip_forbidden_playlists() {
        let t0 = Instant::now();
        let mut spacer = Spacer::new(PLAYLIST_FETCH_SPACING);
        let mut stopped = false;
        let mut slept = Vec::new();
        let mut calls = 0;
        let mut download = |status: i32, now: Instant, slept: &mut Vec<Duration>| {
            sync_download(
                &mut spacer,
                &mut stopped,
                status,
                now,
                |d| slept.push(d),
                || {
                    calls += 1;
                    Ok(Vec::new())
                },
            )
        };
        assert!(
            download(403, t0, &mut slept).is_none(),
            "403 is not requested"
        );
        assert!(matches!(download(200, t0, &mut slept), Some(Ok(_))));
        assert!(matches!(download(200, t0, &mut slept), Some(Ok(_))));
        assert_eq!(calls, 2);
        assert_eq!(slept, vec![PLAYLIST_FETCH_SPACING]);
    }

    #[test]
    fn a_rate_limit_stops_the_remaining_downloads() {
        let t0 = Instant::now();
        let mut spacer = Spacer::new(PLAYLIST_FETCH_SPACING);
        let mut stopped = false;
        let mut calls = 0;
        let mut download = |result: Result<Vec<Playable>, InternalError>| {
            sync_download(
                &mut spacer,
                &mut stopped,
                200,
                t0,
                |_| {},
                || {
                    calls += 1;
                    result
                },
            )
        };
        assert!(matches!(
            download(Err(InternalError::Transient("x".into()))),
            Some(Err(InternalError::Transient(_)))
        ));
        assert!(matches!(
            download(Err(InternalError::RateLimited)),
            Some(Err(InternalError::RateLimited))
        ));
        assert!(
            download(Ok(Vec::new())).is_none(),
            "no request after a rate limit"
        );
        assert_eq!(calls, 2);
    }

    #[test]
    fn known_playables_skip_placeholders() {
        let mut known = HashMap::new();
        let Playable::Track(mut real) = crate::playlist_items::placeholder("spotify:track:a")
        else {
            unreachable!("a placeholder is a track");
        };
        real.is_playable = Some(true);
        remember(&mut known, &Playable::Track(real));
        remember(
            &mut known,
            &crate::playlist_items::placeholder("spotify:track:b"),
        );
        assert!(
            known.contains_key("spotify:track:a"),
            "a real track with that title"
        );
        assert!(!known.contains_key("spotify:track:b"));
    }

    #[test]
    fn rootlist_playlists_are_listed_unloaded() {
        let root = RootPlaylist {
            id: "p".into(),
            name: "n".into(),
            snapshot_id: "s".into(),
            length: 3,
            owner_id: "o".into(),
            collaborative: true,
            cover_url: Some("c".into()),
            status: 200,
        };
        let p = listed_playlist(&root);
        assert_eq!(
            (p.id.as_str(), p.name.as_str(), p.snapshot_id.as_str()),
            ("p", "n", "s")
        );
        assert_eq!((p.num_tracks, p.owner_id.as_str()), (3, "o"));
        assert!(p.collaborative && p.owner_name.is_none() && p.tracks.is_none());
        assert_eq!(p.cover_url.as_deref(), Some("c"));
    }
}
