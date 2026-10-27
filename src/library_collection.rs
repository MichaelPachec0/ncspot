//! Pure parts of the collection v2 library sync: which mode a set syncs in, how a listing or a
//! delta turns the cached list into the new one, and the sort orders.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use log::debug;

use crate::internal_api::Area;
use crate::internal_api::collection::{Delta, Item, Listing};
use crate::internal_api::error::InternalError;
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::episode::Episode;
use crate::model::show::Show;
use crate::model::track::Track;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The area is unusable: use the Web API fetchers.
    Web,
    /// Page the set and refetch metadata for every item.
    Full,
    /// Page the set, fetching metadata only for new items.
    Repage,
    /// Apply the changes since the stored sync token.
    Delta,
}

/// What a fetch learned about a set's membership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Membership {
    Listing(Vec<Item>),
    Changes {
        added: Vec<Item>,
        removed: Vec<String>,
    },
}

pub struct Fetched {
    pub membership: Membership,
    pub sync_token: String,
}

pub trait CollectionSource {
    fn page_all(&self, set: &'static str) -> Result<Listing, InternalError>;
    fn delta(&self, set: &'static str, token: &str) -> Result<Delta, InternalError>;
}

impl CollectionSource for crate::spotify_api::WebApi {
    fn page_all(&self, set: &'static str) -> Result<Listing, InternalError> {
        self.collection_page_all(Area::Library, set)
    }

    fn delta(&self, set: &'static str, token: &str) -> Result<Delta, InternalError> {
        self.collection_delta(set, token)
    }
}

/// A cached library entry that can be matched to a collection item by URI.
pub trait Entry: Clone {
    fn entry_uri(&self) -> Option<String>;
    fn set_added_at(&mut self, secs: i64);
}

impl Entry for Track {
    fn entry_uri(&self) -> Option<String> {
        Some(self.uri.clone())
    }
    fn set_added_at(&mut self, secs: i64) {
        self.added_at = DateTime::from_timestamp(secs, 0);
    }
}

impl Entry for Album {
    fn entry_uri(&self) -> Option<String> {
        self.id.as_ref().map(|id| format!("spotify:album:{id}"))
    }
    fn set_added_at(&mut self, secs: i64) {
        self.added_at = DateTime::from_timestamp(secs, 0);
    }
}

impl Entry for Artist {
    fn entry_uri(&self) -> Option<String> {
        self.id.as_ref().map(|id| format!("spotify:artist:{id}"))
    }
    fn set_added_at(&mut self, _secs: i64) {}
}

impl Entry for Show {
    fn entry_uri(&self) -> Option<String> {
        Some(self.uri.clone())
    }
    fn set_added_at(&mut self, secs: i64) {
        self.added_at = DateTime::from_timestamp(secs, 0);
    }
}

impl Entry for Episode {
    fn entry_uri(&self) -> Option<String> {
        Some(self.uri.clone())
    }
    fn set_added_at(&mut self, secs: i64) {
        self.added_at = DateTime::from_timestamp(secs, 0);
    }
}

/// Newest first, entries without a date last; stable.
pub fn sort_newest_first<T>(items: &mut [T], added: impl Fn(&T) -> Option<DateTime<Utc>>) {
    items.sort_by_key(|a| std::cmp::Reverse(added(a)));
}

/// Albums by album artist (without "The "), year, then title (without "The ").
pub fn sort_albums(albums: &mut [Album]) {
    albums.sort_unstable_by_key(|album| {
        let artist = album.artists.first().map(String::as_str).unwrap_or("");
        let album_artist = artist.strip_prefix("The ").unwrap_or(artist);
        let album_title = album.title.strip_prefix("The ").unwrap_or(&album.title);
        format!(
            "{}{}{}",
            album_artist.to_lowercase(),
            album.year,
            album_title.to_lowercase()
        )
    });
}

/// An album without artists gets its title, so it still sorts and displays.
pub fn with_artist(mut album: Album) -> Album {
    if album.artists.is_empty() {
        album.artists = vec![album.title.clone()];
    }
    album
}

/// Mark exactly the `followed` artists as followed, adding the ones the store lacks. A
/// placeholder (an artist only known from saved tracks) that is now followed takes the fetched
/// artist's data.
pub fn merge_followed(store: &mut Vec<Artist>, followed: Vec<Artist>) {
    let was_followed: Vec<bool> = store.iter().map(|a| a.is_followed).collect();
    for a in store.iter_mut() {
        a.is_followed = false;
    }
    for mut f in followed {
        f.is_followed = true;
        match store.iter().position(|a| a.id == f.id) {
            Some(i) if was_followed.get(i).copied().unwrap_or(true) => store[i].is_followed = true,
            Some(i) => store[i] = f,
            None => store.push(f),
        }
    }
}

pub fn plan_sync(area_usable: bool, full_due: bool, has_token: bool, cache_valid: bool) -> Mode {
    if !area_usable {
        Mode::Web
    } else if full_due {
        Mode::Full
    } else if !has_token || !cache_valid {
        Mode::Repage
    } else {
        Mode::Delta
    }
}

impl Membership {
    pub fn only(&self, kind: &str) -> Self {
        let prefix = format!("spotify:{kind}:");
        let keep = |i: &&Item| i.uri.starts_with(&prefix);
        match self {
            Self::Listing(items) => Self::Listing(items.iter().filter(keep).cloned().collect()),
            Self::Changes { added, removed } => Self::Changes {
                added: added.iter().filter(keep).cloned().collect(),
                removed: removed
                    .iter()
                    .filter(|u| u.starts_with(&prefix))
                    .cloned()
                    .collect(),
            },
        }
    }
}

pub fn fetch_membership(
    source: &impl CollectionSource,
    set: &'static str,
    mode: Mode,
    token: Option<&str>,
) -> Result<Fetched, InternalError> {
    if let (Mode::Delta, Some(token)) = (mode, token) {
        match source.delta(set, token)? {
            Delta::Changes {
                added,
                removed,
                sync_token,
            } => {
                return Ok(Fetched {
                    membership: Membership::Changes { added, removed },
                    sync_token,
                });
            }
            Delta::Refused => debug!("{set}: delta refused, paging the set"),
        }
    }
    let listing = source.page_all(set)?;
    match listing.sync_token {
        Some(sync_token) => Ok(Fetched {
            membership: Membership::Listing(listing.items),
            sync_token,
        }),
        None => Err(InternalError::Transient(format!(
            "{set}: listing incomplete after {} pages",
            listing.pages
        ))),
    }
}

pub fn rebuild<T: Entry>(
    store: &[T],
    membership: &Membership,
    refetch_all: bool,
    fetch: impl FnOnce(&[String]) -> Result<Vec<T>, InternalError>,
) -> Result<Vec<T>, InternalError> {
    let cached: HashMap<String, &T> = store
        .iter()
        .filter_map(|t| t.entry_uri().map(|u| (u, t)))
        .collect();
    let by_uri = |items: Vec<T>| -> HashMap<String, T> {
        items
            .into_iter()
            .filter_map(|t| t.entry_uri().map(|u| (u, t)))
            .collect()
    };
    match membership {
        Membership::Listing(items) => {
            let to_fetch: Vec<String> = items
                .iter()
                .filter(|i| refetch_all || !cached.contains_key(&i.uri))
                .map(|i| i.uri.clone())
                .collect();
            let mut fetched = by_uri(fetch(&to_fetch)?);
            let mut out = Vec::with_capacity(items.len());
            let mut missing = 0;
            for i in items {
                let entry = fetched
                    .remove(&i.uri)
                    .or_else(|| cached.get(&i.uri).map(|t| (*t).clone()));
                match entry {
                    Some(mut t) => {
                        t.set_added_at(i.added_at);
                        out.push(t);
                    }
                    None => missing += 1,
                }
            }
            if missing > 0 {
                debug!("{missing} library items without metadata left out");
            }
            Ok(out)
        }
        Membership::Changes { added, removed } => {
            let removed: HashSet<&String> = removed.iter().collect();
            let added_at: HashMap<&String, i64> =
                added.iter().map(|i| (&i.uri, i.added_at)).collect();
            let mut out: Vec<T> = store
                .iter()
                .filter(|t| t.entry_uri().is_none_or(|u| !removed.contains(&u)))
                .cloned()
                .map(|mut t| {
                    if let Some(secs) = t.entry_uri().and_then(|u| added_at.get(&u).copied()) {
                        t.set_added_at(secs);
                    }
                    t
                })
                .collect();
            let to_fetch: Vec<String> = added
                .iter()
                .filter(|i| !cached.contains_key(&i.uri) && !removed.contains(&i.uri))
                .map(|i| i.uri.clone())
                .collect();
            let mut fetched = by_uri(fetch(&to_fetch)?);
            for i in added.iter().filter(|i| to_fetch.contains(&i.uri)) {
                if let Some(mut t) = fetched.remove(&i.uri) {
                    t.set_added_at(i.added_at);
                    out.push(t);
                }
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal_api::collection::{Delta, Item, Listing};
    use crate::internal_api::error::InternalError;
    use std::cell::RefCell;

    #[derive(Clone, Debug, PartialEq)]
    struct E {
        uri: String,
        added: i64,
        name: &'static str,
    }

    impl Entry for E {
        fn entry_uri(&self) -> Option<String> {
            Some(self.uri.clone())
        }
        fn set_added_at(&mut self, secs: i64) {
            self.added = secs;
        }
    }

    fn e(uri: &str, added: i64, name: &'static str) -> E {
        E {
            uri: uri.into(),
            added,
            name,
        }
    }

    fn item(uri: &str, added_at: i64) -> Item {
        Item {
            uri: uri.into(),
            added_at,
        }
    }

    fn fresh(uris: &[String]) -> Result<Vec<E>, InternalError> {
        Ok(uris.iter().map(|u| e(u, 0, "fresh")).collect())
    }

    #[test]
    fn plan_sync_checks_in_order() {
        assert_eq!(plan_sync(false, true, true, true), Mode::Web);
        assert_eq!(plan_sync(true, true, true, true), Mode::Full);
        assert_eq!(plan_sync(true, false, false, true), Mode::Repage);
        assert_eq!(plan_sync(true, false, true, false), Mode::Repage);
        assert_eq!(plan_sync(true, false, true, true), Mode::Delta);
    }

    #[test]
    fn repage_keeps_cached_fetches_new_and_drops_removed() {
        let store = vec![e("a", 1, "cached"), e("gone", 1, "cached")];
        let listing = Membership::Listing(vec![item("a", 5), item("b", 6)]);
        let asked = RefCell::new(Vec::new());
        let out = rebuild(&store, &listing, false, |uris| {
            asked.borrow_mut().extend_from_slice(uris);
            fresh(uris)
        })
        .unwrap();
        assert_eq!(*asked.borrow(), vec!["b".to_string()]);
        assert_eq!(out, vec![e("a", 5, "cached"), e("b", 6, "fresh")]);
    }

    #[test]
    fn full_refetches_everything_but_keeps_cached_items_it_cannot_resolve() {
        let store = vec![e("a", 1, "cached"), e("pulled", 1, "cached")];
        let listing =
            Membership::Listing(vec![item("a", 5), item("pulled", 4), item("new-pulled", 3)]);
        let out = rebuild(&store, &listing, true, |uris| {
            Ok(uris
                .iter()
                .filter(|u| u.as_str() == "a")
                .map(|u| e(u, 0, "fresh"))
                .collect())
        })
        .unwrap();
        assert_eq!(out, vec![e("a", 5, "fresh"), e("pulled", 4, "cached")]);
    }

    #[test]
    fn delta_applies_and_is_idempotent() {
        let store = vec![e("a", 1, "cached"), e("b", 2, "cached")];
        let changes = Membership::Changes {
            added: vec![item("c", 9), item("a", 7)],
            removed: vec!["b".into(), "never-there".into()],
        };
        let once = rebuild(&store, &changes, false, fresh).unwrap();
        assert_eq!(once, vec![e("a", 7, "cached"), e("c", 9, "fresh")]);
        let twice = rebuild(&once, &changes, false, fresh).unwrap();
        assert_eq!(twice, once);
    }

    #[test]
    fn a_failed_fetch_fails_the_rebuild() {
        let listing = Membership::Listing(vec![item("a", 1)]);
        let out = rebuild::<E>(&[], &listing, false, |_| Err(InternalError::RateLimited));
        assert_eq!(out, Err(InternalError::RateLimited));
    }

    #[test]
    fn only_keeps_one_uri_kind() {
        let m = Membership::Listing(vec![
            item("spotify:track:1", 1),
            item("spotify:album:2", 2),
            item("spotify:local:x:y:z:1", 3),
        ]);
        assert_eq!(
            m.only("track"),
            Membership::Listing(vec![item("spotify:track:1", 1)])
        );
        assert_eq!(
            m.only("album"),
            Membership::Listing(vec![item("spotify:album:2", 2)])
        );
    }

    struct Fake {
        delta: Result<Delta, InternalError>,
        listing: Result<Listing, InternalError>,
        pages: RefCell<u32>,
    }

    impl CollectionSource for Fake {
        fn page_all(&self, _set: &'static str) -> Result<Listing, InternalError> {
            *self.pages.borrow_mut() += 1;
            self.listing.clone()
        }
        fn delta(&self, _set: &'static str, _token: &str) -> Result<Delta, InternalError> {
            self.delta.clone()
        }
    }

    fn listing(token: Option<&str>) -> Listing {
        Listing {
            items: vec![item("a", 1)],
            sync_token: token.map(String::from),
            pages: 1,
        }
    }

    #[test]
    fn delta_mode_uses_the_delta() {
        let fake = Fake {
            delta: Ok(Delta::Changes {
                added: vec![],
                removed: vec!["x".into()],
                sync_token: "t2".into(),
            }),
            listing: Ok(listing(Some("t3"))),
            pages: RefCell::new(0),
        };
        let got = fetch_membership(&fake, "artist", Mode::Delta, Some("t1")).unwrap();
        assert_eq!(got.sync_token, "t2");
        assert_eq!(*fake.pages.borrow(), 0);
    }

    #[test]
    fn a_refused_delta_pages_the_set() {
        let fake = Fake {
            delta: Ok(Delta::Refused),
            listing: Ok(listing(Some("t3"))),
            pages: RefCell::new(0),
        };
        let got = fetch_membership(&fake, "artist", Mode::Delta, Some("t1")).unwrap();
        assert_eq!(got.membership, Membership::Listing(vec![item("a", 1)]));
        assert_eq!(got.sync_token, "t3");
    }

    #[test]
    fn an_incomplete_listing_is_transient() {
        let fake = Fake {
            delta: Ok(Delta::Refused),
            listing: Ok(listing(None)),
            pages: RefCell::new(0),
        };
        let got = fetch_membership(&fake, "artist", Mode::Repage, None);
        assert!(matches!(got, Err(InternalError::Transient(_))));
    }

    #[test]
    fn errors_pass_through() {
        let fake = Fake {
            delta: Err(InternalError::ProtocolChanged("x".into())),
            listing: Ok(listing(Some("t"))),
            pages: RefCell::new(0),
        };
        assert!(matches!(
            fetch_membership(&fake, "artist", Mode::Delta, Some("t1")),
            Err(InternalError::ProtocolChanged(_))
        ));
    }

    #[test]
    fn newest_first_is_stable_and_puts_unknown_last() {
        let mut v = vec![(1, Some(5)), (2, None), (3, Some(9)), (4, Some(5))];
        sort_newest_first(&mut v, |x| {
            x.1.map(|s| chrono::DateTime::from_timestamp(s, 0).unwrap())
        });
        assert_eq!(v.iter().map(|x| x.0).collect::<Vec<_>>(), vec![3, 1, 4, 2]);
    }

    #[test]
    fn an_album_without_artists_gets_its_title() {
        let mut album = crate::model::album::Album::default_for_test("T");
        album.artists.clear();
        assert_eq!(with_artist(album).artists, vec!["T".to_string()]);
    }

    #[test]
    fn merge_followed_sets_flags_and_adds_missing() {
        use crate::model::artist::Artist;
        let mut store = vec![
            Artist::new("a".into(), "A".into()),
            Artist::new("b".into(), "B".into()),
        ];
        store[1].is_followed = true;
        merge_followed(
            &mut store,
            vec![
                Artist::new("a".into(), "A".into()),
                Artist::new("c".into(), "C".into()),
            ],
        );
        let flags: Vec<_> = store
            .iter()
            .map(|a| (a.id.clone().unwrap(), a.is_followed))
            .collect();
        assert_eq!(
            flags,
            vec![("a".into(), true), ("b".into(), false), ("c".into(), true)]
        );
    }

    #[test]
    fn a_placeholder_that_becomes_followed_takes_the_fetched_artist() {
        use crate::model::artist::Artist;
        let mut store = vec![Artist::new("a".into(), "placeholder".into())];
        let mut fetched = Artist::new("a".into(), "Fetched".into());
        fetched.url = Some("u".into());
        merge_followed(&mut store, vec![fetched]);
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].name, "Fetched");
        assert_eq!(store[0].url.as_deref(), Some("u"));
        assert!(store[0].is_followed);
    }
}
