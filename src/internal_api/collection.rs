//! Spotify's collection v2 service (liked songs, saved albums, followed artists, saved shows,
//! Your Episodes) over the librespot session.

/// Messages generated from the vendored `collection2v2.proto` (librespot ships the file but
/// does not compile it).
#[allow(clippy::all, clippy::use_self, dead_code)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/collection_proto/mod.rs"));
    pub use collection2v2::*;
}

use http::{HeaderMap, HeaderValue, Method, header};
use librespot_core::Session;
use log::debug;
use protobuf::Message;

use super::error::{InternalError, classify};
use proto::{CollectionItem, DeltaRequest, DeltaResponse, PageRequest, PageResponse, WriteRequest};

pub const SET_COLLECTION: &str = "collection";
pub const SET_ARTIST: &str = "artist";
pub const SET_SHOW: &str = "show";
pub const SET_LISTEN_LATER: &str = "listenlater";

/// Items per page request.
pub const PAGE_LIMIT: i32 = 300;
/// Most pages read for one set; past it the listing is incomplete.
pub const MAX_PAGES: usize = 200;

/// The collection service rejects `application/x-protobuf` with HTTP 400.
const CONTENT_TYPE: &str = "application/vnd.collection-v2.spotify.proto";

/// One saved item: its URI and the time it was added (Unix seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub uri: String,
    pub added_at: i64,
}

/// A whole set read page by page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    pub items: Vec<Item>,
    pub sync_token: Option<String>,
    pub pages: usize,
}

/// The changes to a set since a sync token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delta {
    Changes {
        added: Vec<Item>,
        removed: Vec<String>,
        sync_token: String,
    },
    /// The service cannot compute a delta from that token; read the whole set again.
    Refused,
}

impl Listing {
    /// Only a listing whose last page carried a sync token is the whole set.
    pub fn complete(&self) -> bool {
        self.sync_token.is_some()
    }
}

pub fn absorb(listing: &mut Listing, page: PageResponse) -> Option<String> {
    listing.pages += 1;
    listing.items.extend(page.items.into_iter().map(|i| Item {
        uri: i.uri,
        added_at: i64::from(i.added_at),
    }));
    if page.next_page_token.is_empty() {
        listing.sync_token = (!page.sync_token.is_empty()).then_some(page.sync_token);
        return None;
    }
    if listing.pages >= MAX_PAGES {
        return None;
    }
    Some(page.next_page_token)
}

/// An empty token could not be sent back for the next delta, so that answer is no delta.
pub fn delta_from(resp: DeltaResponse) -> Delta {
    if !resp.delta_update_possible || resp.sync_token.is_empty() {
        return Delta::Refused;
    }
    let (removed, added): (Vec<_>, Vec<_>) = resp.items.into_iter().partition(|i| i.is_removed);
    Delta::Changes {
        added: added
            .into_iter()
            .map(|i| Item {
                uri: i.uri,
                added_at: i64::from(i.added_at),
            })
            .collect(),
        removed: removed.into_iter().map(|i| i.uri).collect(),
        sync_token: resp.sync_token,
    }
}

/// A 400 from the collection service means our request no longer matches what it expects.
pub fn classify_collection(err: &librespot_core::Error) -> InternalError {
    match err.kind {
        librespot_core::error::ErrorKind::InvalidArgument => {
            InternalError::ProtocolChanged(format!("collection request rejected: {err}"))
        }
        _ => classify(err),
    }
}

/// The answer to a delta request. A 400 there means the service rejected the token (stale or
/// unknown), so the set is read again instead of the area being latched; a 400 on paging is
/// still a protocol change.
fn delta_answer(answer: Result<Vec<u8>, librespot_core::Error>) -> Result<Delta, InternalError> {
    match answer {
        Err(e) if e.kind == librespot_core::error::ErrorKind::InvalidArgument => Ok(Delta::Refused),
        Err(e) => Err(classify_collection(&e)),
        Ok(bytes) => DeltaResponse::parse_from_bytes(&bytes)
            .map(delta_from)
            .map_err(|e| InternalError::ProtocolChanged(format!("collection delta parse: {e}"))),
    }
}

fn encode<M: Message>(message: &M) -> Result<Vec<u8>, InternalError> {
    message
        .write_to_bytes()
        .map_err(|e| InternalError::ProtocolChanged(format!("encode: {e}")))
}

async fn post(
    session: &Session,
    endpoint: &str,
    body: Vec<u8>,
) -> Result<Vec<u8>, librespot_core::Error> {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
    headers.insert(header::ACCEPT, HeaderValue::from_static(CONTENT_TYPE));
    session
        .spclient()
        .request(
            &Method::POST,
            endpoint,
            Some(headers),
            Some(body.as_slice()),
        )
        .await
        .map(|bytes| bytes.to_vec())
}

pub async fn page(
    session: Session,
    set: &'static str,
    token: String,
) -> Result<PageResponse, InternalError> {
    debug!("collection page: {set}");
    let request = PageRequest {
        username: session.username(),
        set: set.to_string(),
        pagination_token: token,
        limit: PAGE_LIMIT,
        ..Default::default()
    };
    let bytes = post(&session, "/collection/v2/paging", encode(&request)?)
        .await
        .map_err(|e| classify_collection(&e))?;
    PageResponse::parse_from_bytes(&bytes)
        .map_err(|e| InternalError::ProtocolChanged(format!("collection page parse: {e}")))
}

pub async fn delta(
    session: Session,
    set: &'static str,
    token: String,
) -> Result<Delta, InternalError> {
    debug!("collection delta: {set}");
    let request = DeltaRequest {
        username: session.username(),
        set: set.to_string(),
        last_sync_token: token,
        ..Default::default()
    };
    delta_answer(post(&session, "/collection/v2/delta", encode(&request)?).await)
}

/// Items per write request.
#[expect(dead_code)]
pub const WRITE_BATCH: usize = 100;

/// A random id for one write, as the desktop client sends.
fn update_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

/// One write of `uris` to `set`: all added, or all removed. `now` is Unix seconds; the field
/// is 32 bits wide.
pub fn write_request(
    username: String,
    set: &str,
    uris: &[String],
    removed: bool,
    now: i64,
    update_id: String,
) -> WriteRequest {
    let added_at = i32::try_from(now).unwrap_or(i32::MAX);
    WriteRequest {
        username,
        set: set.to_string(),
        items: uris
            .iter()
            .map(|uri| CollectionItem {
                uri: uri.clone(),
                added_at,
                is_removed: removed,
                ..Default::default()
            })
            .collect(),
        client_update_id: update_id,
        ..Default::default()
    }
}

/// Add or remove `uris` in `set` (at most [WRITE_BATCH]). Set membership makes a repeated
/// write harmless, so librespot's own network retry is fine here.
#[expect(dead_code)]
pub async fn write(
    session: Session,
    set: &'static str,
    uris: Vec<String>,
    removed: bool,
) -> Result<(), InternalError> {
    debug!("collection write: {set}, {} items", uris.len());
    let request = write_request(
        session.username(),
        set,
        &uris,
        removed,
        chrono::Utc::now().timestamp(),
        update_id(),
    );
    post(&session, "/collection/v2/write", encode(&request)?)
        .await
        .map(|_| ())
        .map_err(|e| classify_collection(&e))
}

#[cfg(test)]
mod tests {
    use super::proto::{
        CollectionItem, DeltaRequest, DeltaResponse, PageRequest, PageResponse, WriteRequest,
    };
    use protobuf::Message;

    #[test]
    fn write_request_marks_every_item() {
        let uris = vec!["spotify:track:a".to_string(), "spotify:track:b".to_string()];
        let req = super::write_request(
            "u".into(),
            "collection",
            &uris,
            true,
            1_700_000_000,
            "0123456789abcdef".into(),
        );
        assert_eq!(req.username, "u");
        assert_eq!(req.set, "collection");
        assert_eq!(req.client_update_id, "0123456789abcdef");
        assert_eq!(req.items.len(), 2);
        assert!(
            req.items
                .iter()
                .all(|i| i.is_removed && i.added_at == 1_700_000_000)
        );
        assert_eq!(req.items[1].uri, "spotify:track:b");
        let bytes = req.write_to_bytes().unwrap();
        assert_eq!(WriteRequest::parse_from_bytes(&bytes).unwrap(), req);
    }

    #[test]
    fn write_request_clamps_a_time_past_i32() {
        let req = super::write_request(
            "u".into(),
            "show",
            &["spotify:show:s".into()],
            false,
            i64::MAX,
            String::new(),
        );
        assert_eq!(req.items[0].added_at, i32::MAX);
        assert!(!req.items[0].is_removed);
    }

    #[test]
    fn update_ids_are_16_hex_chars() {
        let id = super::update_id();
        assert_eq!(id.len(), 16);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn page_request_round_trips() {
        let req = PageRequest {
            username: "u".into(),
            set: "collection".into(),
            pagination_token: "t".into(),
            limit: 300,
            ..Default::default()
        };
        let bytes = req.write_to_bytes().unwrap();
        assert_eq!(PageRequest::parse_from_bytes(&bytes).unwrap(), req);
    }

    #[test]
    fn delta_request_round_trips() {
        let req = DeltaRequest {
            username: "u".into(),
            set: "artist".into(),
            last_sync_token: "s".into(),
            ..Default::default()
        };
        let bytes = req.write_to_bytes().unwrap();
        assert_eq!(DeltaRequest::parse_from_bytes(&bytes).unwrap(), req);
    }

    #[test]
    fn page_response_decodes_fixed_bytes() {
        // items { uri: "spotify:track:a" added_at: 5 } next_page_token: "n" sync_token: ""
        let bytes = [
            0x0a, 0x13, 0x0a, 0x0f, b's', b'p', b'o', b't', b'i', b'f', b'y', b':', b't', b'r',
            b'a', b'c', b'k', b':', b'a', 0x10, 0x05, 0x12, 0x01, b'n',
        ];
        let resp = PageResponse::parse_from_bytes(&bytes).unwrap();
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].uri, "spotify:track:a");
        assert_eq!(resp.items[0].added_at, 5);
        assert_eq!(resp.next_page_token, "n");
        assert!(resp.sync_token.is_empty());
    }

    #[test]
    fn delta_response_decodes_fixed_bytes() {
        // delta_update_possible: true items { uri: "spotify:artist:b" is_removed: true } sync_token: "s"
        let bytes = [
            0x08, 0x01, 0x12, 0x14, 0x0a, 0x10, b's', b'p', b'o', b't', b'i', b'f', b'y', b':',
            b'a', b'r', b't', b'i', b's', b't', b':', b'b', 0x18, 0x01, 0x1a, 0x01, b's',
        ];
        let resp = DeltaResponse::parse_from_bytes(&bytes).unwrap();
        assert!(resp.delta_update_possible);
        assert_eq!(
            resp.items
                .first()
                .map(|i: &CollectionItem| (i.uri.as_str(), i.is_removed)),
            Some(("spotify:artist:b", true))
        );
        assert_eq!(resp.sync_token, "s");
    }

    use super::*;

    fn page_of(uris: &[(&str, i32)], next: &str, sync: &str) -> PageResponse {
        PageResponse {
            items: uris
                .iter()
                .map(|(u, t)| CollectionItem {
                    uri: (*u).into(),
                    added_at: *t,
                    ..Default::default()
                })
                .collect(),
            next_page_token: next.into(),
            sync_token: sync.into(),
            ..Default::default()
        }
    }

    #[test]
    fn absorb_pages_until_the_last_one_and_keeps_its_token() {
        let mut listing = Listing::default();
        assert_eq!(
            absorb(&mut listing, page_of(&[("a", 1)], "p2", "")),
            Some("p2".into())
        );
        assert!(!listing.complete());
        assert_eq!(absorb(&mut listing, page_of(&[("b", 2)], "", "sync")), None);
        assert!(listing.complete());
        assert_eq!(listing.sync_token.as_deref(), Some("sync"));
        assert_eq!(
            listing.items,
            vec![
                Item {
                    uri: "a".into(),
                    added_at: 1
                },
                Item {
                    uri: "b".into(),
                    added_at: 2
                }
            ]
        );
    }

    #[test]
    fn a_last_page_without_a_token_is_incomplete() {
        let mut listing = Listing::default();
        assert_eq!(absorb(&mut listing, page_of(&[("a", 1)], "", "")), None);
        assert!(!listing.complete());
    }

    #[test]
    fn the_page_cap_stops_paging_incomplete() {
        let mut listing = Listing::default();
        for _ in 0..MAX_PAGES - 1 {
            assert!(absorb(&mut listing, page_of(&[], "more", "")).is_some());
        }
        assert_eq!(absorb(&mut listing, page_of(&[], "more", "")), None);
        assert!(!listing.complete());
        assert_eq!(listing.pages, MAX_PAGES);
    }

    #[test]
    fn delta_splits_added_and_removed() {
        let resp = DeltaResponse {
            delta_update_possible: true,
            items: vec![
                CollectionItem {
                    uri: "a".into(),
                    added_at: 3,
                    ..Default::default()
                },
                CollectionItem {
                    uri: "b".into(),
                    is_removed: true,
                    ..Default::default()
                },
            ],
            sync_token: "s2".into(),
            ..Default::default()
        };
        assert_eq!(
            delta_from(resp),
            Delta::Changes {
                added: vec![Item {
                    uri: "a".into(),
                    added_at: 3
                }],
                removed: vec!["b".into()],
                sync_token: "s2".into(),
            }
        );
    }

    #[test]
    fn an_impossible_delta_is_refused() {
        let resp = DeltaResponse {
            delta_update_possible: false,
            ..Default::default()
        };
        assert_eq!(delta_from(resp), Delta::Refused);
    }

    #[test]
    fn a_rejected_request_is_a_protocol_change() {
        let err = librespot_core::Error::invalid_argument("bad request");
        assert!(matches!(
            classify_collection(&err),
            InternalError::ProtocolChanged(_)
        ));
        let err = librespot_core::Error::resource_exhausted("429");
        assert_eq!(classify_collection(&err), InternalError::RateLimited);
    }

    #[test]
    fn a_delta_with_an_empty_token_is_refused() {
        let resp = DeltaResponse {
            delta_update_possible: true,
            items: vec![CollectionItem {
                uri: "a".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(delta_from(resp), Delta::Refused);
    }

    #[test]
    fn a_rejected_delta_token_is_refused_not_a_protocol_change() {
        let err = librespot_core::Error::invalid_argument("bad request");
        assert_eq!(delta_answer(Err(err)), Ok(Delta::Refused));
        let err = librespot_core::Error::resource_exhausted("429");
        assert_eq!(delta_answer(Err(err)), Err(InternalError::RateLimited));
        let err = librespot_core::Error::failed_precondition("412");
        assert!(matches!(
            delta_answer(Err(err)),
            Err(InternalError::ProtocolChanged(_))
        ));
    }

    #[test]
    fn a_delta_answer_decodes_its_bytes() {
        let resp = DeltaResponse {
            delta_update_possible: true,
            sync_token: "s".into(),
            ..Default::default()
        };
        assert_eq!(
            delta_answer(Ok(resp.write_to_bytes().unwrap())),
            Ok(Delta::Changes {
                added: vec![],
                removed: vec![],
                sync_token: "s".into(),
            })
        );
        assert!(matches!(
            delta_answer(Ok(vec![0xff, 0xff])),
            Err(InternalError::ProtocolChanged(_))
        ));
    }
}
