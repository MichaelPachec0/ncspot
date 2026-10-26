use librespot_core::{Session, SessionConfig, cache::Cache};

use rspotify::prelude::Id;

use super::metadata;
use crate::authentication::SPOTIFY_CLIENT_ID;

/// Known ids (public catalog items that have existed for years). The album under test is the
/// track's own album, so no album id needs maintaining.
const TRACK: &str = "4uLU6hMCjMI75M1A2tKUQC";
const ARTIST: &str = "0OdUWJ0sBjDrqHygGUXeCF";
const SHOW: &str = "4rOoJ6Egrf8K2IrywzwOMk";
/// An episode Spotify pulled (status 451 on the batch path).
const REMOVED_EPISODE: &str = "2rKMiQw2A58GL5ZHH8jfHo";

fn with_session<T>(f: impl FnOnce(&tokio::runtime::Runtime, Session) -> T) -> T {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
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
    f(&rt, session)
}

#[test]
#[ignore = "needs cached librespot credentials and network"]
fn track_album_artist_show_load() {
    with_session(|rt, s| {
        let t = rt
            .block_on(metadata::track(s.clone(), TRACK.into()))
            .unwrap();
        assert!(!t.name.is_empty());
        let album_id = t.album.id.as_ref().unwrap().id().to_string();
        let a = rt
            .block_on(metadata::album(s.clone(), album_id.clone()))
            .unwrap();
        assert!(a.tracks.total > 0);
        assert_eq!(a.tracks.items.len(), (a.tracks.total as usize).min(50));
        let album_unplayable = a
            .tracks
            .items
            .iter()
            .filter(|t| t.is_playable == Some(false))
            .count();
        eprintln!(
            "album tracks: {}, is_playable == Some(false): {album_unplayable}",
            a.tracks.items.len()
        );
        assert!(
            a.tracks.items.iter().any(|t| t.is_playable == Some(true)),
            "no album track is playable"
        );
        let page = rt
            .block_on(metadata::album_tracks(s.clone(), album_id, 2, 1))
            .unwrap();
        assert_eq!(page.offset, 1);
        assert!(page.items.len() <= 2);
        let top = rt
            .block_on(metadata::artist_top_tracks(s.clone(), ARTIST.into()))
            .unwrap();
        assert!(!top.is_empty());
        let top_unplayable = top.iter().filter(|t| t.is_playable == Some(false)).count();
        eprintln!(
            "top tracks: {}, is_playable == Some(false): {top_unplayable}",
            top.len()
        );
        let ids = rt
            .block_on(metadata::artist_album_ids(s.clone(), ARTIST.into(), None))
            .unwrap();
        assert!(!ids.is_empty());
        let show = rt.block_on(metadata::show(s.clone(), SHOW.into())).unwrap();
        assert!(!show.episodes.items.is_empty());
        let user = rt.block_on(metadata::current_user(s)).unwrap();
        assert!(!user.id.to_string().is_empty());
    });
}

#[test]
#[ignore = "needs cached librespot credentials and network"]
fn show_episode_order_is_newest_first() {
    with_session(|rt, s| {
        let ids = rt
            .block_on(metadata::show_episode_ids(s.clone(), SHOW.into()))
            .unwrap();
        // Some ids may be pulled episodes (dropped by episodes_by_uri), so look at a window.
        let eps = rt
            .block_on(metadata::episodes_by_uri(
                s,
                ids[..5.min(ids.len())].to_vec(),
                metadata::Missing::Fail,
            ))
            .unwrap();
        assert!(eps.len() >= 2, "fewer than two episodes loaded");
        assert!(
            eps.windows(2)
                .all(|w| w[0].release_date >= w[1].release_date),
            "librespot returned episodes oldest first: reverse in show_episode_ids"
        );
    });
}

#[test]
#[ignore = "needs cached librespot credentials and network"]
fn removed_episode_is_not_found_and_does_not_latch() {
    use super::error::InternalError;
    use super::fallback::{Action, decide_after};
    with_session(|rt, s| {
        let err = rt
            .block_on(metadata::episode(s, REMOVED_EPISODE.into()))
            .unwrap_err();
        assert_eq!(err, InternalError::NotFound);
        assert_eq!(decide_after(&err), Action::WebOnce);
    });
}

#[test]
#[ignore = "needs cached librespot credentials and network"]
fn collection_sets_page_completely_and_accept_a_delta() {
    use super::collection::{
        self, Delta, Listing, SET_ARTIST, SET_COLLECTION, SET_LISTEN_LATER, SET_SHOW,
    };
    with_session(|rt, session| {
        for set in [SET_COLLECTION, SET_ARTIST, SET_SHOW, SET_LISTEN_LATER] {
            let mut listing = Listing::default();
            let mut token = String::new();
            loop {
                let page = rt
                    .block_on(collection::page(session.clone(), set, token.clone()))
                    .expect("page");
                match collection::absorb(&mut listing, page) {
                    Some(next) => token = next,
                    None => break,
                }
            }
            assert!(listing.complete(), "{set}: complete listing");
            println!(
                "{set}: {} items, {} pages",
                listing.items.len(),
                listing.pages
            );
            let sync = listing.sync_token.unwrap();
            let delta = rt
                .block_on(collection::delta(session.clone(), set, sync))
                .expect("delta");
            assert!(
                matches!(delta, Delta::Changes { .. }),
                "{set}: delta accepted"
            );
        }
    });
}
