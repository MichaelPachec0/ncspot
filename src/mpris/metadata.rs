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
    hm.insert(
        "xesam:userRating".to_string(),
        Value::F64(
            playable
                .and_then(|p| p.track())
                .map(|t| match library.is_saved_track(&Playable::Track(t)) {
                    true => 1.0,
                    false => 0.0,
                })
                .unwrap_or(0.0),
        ),
    );

    hm
}
