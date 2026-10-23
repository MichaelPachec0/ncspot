//! Headless probes: a real WebApi on the cached librespot login, no UI. They exercise the model
//! paths the views use and check that metadata stays on the internal API. Run with
//! `cargo test probe -- --ignored`. They never print credentials, tokens or the username.

use std::sync::{Arc, RwLock};

use librespot_core::{Session, SessionConfig, cache::Cache};
use rspotify::model::AlbumType;
use rspotify::prelude::Id;

use crate::application::ASYNC_RUNTIME;
use crate::authentication::SPOTIFY_CLIENT_ID;
use crate::internal_api::Area;
use crate::spotify_api::WebApi;
use crate::traits::ListItem;

const TRACK: &str = "4uLU6hMCjMI75M1A2tKUQC";
const ARTIST: &str = "0OdUWJ0sBjDrqHygGUXeCF";
const SHOW: &str = "4rOoJ6Egrf8K2IrywzwOMk";
/// J. S. Bach: many albums with more than 50 tracks, for the album paging probe.
const LONG_ALBUM_ARTIST: &str = "5aIqB5nVVvmFsvSdExz408";

fn with_api<T>(f: impl FnOnce(&WebApi) -> T) -> T {
    let rt = ASYNC_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    });
    let cache = Cache::new(
        Some(crate::config::cache_path("librespot")),
        None,
        None,
        None,
    )
    .unwrap();
    let credentials = cache.credentials().expect("cached librespot credentials");
    let session = rt.block_on(async {
        let session = Session::new(
            SessionConfig {
                client_id: SPOTIFY_CLIENT_ID.to_string(),
                ..Default::default()
            },
            None,
        );
        session.connect(credentials, false).await.expect("login");
        session
    });
    let mut api = WebApi::new(&crate::config::Config::new_for_test());
    api.set_session(Arc::new(RwLock::new(Some(session))));
    f(&api)
}

fn assert_internal_only(api: &WebApi) {
    let counts = api.area_counts(Area::Metadata);
    assert!(
        counts.internal_ok > 0,
        "metadata went through the internal API"
    );
    assert_eq!(counts.fallback, 0, "no Web API fallback");
    assert!(!counts.latched);
}

#[test]
#[ignore = "needs cached librespot credentials and network"]
fn probe_album_of_a_track() {
    with_api(|api| {
        let track = api.track(TRACK).unwrap();
        let album_id = track.album.id.as_ref().unwrap().id().to_string();
        let album = api.album(&album_id).unwrap();
        assert!(album.tracks.total > 0);
        assert_internal_only(api);
    });
}

#[test]
#[ignore = "needs cached librespot credentials and network"]
fn probe_a_long_album_pages_to_its_total() {
    with_api(|api| {
        let albums = api.artist_albums(LONG_ALBUM_ARTIST, Some(AlbumType::Album));
        let candidates: Vec<String> = albums
            .items
            .read()
            .unwrap()
            .iter()
            .filter_map(|a| a.id.clone())
            .take(20)
            .collect();
        let mut errors = 0;
        let mut long = None;
        for id in &candidates {
            match api.album(id) {
                Ok(a) if a.tracks.total > 50 => {
                    long = Some((id.clone(), a));
                    break;
                }
                Ok(_) => {}
                Err(_) => errors += 1,
            }
        }
        let (id, album) = long.unwrap_or_else(|| {
            panic!(
                "an album with more than 50 tracks among the first 20 ({errors} album calls failed); \
                 pick another artist"
            )
        });
        assert_eq!(errors, 0, "no album call failed before the long album");
        let mut loaded = album.tracks.items.len() as u32;
        while loaded < album.tracks.total {
            let page = api.album_tracks(&id, 50, loaded).expect("next page");
            assert!(!page.items.is_empty(), "a page made progress");
            loaded += page.items.len() as u32;
        }
        assert_eq!(loaded, album.tracks.total);
        // Album::load_all_tracks needs a Spotify (player, queue, events), which is not cheap to
        // build headless, so the paging loop above mirrors it and the model conversion is
        // checked against the paged count instead.
        let model = crate::model::album::Album::from(&album);
        assert_eq!(model.total_tracks, Some(loaded as usize));
        assert_internal_only(api);
    });
}

#[test]
#[ignore = "needs cached librespot credentials and network"]
fn probe_artist_albums_and_top_tracks() {
    with_api(|api| {
        let albums = api.artist_albums(ARTIST, Some(AlbumType::Album));
        albums.fetch_all();
        assert!(!albums.failed());
        assert_eq!(albums.items.read().unwrap().len(), albums.total as usize);
        assert!(albums.total > 0);
        let top = api.artist_top_tracks(ARTIST).unwrap();
        assert!(!top.is_empty());
        assert!(top.iter().any(|t| t.is_playable()), "some top track plays");
        assert_internal_only(api);
    });
}

#[test]
#[ignore = "needs cached librespot credentials and network"]
fn probe_show_episodes_page_to_the_end() {
    with_api(|api| {
        let episodes = api.show_episodes(SHOW);
        episodes.fetch_all();
        assert!(!episodes.failed());
        // Episodes Spotify pulled (status 451) are skipped, so the loaded count may trail the total
        // by a few; it must never exceed it.
        let loaded = episodes.items.read().unwrap().len();
        assert!(loaded <= episodes.total as usize);
        assert!(
            loaded * 100 >= episodes.total as usize * 99,
            "at most 1% skipped"
        );
        assert!(episodes.total > 50, "more than one page");
        assert_internal_only(api);
    });
}

#[test]
#[ignore = "needs cached librespot credentials and network"]
fn probe_current_user() {
    with_api(|api| {
        let user = api.current_user().unwrap();
        // Never print it; only check that something came back.
        assert!(!user.id.id().is_empty());
        assert_internal_only(api);
    });
}
