//! Metadata from the internal API: single items through librespot-metadata, lists through
//! batched extended-metadata requests (one request per 100 ids).

use std::collections::HashMap;

use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_metadata::{Album, Artist, Episode, Metadata, Show, Track};
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use librespot_protocol::extension_kind::ExtensionKind;
use log::debug;
use protobuf::{EnumOrUnknown, Message};
use rspotify::model::{
    AlbumType, FullAlbum, FullArtist, FullEpisode, FullShow, FullTrack, Page, PrivateUser,
    SimplifiedAlbum, SimplifiedEpisode, SimplifiedTrack, SubscriptionLevel, UserId,
};

use super::convert;
use super::error::{InternalError, classify};

/// Most ids in one extended-metadata request.
pub const BATCH_SIZE: usize = 100;
/// Items in the first page embedded in `FullAlbum` and `FullShow`, as the Web API does.
pub const FIRST_PAGE: usize = 50;

/// An internal metadata type and its extended-metadata kind.
pub trait Kind: Metadata {
    const KIND: ExtensionKind;
    const NAME: &'static str;
}
impl Kind for Track {
    const KIND: ExtensionKind = ExtensionKind::TRACK_V4;
    const NAME: &'static str = "track";
}
impl Kind for Album {
    const KIND: ExtensionKind = ExtensionKind::ALBUM_V4;
    const NAME: &'static str = "album";
}
impl Kind for Artist {
    const KIND: ExtensionKind = ExtensionKind::ARTIST_V4;
    const NAME: &'static str = "artist";
}
impl Kind for Show {
    const KIND: ExtensionKind = ExtensionKind::SHOW_V4;
    const NAME: &'static str = "show";
}
impl Kind for Episode {
    const KIND: ExtensionKind = ExtensionKind::EPISODE_V4;
    const NAME: &'static str = "episode";
}

fn uri(kind: &str, id: &str) -> Result<SpotifyUri, InternalError> {
    SpotifyUri::from_uri(&format!("spotify:{kind}:{id}")).map_err(|_| InternalError::NotFound)
}

async fn get_one<T: Kind>(session: &Session, uri: &SpotifyUri) -> Result<T, InternalError> {
    debug!("internal metadata: {} {}", T::NAME, uri.to_id());
    T::get(session, uri).await.map_err(|e| classify(&e))
}

/// What `get_many` does with an entity the response lacks or marks with an error status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// Fail the call with `NotFound`.
    Fail,
    /// Drop the entity (the result is shorter than the request).
    Skip,
}

/// The payloads of `chunk` in request order. `found` maps a uri to its status code and payload
/// (`None` when the response carried no data). A uri may appear twice in `chunk`.
fn pick_in_order<'a>(
    chunk: &'a [SpotifyUri],
    found: &'a HashMap<String, (i32, Option<Vec<u8>>)>,
    missing: Missing,
) -> Result<Vec<(&'a SpotifyUri, &'a [u8])>, InternalError> {
    let mut out = Vec::with_capacity(chunk.len());
    for u in chunk {
        // get, not remove: a uri may appear twice in one request.
        match found.get(&u.to_uri()) {
            Some((status, Some(bytes))) if *status == 0 || *status == 200 => {
                out.push((u, bytes.as_slice()));
            }
            other => {
                let status = other.map(|(s, _)| *s);
                debug!(
                    "internal metadata: {} missing, status {status:?}",
                    u.to_uri()
                );
                if missing == Missing::Fail {
                    return Err(InternalError::NotFound);
                }
            }
        }
    }
    Ok(out)
}

/// The items of `uris`, in request order. With `Missing::Fail` any missing item fails the call
/// with `NotFound`, so a page is never shorter than its window; with `Missing::Skip` it is
/// dropped.
async fn get_many<T: Kind>(
    session: &Session,
    uris: &[SpotifyUri],
    missing: Missing,
) -> Result<Vec<T>, InternalError> {
    let mut out = Vec::with_capacity(uris.len());
    for chunk in uris.chunks(BATCH_SIZE) {
        debug!("internal metadata: {} {}s", chunk.len(), T::NAME);
        let request = BatchedEntityRequest {
            entity_request: chunk
                .iter()
                .map(|u| EntityRequest {
                    entity_uri: u.to_uri(),
                    query: vec![ExtensionQuery {
                        extension_kind: EnumOrUnknown::new(T::KIND),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let response = session
            .spclient()
            .get_extended_metadata(request)
            .await
            .map_err(|e| classify(&e))?;
        let mut found: HashMap<String, (i32, Option<Vec<u8>>)> = HashMap::new();
        for array in response.extended_metadata {
            for data in array.extension_data {
                let status = data.header.status_code;
                let bytes = data.extension_data.into_option().map(|any| any.value);
                found.insert(data.entity_uri, (status, bytes));
            }
        }
        for (u, bytes) in pick_in_order(chunk, &found, missing)? {
            let msg = <T as Metadata>::Message::parse_from_bytes(bytes)
                .map_err(|e| InternalError::ProtocolChanged(format!("{} parse: {e}", T::NAME)))?;
            out.push(T::parse(&msg, u).map_err(|e| classify(&e))?);
        }
    }
    Ok(out)
}

/// The user's catalogue as librespot reads it, "premium" when the session has none.
fn catalogue(session: &Session) -> String {
    session
        .get_user_attribute("catalogue")
        .unwrap_or_else(|| "premium".to_string())
}

/// The items of `items` from `offset`, at most `limit` of them; empty past the end.
pub fn window<T: Clone>(items: &[T], offset: usize, limit: usize) -> Vec<T> {
    let start = offset.min(items.len());
    let end = start.saturating_add(limit).min(items.len());
    items[start..end].to_vec()
}

pub async fn track(session: Session, id: String) -> Result<FullTrack, InternalError> {
    let t: Track = get_one(&session, &uri("track", &id)?).await?;
    convert::full_track(&t, &session.country(), &catalogue(&session))
}

async fn simplified_tracks(
    session: &Session,
    uris: &[SpotifyUri],
) -> Result<Vec<SimplifiedTrack>, InternalError> {
    let country = session.country();
    let catalogue = catalogue(session);
    // Fail: Album::load_all_tracks pages by offset + items.len(), so a short page would
    // duplicate or loop.
    get_many::<Track>(session, uris, Missing::Fail)
        .await?
        .iter()
        .map(|t| convert::simplified_track(t, &country, &catalogue))
        .collect()
}

pub async fn album(session: Session, id: String) -> Result<FullAlbum, InternalError> {
    let a: Album = get_one(&session, &uri("album", &id)?).await?;
    let ids: Vec<SpotifyUri> = a.tracks().cloned().collect();
    let first = simplified_tracks(&session, &window(&ids, 0, FIRST_PAGE)).await?;
    convert::full_album(&a, first, ids.len())
}

pub async fn album_tracks(
    session: Session,
    id: String,
    limit: u32,
    offset: u32,
) -> Result<Page<SimplifiedTrack>, InternalError> {
    let a: Album = get_one(&session, &uri("album", &id)?).await?;
    let ids: Vec<SpotifyUri> = a.tracks().cloned().collect();
    let (offset, limit) = (offset as usize, limit as usize);
    let items = simplified_tracks(&session, &window(&ids, offset, limit)).await?;
    let href = format!("https://api.spotify.com/v1/albums/{id}/tracks");
    Ok(convert::page(items, ids.len(), offset, limit, &href))
}

pub async fn artist(session: Session, id: String) -> Result<FullArtist, InternalError> {
    let a: Artist = get_one(&session, &uri("artist", &id)?).await?;
    convert::full_artist(&a)
}

/// The release ids of `artist` in the groups `album_type` selects (all four when `None`), one
/// id per release: the first id of each group.
pub fn group_ids(artist: &Artist, album_type: Option<AlbumType>) -> Vec<SpotifyUri> {
    let groups = match album_type {
        Some(AlbumType::Album) => vec![&artist.albums],
        Some(AlbumType::Single) => vec![&artist.singles],
        Some(AlbumType::Compilation) => vec![&artist.compilations],
        Some(AlbumType::AppearsOn) => vec![&artist.appears_on_albums],
        None => vec![
            &artist.albums,
            &artist.singles,
            &artist.compilations,
            &artist.appears_on_albums,
        ],
    };
    groups
        .into_iter()
        .flat_map(|g| g.iter())
        .filter_map(|group| group.first().cloned())
        .collect()
}

pub async fn artist_album_ids(
    session: Session,
    id: String,
    album_type: Option<AlbumType>,
) -> Result<Vec<SpotifyUri>, InternalError> {
    let a: Artist = get_one(&session, &uri("artist", &id)?).await?;
    Ok(group_ids(&a, album_type))
}

pub async fn albums_by_uri(
    session: Session,
    uris: Vec<SpotifyUri>,
) -> Result<Vec<SimplifiedAlbum>, InternalError> {
    // Skip: callers advance by the requested limit, so a short page is fine.
    get_many::<Album>(&session, &uris, Missing::Skip)
        .await?
        .iter()
        .map(convert::simplified_album)
        .collect()
}

pub async fn artist_top_tracks(
    session: Session,
    id: String,
) -> Result<Vec<FullTrack>, InternalError> {
    let a: Artist = get_one(&session, &uri("artist", &id)?).await?;
    let country = session.country();
    let catalogue = catalogue(&session);
    let ids: Vec<SpotifyUri> = a.top_tracks.for_country(&country).0;
    // Skip: top tracks are not paged, so a dropped track is harmless.
    get_many::<Track>(&session, &ids, Missing::Skip)
        .await?
        .iter()
        .map(|t| convert::full_track(t, &country, &catalogue))
        .collect()
}

pub async fn show(session: Session, id: String) -> Result<FullShow, InternalError> {
    let s: Show = get_one(&session, &uri("show", &id)?).await?;
    let ids: Vec<SpotifyUri> = s.episodes.0.clone();
    let first = episodes_by_uri(session.clone(), window(&ids, 0, FIRST_PAGE)).await?;
    convert::full_show(&s, first, ids.len())
}

pub async fn show_episode_ids(
    session: Session,
    id: String,
) -> Result<Vec<SpotifyUri>, InternalError> {
    let s: Show = get_one(&session, &uri("show", &id)?).await?;
    Ok(s.episodes.0.clone())
}

pub async fn episodes_by_uri(
    session: Session,
    uris: Vec<SpotifyUri>,
) -> Result<Vec<SimplifiedEpisode>, InternalError> {
    // Skip: callers advance by the requested limit, so a short page is fine (and one pulled
    // episode must not hide a whole show).
    get_many::<Episode>(&session, &uris, Missing::Skip)
        .await?
        .iter()
        .map(convert::simplified_episode)
        .collect()
}

/// The show of an episode message, if it names one.
pub fn show_uri_of(msg: &librespot_protocol::metadata::Episode) -> Option<SpotifyUri> {
    let gid = msg.show.as_ref()?.gid.as_ref()?;
    SpotifyId::from_raw(gid)
        .ok()
        .map(|id| SpotifyUri::Show { id })
}

pub async fn episode(session: Session, id: String) -> Result<FullEpisode, InternalError> {
    let u = uri("episode", &id)?;
    debug!("internal metadata: episode {id}");
    // librespot-metadata's Episode drops the show id, so parse the message here once.
    let bytes = <Episode as Metadata>::request(&session, &u)
        .await
        .map_err(|e| classify(&e))?;
    let msg = librespot_protocol::metadata::Episode::parse_from_bytes(&bytes)
        .map_err(|e| InternalError::ProtocolChanged(format!("episode parse: {e}")))?;
    let e = Episode::parse(&msg, &u).map_err(|e| classify(&e))?;
    let show_uri = show_uri_of(&msg).ok_or(InternalError::NotFound)?;
    let s: Show = get_one(&session, &show_uri).await?;
    convert::full_episode(&e, convert::simplified_show(&s)?)
}

/// The current user as the Web API describes it, from the session and the profile JSON
/// (`Null` or a profile without `name` falls back to the username).
// rspotify marks several fields Spotify removed as deprecated; they still must be set.
#[allow(deprecated)]
pub fn user_from_profile(
    username: &str,
    country: &str,
    profile: &serde_json::Value,
) -> PrivateUser {
    PrivateUser {
        country: serde_json::from_value(serde_json::Value::String(country.to_string())).ok(),
        display_name: Some(profile["name"].as_str().unwrap_or(username).to_string()),
        email: None,
        external_urls: HashMap::from([(
            "spotify".to_string(),
            format!("https://open.spotify.com/user/{username}"),
        )]),
        explicit_content: None,
        followers: None,
        href: format!("https://api.spotify.com/v1/users/{username}"),
        id: UserId::from_id(username.to_string()).expect("any username is a valid user id"),
        images: None,
        product: Some(SubscriptionLevel::Premium),
    }
}

pub async fn current_user(session: Session) -> Result<PrivateUser, InternalError> {
    let username = session.username();
    debug!("internal metadata: user {username}");
    let profile = session
        .spclient()
        .get_user_profile(&username, Some(0), Some(0))
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(serde_json::Value::Null);
    Ok(user_from_profile(&username, &session.country(), &profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rspotify::model::AlbumType;
    use rspotify::prelude::Id;

    fn uris(ids: &[&str]) -> Vec<SpotifyUri> {
        ids.iter()
            .map(|i| SpotifyUri::from_uri(&format!("spotify:track:{i}")).unwrap())
            .collect()
    }

    fn found(entries: &[(&str, i32, Option<&[u8]>)]) -> HashMap<String, (i32, Option<Vec<u8>>)> {
        entries
            .iter()
            .map(|(i, st, b)| (format!("spotify:track:{i}"), (*st, b.map(<[u8]>::to_vec))))
            .collect()
    }

    const A: &str = "4uLU6hMCjMI75M1A2tKUQC";
    const B: &str = "0OdUWJ0sBjDrqHygGUXeCF";
    const C: &str = "4rOoJ6Egrf8K2IrywzwOMk";

    #[test]
    fn pick_keeps_request_order_and_duplicates() {
        let chunk = uris(&[B, A, B]);
        let f = found(&[(A, 200, Some(b"a")), (B, 0, Some(b"b"))]);
        for m in [Missing::Fail, Missing::Skip] {
            let got: Vec<&[u8]> = pick_in_order(&chunk, &f, m)
                .unwrap()
                .into_iter()
                .map(|(_, b)| b)
                .collect();
            assert_eq!(got, vec![&b"b"[..], b"a", b"b"]);
        }
    }

    #[test]
    fn pick_missing_entity() {
        let chunk = uris(&[A, C, B]);
        let f = found(&[(A, 200, Some(b"a")), (B, 200, Some(b"b"))]);
        assert_eq!(
            pick_in_order(&chunk, &f, Missing::Fail).unwrap_err(),
            InternalError::NotFound
        );
        let got = pick_in_order(&chunk, &f, Missing::Skip).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].1, b"b");
    }

    #[test]
    fn pick_bad_status_or_no_data() {
        let chunk = uris(&[A, B, C]);
        let f = found(&[(A, 451, None), (B, 200, None), (C, 200, Some(b"c"))]);
        assert_eq!(
            pick_in_order(&chunk, &f, Missing::Fail).unwrap_err(),
            InternalError::NotFound
        );
        let got = pick_in_order(&chunk, &f, Missing::Skip).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, b"c");
    }

    #[test]
    fn window_clamps() {
        let v = vec![1, 2, 3, 4, 5];
        assert_eq!(window(&v, 0, 2), vec![1, 2]);
        assert_eq!(window(&v, 4, 2), vec![5]);
        assert!(window(&v, 9, 2).is_empty());
    }

    #[test]
    #[allow(deprecated)]
    fn user_from_profile_falls_back_to_the_username() {
        let u = user_from_profile("jay", "NL", &serde_json::Value::Null);
        assert_eq!(u.id.id(), "jay");
        assert_eq!(u.display_name.as_deref(), Some("jay"));
        assert_eq!(u.country, Some(rspotify::model::Country::Netherlands));
        let named = user_from_profile("jay", "NL", &serde_json::json!({ "name": "Jay" }));
        assert_eq!(named.display_name.as_deref(), Some("Jay"));
    }

    fn artist_with_groups() -> librespot_metadata::Artist {
        use librespot_protocol::metadata as proto;
        let group = |b: u8| proto::AlbumGroup {
            album: vec![
                proto::Album {
                    gid: Some(vec![b; 16]),
                    ..Default::default()
                },
                proto::Album {
                    gid: Some(vec![b + 100; 16]),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        librespot_metadata::Artist::try_from(&proto::Artist {
            gid: Some(vec![9; 16]),
            name: Some("A".into()),
            album_group: vec![group(1)],
            single_group: vec![group(2)],
            compilation_group: vec![group(3)],
            appears_on_group: vec![group(4)],
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn group_ids_pick_the_first_release_of_each_group() {
        let artist = artist_with_groups();
        let singles = group_ids(&artist, Some(AlbumType::Single));
        assert_eq!(singles.len(), 1);
        assert_eq!(singles[0], artist.singles.0[0].0.0[0]);
        assert_eq!(group_ids(&artist, None).len(), 4);
        assert_eq!(group_ids(&artist, None)[0], artist.albums.0[0].0.0[0]);
    }

    #[test]
    fn show_uri_of_reads_the_show_gid() {
        use librespot_protocol::metadata as proto;
        let msg = proto::Episode {
            show: protobuf::MessageField::some(proto::Show {
                gid: Some(vec![7; 16]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(show_uri_of(&msg), Some(SpotifyUri::Show { .. })));
        assert!(show_uri_of(&proto::Episode::default()).is_none());
    }
}
