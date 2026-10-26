//! Drive harness (feature `harness`): IPC commands that open a view by URI and write a JSON
//! snapshot of the current view, so a script can drive ncspot and check what it shows.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::activity::ACTIVITY;
use crate::internal_api::{Area, AreaCounts};
use crate::library::Library;
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::episode::Episode;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::track::Track;
use crate::queue::Queue;
use crate::rate_limit::{ClientLabel, Stamp};
use crate::spotify::{PlayerEvent, Spotify, UriType};
use crate::spotify_api::WebCalls;
use crate::spotify_url::SpotifyUrl;
use crate::traits::{IntoBoxedViewExt, ListItem, ViewExt};
use crate::ui::{album::AlbumView, artist::ArtistView, playlist::PlaylistView, show::ShowView};

/// One list in the current view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TabState {
    pub name: String,
    /// Rows loaded so far.
    pub rows: usize,
    /// Rows that can be played.
    pub playable: usize,
    /// Total rows the source reports, `None` when the list has no pagination (fully loaded).
    pub max: Option<usize>,
    /// Whether a page is being fetched.
    pub busy: bool,
}

/// What a view reports to the harness.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HarnessState {
    pub view: String,
    /// Name of the selected tab, for tabbed views.
    pub tab: Option<String>,
    pub tabs: Vec<TabState>,
}

/// Internal API counters per area.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Areas {
    pub metadata: AreaCounts,
    pub library: AreaCounts,
}

/// Which clients are inside a rate-limit window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Throttled {
    pub shared: bool,
    pub personal: bool,
    pub internal: bool,
}

/// The snapshot `harness dump` writes. Field order is the JSON order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Dump {
    /// Increases on every dump, so a reader can tell a fresh file from a stale one.
    pub seq: u64,
    pub view: Option<String>,
    pub title: String,
    pub tab: Option<String>,
    pub tabs: Vec<TabState>,
    pub in_flight: usize,
    /// Milliseconds since the last call ended; `None` before the first one.
    pub quiet_ms: Option<u64>,
    /// The last command's error, as shown in the status bar.
    pub error: Option<String>,
    pub areas: Areas,
    pub web: WebCalls,
    pub throttled: Throttled,
    /// Whether the first library sync finished.
    pub library_done: bool,
    pub player: PlayerState,
    pub session: SessionState,
}

/// What the player is doing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlayerState {
    /// One of `playing`, `paused`, `stopped`, `finished`.
    pub status: String,
    pub position_ms: u64,
}

/// The playback session's health.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionState {
    pub valid: bool,
    pub reconnects: u64,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Snapshot the view state plus the counters of the web API, the player and the session.
pub fn build_dump(
    title: String,
    state: Option<HarnessState>,
    error: Option<String>,
    spotify: &Spotify,
    library_done: bool,
) -> Dump {
    let api = &spotify.api;
    let limited = api.rate_limit_status();
    let throttled = |label: ClientLabel| limited.iter().any(|(l, _)| *l == label);
    let (view, tab, tabs) = match state {
        Some(state) => (Some(state.view), state.tab, state.tabs),
        None => (None, None, Vec::new()),
    };
    Dump {
        seq: SEQ.fetch_add(1, Ordering::Relaxed) + 1,
        view,
        title,
        tab,
        tabs,
        in_flight: ACTIVITY.in_flight(),
        quiet_ms: ACTIVITY
            .quiet_for(Stamp::now())
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        error,
        areas: Areas {
            metadata: api.area_counts(Area::Metadata),
            library: api.area_counts(Area::Library),
        },
        web: api.web_calls(),
        throttled: Throttled {
            shared: throttled(ClientLabel::Shared),
            personal: throttled(ClientLabel::Personal),
            internal: throttled(ClientLabel::Internal),
        },
        library_done,
        player: PlayerState {
            status: match spotify.get_current_status() {
                PlayerEvent::Playing(_) => "playing",
                PlayerEvent::Paused(_) => "paused",
                PlayerEvent::Stopped => "stopped",
                PlayerEvent::FinishedTrack => "finished",
            }
            .into(),
            position_ms: u64::try_from(spotify.get_current_progress().as_millis())
                .unwrap_or(u64::MAX),
        },
        session: SessionState {
            valid: spotify.session_valid(),
            reconnects: spotify.reconnects(),
        },
    }
}

/// Write `dump` to `path` atomically: a reader sees the old file or the new one, never half.
pub fn write_dump(path: &Path, dump: &Dump) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let json = serde_json::to_vec(dump).map_err(std::io::Error::other)?;
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Parse the target of `harness open`.
pub fn parse_target(target: &str) -> Result<SpotifyUrl, String> {
    SpotifyUrl::parse_any(target)
        .ok_or_else(|| format!("harness open: not a Spotify URI or URL: {target}"))
}

/// Build the view for `target` the way `goto` does, on the calling (UI) thread: an album,
/// artist, show or playlist opens its own view, a track its album, an episode its show.
pub fn open_view(
    queue: Arc<Queue>,
    library: Arc<Library>,
    target: &str,
) -> Result<Box<dyn ViewExt>, String> {
    let url = parse_target(target)?;
    let spotify = queue.get_spotify();
    let api = &spotify.api;
    let failed = |what: &str| format!("harness open: could not load {what} {}", url.id);
    let view = match url.uri_type {
        UriType::Album => {
            let album = api.album(&url.id).map_err(|_| failed("album"))?;
            AlbumView::new(queue, library, &Album::from(&album)).into_boxed_view_ext()
        }
        UriType::Track => {
            let track = api.track(&url.id).map_err(|_| failed("track"))?;
            let album = Track::from(&track)
                .album(&queue)
                .ok_or_else(|| failed("album of track"))?;
            AlbumView::new(queue, library, &album).into_boxed_view_ext()
        }
        UriType::Artist => {
            let artist = api.artist(&url.id).map_err(|_| failed("artist"))?;
            ArtistView::new(queue, library, &Artist::from(&artist)).into_boxed_view_ext()
        }
        UriType::Show => {
            let show = api.show(&url.id).map_err(|_| failed("show"))?;
            ShowView::new(queue, library, &Show::from(&show)).into_boxed_view_ext()
        }
        UriType::Episode => {
            let episode = api.episode(&url.id).map_err(|_| failed("episode"))?;
            ShowView::new(queue, library, &Show::from(&episode.show)).into_boxed_view_ext()
        }
        UriType::Playlist => {
            let playlist = api.playlist(&url.id).map_err(|_| failed("playlist"))?;
            PlaylistView::new(queue, library, &Playlist::from(&playlist)).into_boxed_view_ext()
        }
    };
    Ok(view)
}

/// A library membership check and its write, for one item.
type ItemOps<'a> = (Box<dyn Fn() -> bool + 'a>, Box<dyn Fn() + 'a>);

/// Save (or remove, with `removed`) one item through the library, as the UI would. Refuses to
/// save an item already in the library, so a test that saves then removes never removes
/// something the user saved, and refuses to remove an item that is not in it. Fails when the
/// library did not change (the write failed). Errors carry no URI or ID.
pub fn write_item(
    queue: Arc<Queue>,
    library: Arc<Library>,
    target: &str,
    removed: bool,
) -> Result<(), String> {
    let url = parse_target(target)?;
    let spotify = queue.get_spotify();
    let api = &spotify.api;
    let verb = if removed {
        "harness unsave"
    } else {
        "harness save"
    };
    let failed = |what: &str| format!("{verb}: could not load {what}");
    let lib = &library;
    let (is_saved, write): ItemOps = match url.uri_type {
        UriType::Track => {
            let track = Track::from(&api.track(&url.id).map_err(|_| failed("track"))?);
            let playable = Playable::Track(track.clone());
            (
                Box::new(move || lib.is_saved_track(&playable)),
                Box::new(move || {
                    if removed {
                        lib.unsave_tracks(&[&track]);
                    } else {
                        lib.save_tracks(&[&track]);
                    }
                }),
            )
        }
        UriType::Album => {
            let album = Album::from(&api.album(&url.id).map_err(|_| failed("album"))?);
            let check = album.clone();
            (
                Box::new(move || lib.is_saved_album(&check)),
                Box::new(move || {
                    if removed {
                        lib.unsave_album(&album);
                    } else {
                        lib.save_album(&album);
                    }
                }),
            )
        }
        UriType::Artist => {
            let artist = Artist::from(&api.artist(&url.id).map_err(|_| failed("artist"))?);
            let check = artist.clone();
            (
                Box::new(move || lib.is_followed_artist(&check)),
                Box::new(move || {
                    if removed {
                        lib.unfollow_artist(&artist);
                    } else {
                        lib.follow_artist(&artist);
                    }
                }),
            )
        }
        UriType::Show => {
            let show = Show::from(&api.show(&url.id).map_err(|_| failed("show"))?);
            let check = show.clone();
            (
                Box::new(move || lib.is_saved_show(&check)),
                Box::new(move || {
                    if removed {
                        lib.unsave_show(&show);
                    } else {
                        lib.save_show(&show);
                    }
                }),
            )
        }
        UriType::Episode => {
            let episode = Episode::from(&api.episode(&url.id).map_err(|_| failed("episode"))?);
            let check = episode.clone();
            (
                Box::new(move || lib.is_saved_episode(&check) == Some(true)),
                Box::new(move || {
                    if removed {
                        lib.unsave_episode(&episode);
                    } else {
                        lib.save_episode(&episode);
                    }
                }),
            )
        }
        UriType::Playlist => return Err(format!("{verb}: playlists are not a library set")),
    };
    let before = is_saved();
    if !removed && before {
        return Err(format!("{verb}: already in the library"));
    }
    if removed && !before {
        return Err(format!("{verb}: not in the library"));
    }
    write();
    if is_saved() == before {
        return Err(format!("{verb}: library unchanged"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_accepts_every_uri_kind_and_urls() {
        let cases = [
            ("spotify:album:a1", "a1"),
            ("spotify:artist:a2", "a2"),
            ("spotify:show:s1", "s1"),
            ("spotify:playlist:p1", "p1"),
            ("spotify:track:t1", "t1"),
            ("spotify:episode:e1", "e1"),
            ("https://open.spotify.com/album/a3", "a3"),
        ];
        for (input, id) in cases {
            assert_eq!(parse_target(input).unwrap().id, id, "{input}");
        }
        assert!(matches!(
            parse_target("spotify:episode:e1").unwrap().uri_type,
            UriType::Episode
        ));
    }

    #[test]
    fn parse_target_names_a_bad_target() {
        let err = parse_target("not-a-uri").unwrap_err();
        assert!(err.contains("not-a-uri"), "{err}");
    }

    fn sample() -> Dump {
        Dump {
            seq: 7,
            view: Some("album".into()),
            title: "T".into(),
            tab: Some("Tracks".into()),
            tabs: vec![TabState {
                name: "Tracks".into(),
                rows: 12,
                playable: 11,
                max: Some(12),
                busy: false,
            }],
            in_flight: 0,
            quiet_ms: Some(850),
            error: None,
            areas: Areas {
                metadata: AreaCounts {
                    internal_ok: 14,
                    fallback: 0,
                    latched: false,
                },
                library: AreaCounts {
                    internal_ok: 3,
                    fallback: 0,
                    latched: false,
                },
            },
            web: WebCalls {
                shared: 0,
                personal: 2,
            },
            throttled: Throttled {
                shared: false,
                personal: false,
                internal: false,
            },
            library_done: true,
            player: PlayerState {
                status: "playing".into(),
                position_ms: 1200,
            },
            session: SessionState {
                valid: true,
                reconnects: 1,
            },
        }
    }

    #[test]
    fn dump_serializes_to_the_documented_shape() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert_eq!(
            json,
            r#"{"seq":7,"view":"album","title":"T","tab":"Tracks","tabs":[{"name":"Tracks","rows":12,"playable":11,"max":12,"busy":false}],"in_flight":0,"quiet_ms":850,"error":null,"areas":{"metadata":{"internal_ok":14,"fallback":0,"latched":false},"library":{"internal_ok":3,"fallback":0,"latched":false}},"web":{"shared":0,"personal":2},"throttled":{"shared":false,"personal":false,"internal":false},"library_done":true,"player":{"status":"playing","position_ms":1200},"session":{"valid":true,"reconnects":1}}"#
        );
    }

    #[test]
    fn write_dump_replaces_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("ncspot-harness-dump-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.json");
        write_dump(&path, &sample()).unwrap();
        let mut second = sample();
        second.seq = 8;
        write_dump(&path, &second).unwrap();
        let read: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read["seq"], 8);
        assert!(
            !dir.join("dump.json.tmp").exists(),
            "temp file renamed away"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn build_dump_reads_the_counters_and_bumps_seq() {
        let cfg = crate::config::Config::new_for_test();
        let spotify = Spotify::new_for_test(cfg, crate::events::EventManager::new_for_test());
        let first = build_dump("x".into(), None, Some("boom".into()), &spotify, false);
        let second = build_dump("x".into(), None, None, &spotify, false);
        assert!(!first.session.valid);
        assert_eq!(first.session.reconnects, 0);
        assert_eq!(first.player.status, "stopped");
        assert!(second.seq > first.seq);
        assert_eq!(first.error.as_deref(), Some("boom"));
        assert_eq!(first.view, None);
        assert!(first.tabs.is_empty());
        assert_eq!(first.web, WebCalls::default());
        assert!(!first.throttled.shared && !first.throttled.personal && !first.throttled.internal);
    }
}
