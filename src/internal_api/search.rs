//! Reading the sections of a Pathfinder search answer into uris and playlist rows, and the
//! rule for the next page's offset.

use std::collections::HashMap;

use serde_json::Value;

use super::hashes::Operation;
use crate::model::playlist::Playlist;

/// Items asked per page.
pub const PAGE: usize = 50;
/// Spotify serves no more than this many results per type.
pub const MAX_TOTAL: usize = 1000;

/// The result types of a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Track,
    Album,
    Artist,
    Playlist,
    Show,
    Episode,
}

impl Kind {
    pub const ALL: [Self; 6] = [
        Self::Track,
        Self::Album,
        Self::Artist,
        Self::Playlist,
        Self::Show,
        Self::Episode,
    ];

    /// The name of the section in the answer.
    pub fn section(self) -> &'static str {
        match self {
            Self::Track => "tracksV2",
            Self::Album => "albumsV2",
            Self::Artist => "artists",
            Self::Playlist => "playlists",
            Self::Show => "podcasts",
            Self::Episode => "episodes",
        }
    }

    /// The type's own operation; `Episode` has none and uses `Operation::Desktop`.
    pub fn operation(self) -> Operation {
        match self {
            Self::Track => Operation::Tracks,
            Self::Album => Operation::Albums,
            Self::Artist => Operation::Artists,
            Self::Playlist => Operation::Playlists,
            Self::Show => Operation::Podcasts,
            Self::Episode => Operation::Desktop,
        }
    }
}

/// One section of an answer.
#[derive(Debug, Clone)]
pub struct Section {
    /// Uris in the answer's order (odd items left out).
    pub uris: Vec<String>,
    /// Rows for `Kind::Playlist`, in the answer's order; empty for other kinds.
    pub playlists: Vec<Playlist>,
    /// Length of the `items` array, odd items included.
    pub item_count: usize,
    /// `totalCount`, capped at `MAX_TOTAL`; 0 when absent.
    pub total: usize,
    /// `Some(true)` when `pagingInfo.nextOffset` is null, `Some(false)` when it is a
    /// number, `None` when the section has no `pagingInfo` object.
    pub last_page: Option<bool>,
}

/// The `data` object of one item: tracks wrap it in an extra `item` object.
fn item_data(item: &Value, kind: Kind) -> Option<&Value> {
    match kind {
        Kind::Track => item.pointer("/item/data"),
        _ => item.get("data"),
    }
}

/// `None`: the section is missing, not an object, or has no `items` array. Items without a
/// uri are left out; a section whose items all lack one reads as a section with no rows (its
/// `item_count` still counts them).
pub fn read_section(search_v2: &Value, kind: Kind) -> Option<Section> {
    let section = search_v2.get(kind.section())?.as_object()?;
    let items = section.get("items")?.as_array()?;

    let mut uris = Vec::new();
    let mut playlists = Vec::new();
    for data in items.iter().filter_map(|item| item_data(item, kind)) {
        let Some(uri) = data.get("uri").and_then(Value::as_str) else {
            continue;
        };
        if kind == Kind::Playlist {
            match playlist_row(data) {
                Some(row) => playlists.push(row),
                None => continue,
            }
        }
        uris.push(uri.to_string());
    }

    let total = section
        .get("totalCount")
        .and_then(Value::as_u64)
        .map_or(0, |n| {
            usize::try_from(n).unwrap_or(usize::MAX).min(MAX_TOTAL)
        });
    let last_page = section
        .get("pagingInfo")
        .and_then(Value::as_object)
        .map(|info| info.get("nextOffset").is_none_or(Value::is_null));

    Some(Section {
        uris,
        playlists,
        item_count: items.len(),
        total,
        last_page,
    })
}

/// All six sections of a `searchDesktop` answer; `None` if any is `None`.
pub fn read_all(search_v2: &Value) -> Option<HashMap<Kind, Section>> {
    Kind::ALL
        .into_iter()
        .map(|kind| Some((kind, read_section(search_v2, kind)?)))
        .collect()
}

/// A playlist row from an item's `data`; `None` when it must be left out.
pub fn playlist_row(data: &Value) -> Option<Playlist> {
    let id = data
        .get("uri")?
        .as_str()?
        .strip_prefix("spotify:playlist:")?;
    let owner = data.pointer("/ownerV2/data")?;
    let owner_id = owner.get("username")?.as_str().filter(|s| !s.is_empty())?;
    Some(Playlist {
        id: id.to_string(),
        name: data
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        owner_id: owner_id.to_string(),
        owner_name: owner.get("name").and_then(Value::as_str).map(String::from),
        snapshot_id: String::new(),
        num_tracks: 0,
        tracks: None,
        collaborative: false,
        cover_url: data
            .pointer("/images/items/0/sources/0/url")
            .and_then(Value::as_str)
            .map(String::from),
        tracks_unknown: true,
    })
}

/// The offset of the page after the one asked with `offset` and `limit`.
pub fn next_offset(offset: usize, limit: usize, section: &Section) -> Option<usize> {
    if section.item_count == 0
        || section.last_page == Some(true)
        || offset.saturating_add(limit) >= section.total
    {
        return None;
    }
    Some(offset + limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn answer() -> serde_json::Value {
        json!({
            "tracksV2": { "totalCount": 427, "items": [
                { "item": { "data": { "uri": "spotify:track:t1" } } },
                { "item": { "data": {} } },
                { "item": { "data": { "uri": "spotify:track:t2" } } } ] },
            "albumsV2": { "totalCount": 3, "items": [ { "data": { "uri": "spotify:album:a1" } } ] },
            "artists": { "totalCount": 0, "items": [] },
            "podcasts": { "totalCount": 2, "items": [ { "data": { "uri": "spotify:show:s1" } } ] },
            "episodes": { "totalCount": 5000, "items": [ { "data": { "uri": "spotify:episode:e1" } } ] },
            "playlists": { "totalCount": 180, "items": [
                { "data": { "uri": "spotify:playlist:p1", "name": "Mix One",
                            "images": { "items": [ { "sources": [ { "url": "https://img/1" } ] } ] },
                            "ownerV2": { "data": { "name": "Owner One", "username": "owner1" } } } },
                { "data": { "uri": "spotify:playlist:p2", "name": "No Owner", "ownerV2": { "data": {} } } },
                { "data": { "uri": "spotify:user:x:collection", "name": "Not A Playlist",
                            "ownerV2": { "data": { "username": "owner2" } } } } ] }
        })
    }

    #[test]
    fn sections_keep_order_and_skip_odd_items() {
        let all = read_all(&answer()).unwrap();
        let t = &all[&Kind::Track];
        assert_eq!(t.uris, ["spotify:track:t1", "spotify:track:t2"]);
        assert_eq!((t.item_count, t.total, t.last_page), (3, 427, None));
        assert_eq!(all[&Kind::Episode].total, MAX_TOTAL);
    }

    #[test]
    fn empty_sections_are_readable() {
        let s = read_section(&answer(), Kind::Artist).unwrap();
        assert_eq!((s.uris.len(), s.item_count, s.total), (0, 0, 0));
        assert_eq!(next_offset(0, PAGE, &s), None);
    }

    #[test]
    fn a_missing_or_unreadable_section_is_none() {
        let mut a = answer();
        a.as_object_mut().unwrap().remove("podcasts");
        assert!(read_all(&a).is_none());
        assert!(read_section(&a, Kind::Track).is_some());
        assert!(read_section(&json!({ "tracksV2": { "items": "x" } }), Kind::Track).is_none());
        assert!(read_section(&json!({ "tracksV2": [] }), Kind::Track).is_none());
    }

    #[test]
    fn items_without_uris_are_a_readable_section_with_no_rows() {
        // Restricted or removed results: this is about the term, not the hash.
        let no_uris =
            json!({ "albumsV2": { "totalCount": 9, "items": [ { "data": {} }, { "data": {} } ] } });
        let s = read_section(&no_uris, Kind::Album).unwrap();
        assert_eq!((s.uris.len(), s.item_count, s.total), (0, 2, 9));
    }

    #[test]
    fn playlist_rows_are_built_from_the_answer() {
        let s = read_section(&answer(), Kind::Playlist).unwrap();
        assert_eq!(s.item_count, 3);
        assert_eq!(s.playlists.len(), 1);
        let p = &s.playlists[0];
        assert_eq!(
            (p.id.as_str(), p.name.as_str(), p.owner_id.as_str()),
            ("p1", "Mix One", "owner1")
        );
        assert_eq!(p.owner_name.as_deref(), Some("Owner One"));
        assert_eq!(p.cover_url.as_deref(), Some("https://img/1"));
        assert!(p.tracks_unknown && p.snapshot_id.is_empty() && !p.collaborative);
    }

    #[test]
    fn paging_info_is_read_when_present() {
        let v = json!({ "tracksV2": { "totalCount": 1000,
            "pagingInfo": { "limit": 50, "nextOffset": null },
            "items": [ { "item": { "data": { "uri": "spotify:track:t1" } } } ] } });
        assert_eq!(read_section(&v, Kind::Track).unwrap().last_page, Some(true));
        let v = json!({ "tracksV2": { "totalCount": 97,
            "pagingInfo": { "limit": 5, "nextOffset": 5 },
            "items": [ { "item": { "data": { "uri": "spotify:track:t1" } } } ] } });
        assert_eq!(
            read_section(&v, Kind::Track).unwrap().last_page,
            Some(false)
        );
    }

    #[test]
    fn next_offset_rules() {
        let sec = |item_count, total, last_page| Section {
            uris: vec![],
            playlists: vec![],
            item_count,
            total,
            last_page,
        };
        // Advances by what was asked, not by what came back (49 items for limit 50).
        assert_eq!(next_offset(0, 50, &sec(49, 180, None)), Some(50));
        assert_eq!(next_offset(50, 50, &sec(50, 180, Some(false))), Some(100));
        assert_eq!(next_offset(150, 50, &sec(30, 180, None)), None);
        assert_eq!(next_offset(50, 50, &sec(0, 500, None)), None);
        assert_eq!(next_offset(900, 50, &sec(50, 1000, Some(true))), None);
        assert_eq!(next_offset(950, 50, &sec(50, 1000, None)), None);
    }

    #[test]
    fn kinds_map_to_sections_and_operations() {
        assert_eq!(Kind::Show.section(), "podcasts");
        assert_eq!(Kind::Show.operation(), Operation::Podcasts);
        assert_eq!(Kind::Episode.operation(), Operation::Desktop);
    }
}
