//! Turning the items of a playlist (URIs from the internal API) into a list of `Playable`s:
//! known metadata where there is some, local files decoded from their URI, and an unplayable
//! placeholder for everything else, so positions always match the playlist's item indexes.

use std::collections::{HashMap, HashSet};

use chrono::DateTime;
use librespot_core::SpotifyUri;

use crate::internal_api::playlist::{PlaylistItem, decode_name as decode};
use crate::model::playable::Playable;
use crate::model::track::Track;

const LOCAL_PREFIX: &str = "spotify:local:";

/// The title of a placeholder for an item whose metadata is not available.
pub const UNAVAILABLE: &str = "Unavailable";

fn is_local(uri: &str) -> bool {
    uri.starts_with(LOCAL_PREFIX)
}

/// An unplayable stand-in for an item that has no metadata (removed, region-locked, or one the
/// service did not return).
pub fn placeholder(uri: &str) -> Playable {
    let id = if is_local(uri) {
        None
    } else {
        SpotifyUri::from_uri(uri).ok().map(|parsed| parsed.to_id())
    };
    Playable::Track(Track {
        id,
        uri: uri.to_string(),
        title: UNAVAILABLE.to_string(),
        track_number: 0,
        disc_number: 0,
        duration: 0,
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
        is_playable: Some(false),
    })
}

/// A local file from `spotify:local:<artist>:<album>:<title>:<seconds>`; `None` when the URI
/// does not have that shape.
pub fn local_track(uri: &str) -> Option<Track> {
    let rest = uri.strip_prefix(LOCAL_PREFIX)?;
    let parts: Vec<&str> = rest.split(':').collect();
    let [artist, album, title, seconds] = parts[..] else {
        return None;
    };
    let seconds: u32 = seconds.parse().ok()?;
    let artist = decode(artist);
    let album = decode(album);
    Some(Track {
        id: None,
        uri: uri.to_string(),
        title: decode(title),
        track_number: 0,
        disc_number: 0,
        duration: seconds.saturating_mul(1000),
        artists: if artist.is_empty() {
            Vec::new()
        } else {
            vec![artist]
        },
        artist_ids: Vec::new(),
        album: (!album.is_empty()).then_some(album),
        album_id: None,
        album_artists: Vec::new(),
        cover_url: None,
        url: String::new(),
        added_at: None,
        list_index: 0,
        is_local: true,
        is_playable: Some(true),
    })
}

/// One `Playable` per item, in order: a local file for local URIs, a copy of what `lookup`
/// finds for the others (with the item's add time), a placeholder where it finds nothing.
/// `list_index` is the item's position.
pub fn assemble<'a>(
    items: &[PlaylistItem],
    lookup: impl Fn(&str) -> Option<&'a Playable>,
) -> Vec<Playable> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let mut playable = if is_local(&item.uri) {
                local_track(&item.uri)
                    .map(Playable::Track)
                    .unwrap_or_else(|| placeholder(&item.uri))
            } else {
                lookup(&item.uri)
                    .cloned()
                    .unwrap_or_else(|| placeholder(&item.uri))
            };
            playable.set_added_at(item.added_at.and_then(DateTime::from_timestamp_millis));
            playable.set_list_index(index);
            playable
        })
        .collect()
}

/// The distinct non-local URIs of `items` that `known` has no metadata for, in request order.
pub fn unknown_uris(items: &[PlaylistItem], known: &HashMap<String, Playable>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .iter()
        .filter(|item| !is_local(&item.uri) && !known.contains_key(&item.uri))
        .filter(|item| seen.insert(item.uri.as_str()))
        .map(|item| item.uri.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::ListItem;

    fn item(uri: &str) -> PlaylistItem {
        PlaylistItem {
            uri: uri.to_string(),
            added_at: Some(0),
        }
    }

    fn track_playable(uri: &str) -> Playable {
        let Playable::Track(mut track) = placeholder(uri) else {
            unreachable!()
        };
        track.title = "Known".to_string();
        track.is_playable = Some(true);
        Playable::Track(track)
    }

    #[test]
    fn assemble_keeps_positions_with_placeholders() {
        let items = [
            item("spotify:track:A"),
            item("spotify:track:B"),
            item("spotify:local:Art:Alb:Song+One:200"),
        ];
        let known = HashMap::from([(
            "spotify:track:A".to_string(),
            track_playable("spotify:track:A"),
        )]);
        let out = assemble(&items, |uri| known.get(uri));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].uri(), "spotify:track:A");
        assert!(
            !out[1].is_playable(),
            "unknown item is an unplayable placeholder"
        );
        let local = out[2].track().unwrap();
        assert!(local.is_local);
        assert_eq!(local.title, "Song One");
        assert_eq!(local.duration, 200_000);
        for (i, p) in out.iter().enumerate() {
            assert_eq!(p.list_index(), i);
        }
    }

    #[test]
    fn malformed_local_uri_is_a_placeholder() {
        assert!(local_track("spotify:local:only:three").is_none());
        let out = assemble(&[item("spotify:local:only:three")], |_| None);
        assert!(!out[0].is_playable());
    }

    #[test]
    fn local_track_decodes_fields() {
        let t = local_track("spotify:local:A%20B::T%C3%A9st:7").unwrap();
        assert_eq!(t.artists, vec!["A B".to_string()]);
        assert_eq!(t.album, None);
        assert_eq!(t.title, "T\u{e9}st");
        assert_eq!(t.id, None);
    }

    #[test]
    fn placeholder_is_titled_unavailable_and_keeps_the_id() {
        let p = placeholder("spotify:track:4uLU6hMCjMI75M1A2tKUQC");
        assert_eq!(p.track().unwrap().title, UNAVAILABLE);
        assert_eq!(p.id().as_deref(), Some("4uLU6hMCjMI75M1A2tKUQC"));
        assert_eq!(placeholder("garbage").id(), None);
    }

    #[test]
    fn unknown_uris_are_distinct_and_skip_local_and_known() {
        let items = [
            item("spotify:track:A"),
            item("spotify:track:B"),
            item("spotify:track:B"),
            item("spotify:local:a:b:c:1"),
        ];
        let known = HashMap::from([(
            "spotify:track:A".to_string(),
            track_playable("spotify:track:A"),
        )]);
        assert_eq!(
            unknown_uris(&items, &known),
            vec!["spotify:track:B".to_string()]
        );
    }
}
