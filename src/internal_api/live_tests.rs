use librespot_core::{Session, SessionConfig, cache::Cache};

use rspotify::prelude::Id;

use super::metadata;
use crate::authentication::SPOTIFY_CLIENT_ID;

/// Known ids (public catalog items that have existed for years). The album under test is the
/// track's own album, so no album id needs maintaining.
const TRACK: &str = "4uLU6hMCjMI75M1A2tKUQC";
const ARTIST: &str = "0OdUWJ0sBjDrqHygGUXeCF";
const SHOW: &str = "0ofXAdFIQQRsCYj9754UFx";

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
        eprintln!("user.id: {}", user.id);
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
        let eps = rt
            .block_on(metadata::episodes_by_uri(
                s,
                ids[..2.min(ids.len())].to_vec(),
            ))
            .unwrap();
        assert!(
            eps.len() == 2 && eps[0].release_date >= eps[1].release_date,
            "librespot returned episodes oldest first: reverse in show_episode_ids"
        );
    });
}
