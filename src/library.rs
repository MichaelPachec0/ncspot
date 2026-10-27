use std::collections::HashMap;
use std::fs::File;
use std::iter::Iterator;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use log::{debug, error, info, warn};
use rspotify::model::Id;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::config::{self, CACHE_VERSION};
use crate::events::EventManager;
use crate::internal_api::Area;
use crate::internal_api::collection::{SET_ARTIST, SET_COLLECTION, SET_LISTEN_LATER, SET_SHOW};
use crate::internal_api::error::InternalError;
use crate::internal_api::fallback::{Action, decide_after, run_with_fallback};
use crate::library_collection::{
    Membership, Mode, fetch_membership, merge_followed, plan_sync, rebuild, sort_albums,
    sort_newest_first, with_artist,
};
use crate::library_sync::{SavedItem, SavedPage, Synced, full_fetch_due, sync_liked, sync_saved};
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::episode::Episode;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::track::Track;
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

/// Outcome of [Library::fetch_playlists].
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

        library.update_library();
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
        self.spotify.api.overwrite_playlist(id, tracks);

        if self.fetch_playlists().listing_complete {
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

    /// Update the local library and its cache on disk. Does nothing if a sync is already
    /// running. If the sync is left incomplete by a rate limit, a resume is scheduled for when
    /// the limit has passed.
    pub fn update_library(&self) {
        if self
            .syncing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            info!("library sync already running, skipped");
            return;
        }
        *self.is_done.write().unwrap() = false;

        let library = self.clone();
        thread::spawn(move || {
            let _guard = SyncGuard(library.syncing.clone());
            let sync_start = Stamp::now();

            let t_collection = {
                let library = library.clone();
                thread::spawn(move || {
                    let valid = library.load_cache(
                        &config::cache_path(CACHE_TRACKS),
                        library.tracks.write().unwrap().as_mut(),
                    ) & library.load_cache(
                        &config::cache_path(CACHE_ALBUMS),
                        library.albums.write().unwrap().as_mut(),
                    );
                    library.fetch_collection(valid)
                })
            };

            let t_artists = {
                let library = library.clone();
                thread::spawn(move || {
                    let valid = library.load_cache(
                        &config::cache_path(CACHE_ARTISTS),
                        library.artists.write().unwrap().as_mut(),
                    );
                    library.fetch_artists_any(valid)
                })
            };

            let t_playlists = {
                let library = library.clone();
                thread::spawn(move || {
                    library.load_cache(
                        &config::cache_path(CACHE_PLAYLISTS),
                        library.playlists.write().unwrap().as_mut(),
                    );
                    let sync = library.fetch_playlists();
                    if sync.listing_complete {
                        library.save_cache(
                            &config::cache_path(CACHE_PLAYLISTS),
                            &library.playlists.read().unwrap(),
                        );
                    }
                    sync.listing_complete && sync.items_complete
                })
            };

            let t_shows = {
                let library = library.clone();
                thread::spawn(move || {
                    // With the area off nothing is loaded: the Web fetcher runs as before.
                    let valid = library.spotify.api.internal_enabled(Area::Library)
                        && library.load_cache(
                            &config::cache_path(CACHE_SHOWS),
                            library.shows.write().unwrap().as_mut(),
                        );
                    library.fetch_shows_any(valid)
                })
            };

            let t_episodes = {
                let library = library.clone();
                thread::spawn(move || {
                    if !library.episodes_enabled {
                        return true;
                    }
                    let valid = library.load_cache(
                        &config::cache_path(CACHE_EPISODES),
                        library.episodes.write().unwrap().as_mut(),
                    );
                    library.fetch_episodes(valid)
                })
            };

            let collection_complete = t_collection.join().unwrap();
            let artists = t_artists.join().unwrap();

            library.populate_artists();
            let artists_saved = library.save_cache(
                &config::cache_path(CACHE_ARTISTS),
                &library.artists.read().unwrap(),
            );
            // The token is stored only once the cache it describes is on disk.
            let synced = matches!(artists.outcome, SetOutcome::Synced(_));
            let artists_complete = artists.fetched && (artists_saved || !synced);
            library.apply_finish(
                SET_ARTIST,
                finish_set(
                    artists.outcome,
                    artists_saved,
                    artists.mode,
                    artists.cache_valid,
                ),
                artists.now,
            );

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
            library.resume_pending.store(false, Ordering::SeqCst);
            library.resume_count.fetch_add(1, Ordering::SeqCst);
            library.update_library();
        });
    }

    /// Returns the current Unix time and whether the list stored under `key` needs a full
    /// fetch now, logging when it does. The state lock is released before returning.
    fn full_fetch_check(&self, key: &str, what: &str) -> (i64, bool) {
        let now = chrono::Utc::now().timestamp();
        let last = self.cfg.state().library_full_fetch.get(key).copied();
        let force_full = full_fetch_due(last, now);
        if force_full {
            debug!("{what}: full refresh (cache older than 24h)");
        }
        (now, force_full)
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
    /// are unchanged. Returns false, leaving the stored shows unchanged, if a page failed.
    fn fetch_shows(&self) -> bool {
        debug!("loading shows");
        let stored_ids: Vec<Option<String>> = self
            .shows
            .read()
            .unwrap()
            .iter()
            .map(|show| Some(show.id.clone()))
            .collect();
        let (now, force_full) = self.full_fetch_check(FULL_FETCH_SHOWS, "shows");
        let synced = sync_saved(&stored_ids, force_full, |offset| {
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
    /// the cached copies were kept and the sync is incomplete).
    fn fetch_playlists(&self) -> PlaylistSync {
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
            let mut downloaded = if is_current(&self.playlists.read().unwrap(), remote) {
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
    /// Saves both caches; the token is stored only if both writes worked.
    fn fetch_collection(&self, cache_valid: bool) -> bool {
        let api = &self.spotify.api;
        // A token describes its cache only: without a valid cache it must not survive a sync
        // that ends without a new one.
        if !cache_valid {
            self.clear_sync_token(SET_COLLECTION);
        }
        let token = self.sync_token(SET_COLLECTION);
        let usable = api.internal_usable(Area::Library);
        let (now, full_due) = if usable {
            self.full_fetch_check(SET_COLLECTION, "collection")
        } else {
            (0, false)
        };
        let mode = plan_sync(usable, full_due, token.is_some(), cache_valid);
        // Mode::Web never goes through `outcome`: an area that is simply off must not latch.
        let outcome = if mode == Mode::Web {
            SetOutcome::Web
        } else {
            outcome(
                api,
                SET_COLLECTION,
                self.collection_internal(mode, token.as_deref()),
            )
        };
        let fetched = match outcome {
            SetOutcome::Web => {
                // Two Web fetchers, both always run, in parallel.
                let (tracks, albums) = thread::scope(|scope| {
                    let tracks = scope.spawn(|| self.fetch_tracks());
                    let albums = scope.spawn(|| self.fetch_albums());
                    (tracks.join().unwrap(), albums.join().unwrap())
                });
                tracks && albums
            }
            SetOutcome::Incomplete => false,
            SetOutcome::Synced(_) => true,
        };
        let saved = self.save_cache(
            &config::cache_path(CACHE_TRACKS),
            &self.tracks.read().unwrap(),
        ) & self.save_cache(
            &config::cache_path(CACHE_ALBUMS),
            &self.albums.read().unwrap(),
        );
        let synced = matches!(outcome, SetOutcome::Synced(_));
        self.apply_finish(
            SET_COLLECTION,
            finish_set(outcome, saved, mode, cache_valid),
            now,
        );
        fetched && (saved || !synced)
    }

    /// Sync the `collection` set over the internal API into the in-memory tracks and albums.
    /// Returns the new sync token; the caller stores it once the caches are written.
    fn collection_internal(
        &self,
        mode: Mode,
        token: Option<&str>,
    ) -> Result<String, InternalError> {
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
        Ok(fetched.sync_token)
    }

    /// Sync the followed artists from the `artist` set into the store. The caller saves
    /// `artists.db` and then finishes the set with the returned [ArtistsSync].
    fn fetch_artists_any(&self, cache_valid: bool) -> ArtistsSync {
        let api = &self.spotify.api;
        if !cache_valid {
            self.clear_sync_token(SET_ARTIST);
        }
        let token = self.sync_token(SET_ARTIST);
        let usable = api.internal_usable(Area::Library);
        let (now, full_due) = if usable {
            self.full_fetch_check(SET_ARTIST, "artists")
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
    /// the sync was complete.
    fn fetch_shows_any(&self, cache_valid: bool) -> bool {
        let api = &self.spotify.api;
        if !cache_valid {
            self.clear_sync_token(SET_SHOW);
        }
        let token = self.sync_token(SET_SHOW);
        let usable = api.internal_usable(Area::Library);
        let (now, full_due) = if usable {
            self.full_fetch_check(SET_SHOW, "shows")
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
            SetOutcome::Web => (self.fetch_shows(), false),
        };
        self.apply_finish(SET_SHOW, finish_set(outcome, saved, mode, cache_valid), now);
        complete
    }

    /// Sync Your Episodes from the `listenlater` set, caching them in `episodes.db`. There is
    /// no Web fetcher: with the area unusable or the session falling back to the Web API the
    /// cache stays as it is. Returns whether the sync was complete.
    fn fetch_episodes(&self, cache_valid: bool) -> bool {
        let api = &self.spotify.api;
        if !api.internal_usable(Area::Library) {
            return true;
        }
        if !cache_valid {
            self.clear_sync_token(SET_LISTEN_LATER);
        }
        let token = self.sync_token(SET_LISTEN_LATER);
        let (now, full_due) = self.full_fetch_check(SET_LISTEN_LATER, "episodes");
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
    /// are unchanged. Returns false, leaving the stored albums unchanged, if a page failed.
    fn fetch_albums(&self) -> bool {
        let stored_ids: Vec<Option<String>> = self
            .albums
            .read()
            .unwrap()
            .iter()
            .map(|album| album.id.clone())
            .collect();
        let (now, force_full) = self.full_fetch_check(FULL_FETCH_ALBUMS, "albums");
        let synced = sync_saved(&stored_ids, force_full, |offset| {
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
                return true;
            }
            Synced::Prepended { .. } => unreachable!("sync_saved never prepends"),
            Synced::Failed => {
                error!("Failed to fetch albums.");
                return false;
            }
        };

        sort_albums(&mut albums);

        *self.albums.write().unwrap() = albums;
        self.record_full_fetch(FULL_FETCH_ALBUMS, now);
        true
    }

    /// Fetch the liked songs from the web API and save them in the local library. Only new
    /// likes are downloaded when nothing else changed. Returns false, leaving the stored songs
    /// unchanged, if a page failed.
    fn fetch_tracks(&self) -> bool {
        let cached: Vec<Track> = self.tracks.read().unwrap().clone();
        let (now, force_full) = self.full_fetch_check(FULL_FETCH_TRACKS, "liked songs");
        let synced = sync_liked(
            &cached,
            force_full,
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
                true
            }
            Synced::Full(items) => {
                *self.tracks.write().unwrap() = items;
                self.record_full_fetch(FULL_FETCH_TRACKS, now);
                true
            }
            Synced::Unchanged => true,
            Synced::Failed => {
                error!("Failed to fetch tracks.");
                false
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

/// Merge the `listed` playlists into `store`, in order. An unchanged, loaded playlist only
/// refreshes its cover, name and track count; otherwise `download` fetches the tracks. A failed
/// download keeps an existing copy untouched (old snapshot, so the next sync retries) or stores
/// the listed playlist unloaded. Returns false if any download failed.
fn merge_listed(
    store: &mut Vec<Playlist>,
    listed: &[Playlist],
    mut download: impl FnMut(&Playlist) -> Option<Vec<Playable>>,
) -> bool {
    let mut items_complete = true;
    for remote in listed {
        let pos = store.iter().position(|p| p.id == remote.id);
        if let Some(i) = pos
            && is_current(std::slice::from_ref(&store[i]), remote)
        {
            let local = &mut store[i];
            local.cover_url = remote.cover_url.clone();
            local.name = remote.name.clone();
            local.num_tracks = remote.num_tracks;
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
                if pos.is_none() {
                    let mut unloaded = remote.clone();
                    unloaded.tracks = None;
                    store.push(unloaded);
                }
            }
        }
    }
    items_complete
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

/// Clears a library's `syncing` flag when dropped, so the flag is released even if the sync
/// thread panics.
struct SyncGuard(Arc<AtomicBool>);

impl Drop for SyncGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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

/// One library write: the internal API first, then the Web API per the fallback classes.
/// True when one of them wrote; the caller updates the store and `.db` only then.
fn write_through(
    api: &WebApi,
    internal: impl FnOnce() -> Result<(), InternalError>,
    web: impl FnOnce() -> bool,
) -> bool {
    let skip = !api.internal_usable(Area::Library);
    run_with_fallback(
        skip,
        || {
            internal().inspect_err(|err| {
                if decide_after(err) == Action::Fail {
                    warn!("Could not update the library: {err}");
                }
            })
        },
        || {
            if !skip {
                api.count_fallback(Area::Library);
            }
            web().then_some(())
        },
        |err| api.latch_area(Area::Library, err),
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
    fn merge_listed_stores_failed_downloads_unloaded() {
        let mut store = vec![Playlist {
            tracks: Some(vec![]),
            ..listed("a", "1")
        }];
        let complete = merge_listed(
            &mut store,
            &[listed("a", "2"), listed("b", "1"), listed("c", "1")],
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
        assert!(merge_listed(&mut store, &[listed("a", "1")], |_| {
            calls += 1;
            None
        }));
        assert_eq!(calls, 0);
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
            || panic!("internal ran while latched"),
            || true
        ));
        assert_eq!(api.area_counts(Area::Library).fallback, 3);
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

        library.update_library();

        assert!(*library.is_done.read().unwrap());
        assert!(library.syncing.load(Ordering::SeqCst));
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
}
