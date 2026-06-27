use std::collections::HashMap;

use zbus::zvariant::{ObjectPath, Value};

use crate::library::Library;
use crate::model::playable::Playable;
use crate::spotify::Spotify;
use crate::traits::ListItem;

// NOTE: do NOT import `Album` here — `open_uri` (which uses it) stays in player.rs,
// so metadata.rs must not import Album or clippy's unused-import lint will fail.

/// Build an MPRIS metadata map for `playable`. `track_path` is used verbatim as
/// `mpris:trackid` so it matches the TrackList object path for this entry.
pub fn build_metadata(
    playable: Option<&Playable>,
    track_path: ObjectPath<'static>,
    spotify: &Spotify,
    library: &Library,
) -> HashMap<String, Value<'static>> {
    let mut hm = HashMap::new();

    // Fetch full track details if this is based on a SimplifiedTrack lacking cover_url.
    let playable_full = playable.and_then(|p| match p {
        Playable::Track(track) => {
            if track.cover_url.is_some() {
                Some(Playable::Track(track.clone()))
            } else {
                spotify
                    .api
                    .track(&track.id.clone().unwrap_or_default())
                    .as_ref()
                    .map(|t| Playable::Track(t.into()))
                    .ok()
            }
        }
        Playable::Episode(episode) => Some(Playable::Episode(episode.clone())),
    });
    let playable = playable_full.as_ref();

    hm.insert("mpris:trackid".to_string(), Value::ObjectPath(track_path));
    hm.insert(
        "mpris:length".to_string(),
        Value::I64(playable.map(|t| t.duration() as i64 * 1_000).unwrap_or(0)),
    );
    hm.insert(
        "mpris:artUrl".to_string(),
        Value::Str(
            playable
                .map(|t| t.cover_url().unwrap_or_default())
                .unwrap_or_default()
                .into(),
        ),
    );
    hm.insert(
        "xesam:album".to_string(),
        Value::Str(
            playable
                .and_then(|p| p.track())
                .map(|t| t.album.unwrap_or_default())
                .unwrap_or_default()
                .into(),
        ),
    );
    hm.insert(
        "xesam:albumArtist".to_string(),
        Value::Array(
            playable
                .and_then(|p| p.track())
                .map(|t| t.album_artists)
                .unwrap_or_default()
                .into(),
        ),
    );
    hm.insert(
        "xesam:artist".to_string(),
        Value::Array(
            playable
                .and_then(|p| p.track())
                .map(|t| t.artists)
                .unwrap_or_default()
                .into(),
        ),
    );
    hm.insert(
        "xesam:discNumber".to_string(),
        Value::I32(
            playable
                .and_then(|p| p.track())
                .map(|t| t.disc_number)
                .unwrap_or(0),
        ),
    );
    hm.insert(
        "xesam:title".to_string(),
        Value::Str(
            playable
                .map(|t| match t {
                    Playable::Track(t) => t.title.clone(),
                    Playable::Episode(ep) => ep.name.clone(),
                })
                .unwrap_or_default()
                .into(),
        ),
    );
    hm.insert(
        "xesam:trackNumber".to_string(),
        Value::I32(
            playable
                .and_then(|p| p.track())
                .map(|t| t.track_number)
                .unwrap_or(0) as i32,
        ),
    );
    hm.insert(
        "xesam:url".to_string(),
        Value::Str(
            playable
                .map(|t| t.share_url().unwrap_or_default())
                .unwrap_or_default()
                .into(),
        ),
    );
    let saved_rating: f64 = playable
        .and_then(|p| p.track())
        .map(|t| {
            if library.is_saved_track(&Playable::Track(t)) {
                1.0
            } else {
                0.0
            }
        })
        .unwrap_or(0.0);
    hm.insert("xesam:userRating".to_string(), Value::F64(saved_rating));

    // autoRating: mirror saved-track state (0.0/1.0), same source as userRating.
    hm.insert("xesam:autoRating".to_string(), Value::F64(saved_rating));

    // Episode-only fields.
    if let Some(Playable::Episode(ep)) = playable {
        if !ep.release_date.is_empty() {
            hm.insert(
                "xesam:contentCreated".to_string(),
                Value::Str(ep.release_date.clone().into()),
            );
        }
        if !ep.description.is_empty() {
            hm.insert(
                "xesam:comment".to_string(),
                Value::Array(vec![ep.description.clone()].into()),
            );
        }
    }

    hm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::events::EventManager;
    use crate::model::episode::Episode;
    use crate::model::track::Track;

    fn ctx() -> (Spotify, std::sync::Arc<Library>) {
        let cfg = Config::new_for_test();
        let ev = EventManager::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        let library = Library::new_for_test(ev, spotify.clone(), cfg);
        (spotify, library)
    }

    fn sample_episode() -> Episode {
        Episode {
            id: "ep1".to_string(),
            uri: "spotify:episode:ep1".to_string(),
            duration: 60_000,
            name: "Ep One".to_string(),
            description: "A description".to_string(),
            release_date: "2021-05-04".to_string(),
            cover_url: Some("http://img".to_string()),
            added_at: None,
            list_index: 0,
        }
    }

    #[test]
    fn test_episode_enrichment_fields() {
        let (spotify, library) = ctx();
        let p = Playable::Episode(sample_episode());
        let md = build_metadata(Some(&p), no_track_path_for_test(), &spotify, &library);
        assert_eq!(
            md.get("xesam:contentCreated"),
            Some(&Value::Str("2021-05-04".into()))
        );
        // comment is an array of strings
        match md.get("xesam:comment") {
            Some(Value::Array(_)) => {}
            other => panic!("expected comment array, got {other:?}"),
        }
        // autoRating present for every playable
        assert!(md.contains_key("xesam:autoRating"));
    }

    #[test]
    fn test_track_has_autorating_and_no_content_created() {
        let (spotify, library) = ctx();
        let t = Track {
            id: Some("t1".to_string()),
            uri: "spotify:track:t1".to_string(),
            title: "T".to_string(),
            track_number: 1,
            disc_number: 1,
            duration: 1000,
            artists: vec![],
            artist_ids: vec![],
            album: None,
            album_id: None,
            album_artists: vec![],
            cover_url: Some("x".to_string()),
            url: String::new(),
            added_at: None,
            list_index: 0,
            is_local: false,
            is_playable: Some(true),
        };
        let p = Playable::Track(t);
        let md = build_metadata(Some(&p), no_track_path_for_test(), &spotify, &library);
        assert!(md.contains_key("xesam:autoRating"));
        assert!(!md.contains_key("xesam:contentCreated"));
    }

    fn no_track_path_for_test() -> zbus::zvariant::ObjectPath<'static> {
        zbus::zvariant::ObjectPath::from_static_str_unchecked("/org/ncspot/queue/0")
    }
}
