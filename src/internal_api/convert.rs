//! Conversion from librespot-metadata types (internal API) into the rspotify types the Web API
//! returns, so `WebApi` callers do not change. Only the fields ncspot reads are filled from the
//! internal data; the rest are empty or zero.
#![allow(deprecated)] // rspotify marks several fields Spotify removed as deprecated; they still must be set.

use std::collections::HashMap;

use chrono::Duration as ChronoDuration;
use librespot_core::{FileId, SpotifyUri, date::Date};
use librespot_metadata::image::Images;
use librespot_metadata::restriction::Restrictions;
use rspotify::model::{
    AlbumId, AlbumType, ArtistId, DatePrecision, EpisodeId, FullAlbum, FullArtist, FullEpisode,
    FullShow, FullTrack, Image, Page, ShowId, SimplifiedAlbum, SimplifiedArtist, SimplifiedEpisode,
    SimplifiedShow, SimplifiedTrack, TrackId, Type,
};
use rspotify::prelude::Id;

use super::error::InternalError;

fn changed(why: impl Into<String>) -> InternalError {
    InternalError::ProtocolChanged(why.into())
}

/// An item without a name falls back to the Web API for this call; it does not latch.
fn non_empty(name: &str) -> Result<String, InternalError> {
    if name.is_empty() {
        Err(InternalError::NotFound)
    } else {
        Ok(name.to_string())
    }
}

pub fn image_url(file_id: &FileId) -> String {
    format!("https://i.scdn.co/image/{}", file_id.to_base16())
}

/// All images of `groups`, widest first (ncspot shows `images.first()`).
pub fn images(groups: &[&Images]) -> Vec<Image> {
    let mut all: Vec<Image> = groups
        .iter()
        .flat_map(|group| group.iter())
        .map(|img| Image {
            url: image_url(&img.id),
            width: u32::try_from(img.width).ok().filter(|w| *w > 0),
            height: u32::try_from(img.height).ok().filter(|h| *h > 0),
        })
        .collect();
    all.sort_by_key(|img| std::cmp::Reverse(img.width.unwrap_or(0)));
    all.dedup_by(|a, b| a.url == b.url);
    all
}

pub fn release_date(date: &Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Whether `restrictions` allow playback in `country`.
pub fn is_playable(restrictions: &Restrictions, country: &str) -> bool {
    restrictions.iter().all(|r| {
        let forbidden = r
            .countries_forbidden
            .as_ref()
            .is_some_and(|list| list.iter().any(|c| c == country));
        let not_allowed = r
            .countries_allowed
            .as_ref()
            .is_some_and(|list| !list.iter().any(|c| c == country));
        !forbidden && !not_allowed
    })
}

macro_rules! id_conv {
    ($name:ident, $ty:ident) => {
        pub fn $name(uri: &SpotifyUri) -> Result<$ty<'static>, InternalError> {
            $ty::from_id(uri.to_id())
                .map(|id| id.into_static())
                .map_err(|e| changed(format!("bad id {}: {e}", uri.to_uri())))
        }
    };
}
id_conv!(track_id, TrackId);
id_conv!(album_id, AlbumId);
id_conv!(artist_id, ArtistId);
id_conv!(show_id, ShowId);
id_conv!(episode_id, EpisodeId);

fn spotify_url(kind: &str, id: &str) -> HashMap<String, String> {
    HashMap::from([(
        "spotify".to_string(),
        format!("https://open.spotify.com/{kind}/{id}"),
    )])
}

fn href(kind: &str, id: &str) -> String {
    format!("https://api.spotify.com/v1/{kind}s/{id}")
}

pub fn simplified_artist(
    a: &librespot_metadata::Artist,
) -> Result<SimplifiedArtist, InternalError> {
    let id = artist_id(&a.id)?;
    Ok(SimplifiedArtist {
        external_urls: spotify_url("artist", id.id()),
        href: Some(href("artist", id.id())),
        name: non_empty(&a.name)?,
        id: Some(id),
    })
}

fn album_type_str(a: &librespot_metadata::Album) -> &'static str {
    use librespot_metadata::album::AlbumType as T;
    match a.album_type {
        T::SINGLE | T::EP => "single",
        T::COMPILATION => "compilation",
        _ => "album",
    }
}

pub fn simplified_album(a: &librespot_metadata::Album) -> Result<SimplifiedAlbum, InternalError> {
    let id = album_id(&a.id)?;
    Ok(SimplifiedAlbum {
        album_group: None,
        album_type: Some(album_type_str(a).to_string()),
        artists: a
            .artists
            .iter()
            .map(simplified_artist)
            .collect::<Result<_, _>>()?,
        available_markets: Vec::new(),
        external_urls: spotify_url("album", id.id()),
        href: Some(href("album", id.id())),
        images: images(&[&a.covers, &a.cover_group]),
        name: non_empty(&a.name)?,
        release_date: Some(release_date(&a.date)),
        release_date_precision: Some("day".to_string()),
        restrictions: None,
        id: Some(id),
    })
}

pub fn full_track(
    t: &librespot_metadata::Track,
    country: &str,
) -> Result<FullTrack, InternalError> {
    let id = track_id(&t.id)?;
    Ok(FullTrack {
        album: simplified_album(&t.album)?,
        artists: t
            .artists
            .iter()
            .map(simplified_artist)
            .collect::<Result<_, _>>()?,
        available_markets: Vec::new(),
        disc_number: t.disc_number,
        duration: ChronoDuration::milliseconds(i64::from(t.duration)),
        explicit: t.is_explicit,
        external_ids: HashMap::new(),
        external_urls: spotify_url("track", id.id()),
        href: Some(href("track", id.id())),
        is_local: false,
        is_playable: Some(is_playable(&t.restrictions, country)),
        linked_from: None,
        restrictions: None,
        name: non_empty(&t.name)?,
        popularity: u32::try_from(t.popularity).unwrap_or(0),
        preview_url: None,
        track_number: u32::try_from(t.number).unwrap_or(0),
        r#type: Type::Track,
        id: Some(id),
    })
}

pub fn simplified_track(
    t: &librespot_metadata::Track,
    country: &str,
) -> Result<SimplifiedTrack, InternalError> {
    let full = full_track(t, country)?;
    Ok(SimplifiedTrack {
        album: Some(full.album),
        artists: full.artists,
        available_markets: None,
        disc_number: full.disc_number,
        duration: full.duration,
        explicit: full.explicit,
        external_urls: full.external_urls,
        href: full.href,
        id: full.id,
        is_local: false,
        is_playable: full.is_playable,
        linked_from: None,
        restrictions: None,
        name: full.name,
        preview_url: None,
        track_number: full.track_number,
    })
}

/// A Web API style page of `items`, which start at `offset` of `total`.
pub fn page<T: serde::de::DeserializeOwned>(
    items: Vec<T>,
    total: usize,
    offset: usize,
    limit: usize,
    href: &str,
) -> Page<T> {
    let next_offset = offset + items.len();
    let next = (next_offset < total).then(|| format!("{href}?offset={next_offset}&limit={limit}"));
    Page {
        href: format!("{href}?offset={offset}&limit={limit}"),
        items,
        limit: limit as u32,
        next,
        offset: offset as u32,
        previous: None,
        total: total as u32,
    }
}

pub fn full_album(
    a: &librespot_metadata::Album,
    first_tracks: Vec<SimplifiedTrack>,
    total_tracks: usize,
) -> Result<FullAlbum, InternalError> {
    let simple = simplified_album(a)?;
    let id = simple
        .id
        .clone()
        .ok_or_else(|| changed("album without id"))?;
    let album_type = match album_type_str(a) {
        "single" => AlbumType::Single,
        "compilation" => AlbumType::Compilation,
        _ => AlbumType::Album,
    };
    let tracks_href = format!("{}/tracks", href("album", id.id()));
    let limit = first_tracks.len().max(1);
    Ok(FullAlbum {
        artists: simple.artists,
        album_type,
        available_markets: None,
        copyrights: Vec::new(),
        external_ids: HashMap::new(),
        external_urls: simple.external_urls,
        genres: Vec::new(),
        href: href("album", id.id()),
        images: simple.images,
        name: simple.name,
        popularity: u32::try_from(a.popularity).unwrap_or(0),
        release_date: release_date(&a.date),
        release_date_precision: DatePrecision::Day,
        tracks: page(first_tracks, total_tracks, 0, limit, &tracks_href),
        label: Some(a.label.clone()).filter(|l| !l.is_empty()),
        id,
    })
}

pub fn full_artist(a: &librespot_metadata::Artist) -> Result<FullArtist, InternalError> {
    let id = artist_id(&a.id)?;
    Ok(FullArtist {
        external_urls: spotify_url("artist", id.id()),
        followers: Default::default(),
        genres: Vec::new(),
        href: href("artist", id.id()),
        images: images(&[&a.portraits, &a.portrait_group]),
        name: non_empty(&a.name)?,
        popularity: u32::try_from(a.popularity).unwrap_or(0),
        id,
    })
}

pub fn simplified_show(s: &librespot_metadata::Show) -> Result<SimplifiedShow, InternalError> {
    let id = show_id(&s.id)?;
    Ok(SimplifiedShow {
        available_markets: Vec::new(),
        copyrights: Vec::new(),
        description: s.description.clone(),
        explicit: s.is_explicit,
        external_urls: spotify_url("show", id.id()),
        href: href("show", id.id()),
        images: images(&[&s.covers]),
        is_externally_hosted: None,
        languages: vec![s.language.clone()]
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect(),
        media_type: "audio".to_string(),
        name: non_empty(&s.name)?,
        publisher: s.publisher.clone(),
        id,
    })
}

pub fn full_show(
    s: &librespot_metadata::Show,
    first_episodes: Vec<SimplifiedEpisode>,
    total: usize,
) -> Result<FullShow, InternalError> {
    let simple = simplified_show(s)?;
    let episodes_href = format!("{}/episodes", simple.href);
    let limit = first_episodes.len().max(1);
    Ok(FullShow {
        available_markets: Vec::new(),
        copyrights: Vec::new(),
        description: simple.description,
        explicit: simple.explicit,
        episodes: page(first_episodes, total, 0, limit, &episodes_href),
        external_urls: simple.external_urls,
        href: simple.href,
        id: simple.id,
        images: simple.images,
        is_externally_hosted: None,
        languages: simple.languages,
        media_type: simple.media_type,
        name: simple.name,
        publisher: simple.publisher,
    })
}

pub fn simplified_episode(
    e: &librespot_metadata::Episode,
) -> Result<SimplifiedEpisode, InternalError> {
    let id = episode_id(&e.id)?;
    Ok(SimplifiedEpisode {
        audio_preview_url: None,
        description: e.description.clone(),
        duration: ChronoDuration::milliseconds(i64::from(e.duration)),
        explicit: e.is_explicit,
        external_urls: spotify_url("episode", id.id()),
        href: href("episode", id.id()),
        images: images(&[&e.covers]),
        is_externally_hosted: false,
        is_playable: true,
        language: e.language.clone(),
        languages: vec![e.language.clone()]
            .into_iter()
            .filter(|l| !l.is_empty())
            .collect(),
        name: non_empty(&e.name)?,
        release_date: release_date(&e.publish_time),
        release_date_precision: DatePrecision::Day,
        resume_point: None,
        id,
    })
}

pub fn full_episode(
    e: &librespot_metadata::Episode,
    show: SimplifiedShow,
) -> Result<FullEpisode, InternalError> {
    let s = simplified_episode(e)?;
    Ok(FullEpisode {
        audio_preview_url: None,
        description: s.description,
        duration: s.duration,
        explicit: s.explicit,
        external_urls: s.external_urls,
        href: s.href,
        id: s.id,
        images: s.images,
        is_externally_hosted: false,
        is_playable: true,
        language: s.language,
        languages: s.languages,
        name: s.name,
        release_date: s.release_date,
        release_date_precision: DatePrecision::Day,
        resume_point: None,
        show,
        r#type: Type::Episode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_metadata::{Album, Artist, Track};
    use librespot_protocol::metadata as proto;
    use protobuf::MessageField;

    const GID: [u8; 16] = [1; 16];

    fn image(file: u8, width: i32) -> proto::Image {
        proto::Image {
            file_id: Some(vec![file; 20]),
            width: Some(width),
            height: Some(width),
            ..Default::default()
        }
    }

    fn artist_msg(name: &str) -> proto::Artist {
        proto::Artist {
            gid: Some(GID.to_vec()),
            name: Some(name.into()),
            ..Default::default()
        }
    }

    fn album_msg() -> proto::Album {
        proto::Album {
            gid: Some(GID.to_vec()),
            name: Some("Album".into()),
            artist: vec![artist_msg("Artist")],
            date: MessageField::some(proto::Date {
                year: Some(2001),
                month: Some(2),
                day: Some(3),
                ..Default::default()
            }),
            cover_group: MessageField::some(proto::ImageGroup {
                image: vec![image(1, 64), image(2, 640)],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn track_msg() -> proto::Track {
        proto::Track {
            gid: Some(GID.to_vec()),
            name: Some("Song".into()),
            album: MessageField::some(album_msg()),
            artist: vec![artist_msg("Artist")],
            number: Some(4),
            disc_number: Some(1),
            duration: Some(123_456),
            explicit: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn image_url_uses_base16_file_id() {
        let album = Album::try_from(&album_msg()).unwrap();
        let first = &album.cover_group.0[0];
        assert_eq!(
            image_url(&first.id),
            format!("https://i.scdn.co/image/{}", "01".repeat(20))
        );
    }

    #[test]
    fn images_are_widest_first() {
        let album = Album::try_from(&album_msg()).unwrap();
        let imgs = images(&[&album.covers, &album.cover_group]);
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[0].width, Some(640));
        assert_eq!(imgs[1].width, Some(64));
    }

    #[test]
    fn no_images_is_empty() {
        let album = Album::try_from(&proto::Album {
            gid: Some(GID.to_vec()),
            name: Some("A".into()),
            ..Default::default()
        })
        .unwrap();
        assert!(images(&[&album.covers, &album.cover_group]).is_empty());
    }

    #[test]
    fn release_date_is_iso_day() {
        let album = Album::try_from(&album_msg()).unwrap();
        assert_eq!(release_date(&album.date), "2001-02-03");
    }

    #[test]
    fn playable_follows_country_restrictions() {
        let restricted = |allowed: Option<&str>, forbidden: Option<&str>| {
            Track::try_from(&proto::Track {
                restriction: vec![proto::Restriction {
                    country_restriction: allowed
                        .map(|c| {
                            proto::restriction::Country_restriction::CountriesAllowed(c.into())
                        })
                        .or(forbidden.map(|c| {
                            proto::restriction::Country_restriction::CountriesForbidden(c.into())
                        })),
                    ..Default::default()
                }],
                ..track_msg()
            })
            .unwrap()
            .restrictions
        };
        let free = Track::try_from(&track_msg()).unwrap().restrictions;
        assert!(is_playable(&free, "US"));
        assert!(!is_playable(&restricted(None, Some("USGB")), "US"));
        assert!(is_playable(&restricted(None, Some("GB")), "US"));
        assert!(!is_playable(&restricted(Some("GB"), None), "US"));
        assert!(is_playable(&restricted(Some("GBUS"), None), "US"));
    }

    #[test]
    fn full_track_copies_the_fields_ncspot_reads() {
        let track = Track::try_from(&track_msg()).unwrap();
        let ft = full_track(&track, "US").unwrap();
        assert_eq!(ft.name, "Song");
        assert_eq!(ft.track_number, 4);
        assert_eq!(ft.disc_number, 1);
        assert_eq!(ft.duration.num_milliseconds(), 123_456);
        assert!(ft.explicit);
        assert_eq!(ft.is_playable, Some(true));
        assert_eq!(ft.album.name, "Album");
        assert_eq!(ft.artists[0].name, "Artist");
        assert_eq!(ft.id.as_ref().unwrap().id(), track.id.to_id());
        assert_eq!(ft.album.images[0].width, Some(640));
    }

    #[test]
    fn empty_name_falls_back_without_latching() {
        let track = Track::try_from(&proto::Track {
            name: Some(String::new()),
            ..track_msg()
        })
        .unwrap();
        assert_eq!(
            full_track(&track, "US").unwrap_err(),
            InternalError::NotFound
        );
    }

    #[test]
    fn page_sets_next_only_when_more_remain() {
        let p = page(
            vec![1, 2],
            5,
            0,
            2,
            "https://api.spotify.com/v1/albums/x/tracks",
        );
        assert_eq!((p.total, p.offset, p.limit), (5, 0, 2));
        assert!(p.next.is_some());
        let last = page(vec![5], 5, 4, 2, "h");
        assert!(last.next.is_none());
    }

    #[test]
    fn full_artist_and_album_convert() {
        let artist = Artist::try_from(&artist_msg("Artist")).unwrap();
        let fa = full_artist(&artist).unwrap();
        assert_eq!(fa.name, "Artist");
        let album = Album::try_from(&album_msg()).unwrap();
        let full = full_album(&album, Vec::new(), 12).unwrap();
        assert_eq!(full.tracks.total, 12);
        assert_eq!(full.release_date, "2001-02-03");
    }

    fn date_msg() -> proto::Date {
        proto::Date {
            year: Some(2020),
            month: Some(5),
            day: Some(6),
            ..Default::default()
        }
    }

    fn show_msg() -> proto::Show {
        proto::Show {
            gid: Some(GID.to_vec()),
            name: Some("Show".into()),
            description: Some("D".into()),
            cover_image: MessageField::some(proto::ImageGroup {
                image: vec![image(3, 300)],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn episode_msg() -> proto::Episode {
        proto::Episode {
            gid: Some(GID.to_vec()),
            name: Some("Ep".into()),
            description: Some("ED".into()),
            duration: Some(60_000),
            publish_time: MessageField::some(date_msg()),
            ..Default::default()
        }
    }

    #[test]
    fn show_converts() {
        let show = librespot_metadata::Show::try_from(&show_msg()).unwrap();
        let simple = simplified_show(&show).unwrap();
        assert_eq!(simple.name, "Show");
        assert_eq!(simple.description, "D");
        assert_eq!(simple.images[0].width, Some(300));
        let ep = librespot_metadata::Episode::try_from(&episode_msg()).unwrap();
        let eps = vec![simplified_episode(&ep).unwrap()];
        let full = full_show(&show, eps, 9).unwrap();
        assert_eq!(full.name, "Show");
        assert_eq!(full.description, "D");
        assert_eq!(full.episodes.total, 9);
        assert_eq!(full.episodes.items.len(), 1);
    }

    #[test]
    fn episode_converts() {
        let ep = librespot_metadata::Episode::try_from(&episode_msg()).unwrap();
        let simple = simplified_episode(&ep).unwrap();
        assert_eq!(simple.name, "Ep");
        assert_eq!(simple.description, "ED");
        assert_eq!(simple.duration.num_milliseconds(), 60_000);
        assert_eq!(simple.release_date, "2020-05-06");
        let show =
            simplified_show(&librespot_metadata::Show::try_from(&show_msg()).unwrap()).unwrap();
        let full = full_episode(&ep, show).unwrap();
        assert_eq!(full.name, "Ep");
        assert_eq!(full.duration.num_milliseconds(), 60_000);
        assert_eq!(full.release_date, "2020-05-06");
        assert_eq!(full.show.name, "Show");
    }
}
