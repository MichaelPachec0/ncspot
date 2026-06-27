use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, Value};

use crate::events::EventManager;
use crate::library::Library;
use crate::model::album::Album;
use crate::model::episode::Episode;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::track::Track;
use crate::queue::{Queue, RepeatSetting};
use crate::spotify::{PlayerEvent, Spotify, VOLUME_PERCENT};
use crate::spotify::UriType;
use crate::spotify_url::SpotifyUrl;

pub struct MprisPlayer {
    pub event: EventManager,
    pub queue: Arc<Queue>,
    pub library: Arc<Library>,
    pub spotify: Spotify,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    #[zbus(property)]
    fn playback_status(&self) -> &str {
        match self.spotify.get_current_status() {
            PlayerEvent::Playing(_) | PlayerEvent::FinishedTrack => "Playing",
            PlayerEvent::Paused(_) => "Paused",
            _ => "Stopped",
        }
    }

    #[zbus(property)]
    fn loop_status(&self) -> &str {
        match self.queue.get_repeat() {
            RepeatSetting::None => "None",
            RepeatSetting::RepeatTrack => "Track",
            RepeatSetting::RepeatPlaylist => "Playlist",
        }
    }

    #[zbus(property)]
    fn set_loop_status(&self, loop_status: &str) {
        let setting = match loop_status {
            "Track" => RepeatSetting::RepeatTrack,
            "Playlist" => RepeatSetting::RepeatPlaylist,
            _ => RepeatSetting::None,
        };
        self.queue.set_repeat(setting);
        self.event.trigger();
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'static>> {
        let current = self.queue.get_current();
        let track_path = self
            .queue
            .get_current_index()
            .and_then(|i| self.queue.id_for_index(i))
            .map(super::track_path_for_id)
            .unwrap_or_else(super::no_track_path);
        super::metadata::build_metadata(current.as_ref(), track_path, &self.spotify, &self.library)
    }

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.queue.get_shuffle()
    }

    #[zbus(property)]
    fn set_shuffle(&self, shuffle: bool) {
        self.queue.set_shuffle(shuffle);
        self.event.trigger();
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.spotify.volume() as f64 / 65535_f64
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) {
        log::info!("set volume: {volume}");
        let clamped = volume.clamp(0.0, 1.0);
        let vol = (VOLUME_PERCENT as f64) * clamped * 100.0;
        self.spotify.set_volume(vol as u16, false);
        self.event.trigger();
    }

    #[zbus(property)]
    fn position(&self) -> i64 {
        self.spotify.get_current_progress().as_micros() as i64
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        // With RepeatPlaylist a non-empty queue can always advance (wraps).
        self.queue.next_index().is_some()
            || (self.queue.get_repeat() == crate::queue::RepeatSetting::RepeatPlaylist
                && self.queue.len() > 0)
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.queue.previous_index().is_some()
            || (self.queue.get_repeat() == crate::queue::RepeatSetting::RepeatPlaylist
                && self.queue.len() > 0)
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.queue.get_current().is_some()
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.queue.get_current().is_some()
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.queue.get_current().is_some()
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }

    #[zbus(signal)]
    pub(super) async fn seeked(context: &SignalEmitter<'_>, position: &i64) -> zbus::Result<()>;

    fn next(&self) {
        self.queue.next(true)
    }

    fn previous(&self) {
        if self.spotify.get_current_progress() < Duration::from_secs(5) {
            self.queue.previous();
        } else {
            self.spotify.seek(0);
        }
    }

    fn pause(&self) {
        self.spotify.pause()
    }

    fn play_pause(&self) {
        self.queue.toggleplayback()
    }

    fn stop(&self) {
        self.queue.stop()
    }

    fn play(&self) {
        self.spotify.play()
    }

    fn seek(&self, offset: i64) {
        if let Some(current_track) = self.queue.get_current() {
            let progress = self.spotify.get_current_progress();
            let new_position = (progress.as_secs() * 1000) as i32
                + progress.subsec_millis() as i32
                + (offset / 1000) as i32;
            let new_position = new_position.max(0) as u32;
            let duration = current_track.duration();

            if new_position < duration {
                self.spotify.seek(new_position);
            } else {
                self.queue.next(true);
            }
        }
    }

    fn set_position(&self, _track: ObjectPath, position: i64) {
        if let Some(current_track) = self.queue.get_current() {
            let position = (position / 1000) as u32;
            let duration = current_track.duration();

            if position < duration {
                self.spotify.seek(position);
            }
        }
    }

    fn open_uri(&self, uri: &str) {
        let spotify_url = if uri.contains("open.spotify.com") {
            SpotifyUrl::from_url(uri)
        } else if let Ok(uri_type) = uri.parse() {
            let id = &uri[uri.rfind(':').unwrap_or(0) + 1..uri.len()];
            Some(SpotifyUrl::new(id, uri_type))
        } else {
            None
        };

        let id = spotify_url
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or("".to_string());
        let uri_type = spotify_url.map(|s| s.uri_type);
        match uri_type {
            Some(UriType::Album) => {
                if let Ok(a) = self.spotify.api.album(&id)
                    && let Some(t) = &Album::from(&a).tracks
                {
                    let should_shuffle = self.queue.get_shuffle();
                    self.queue.clear();
                    let index = self.queue.append_next(
                        &t.iter()
                            .map(|track| Playable::Track(track.clone()))
                            .collect::<Vec<_>>(),
                    );
                    self.queue.play(index, should_shuffle, should_shuffle)
                }
            }
            Some(UriType::Track) => {
                if let Ok(t) = self.spotify.api.track(&id) {
                    self.queue.clear();
                    self.queue.append(Playable::Track(Track::from(&t)));
                    self.queue.play(0, false, false)
                }
            }
            Some(UriType::Playlist) => {
                if let Ok(p) = self.spotify.api.playlist(&id) {
                    let mut playlist = Playlist::from(&p);
                    playlist.load_tracks(&self.spotify);
                    if let Some(tracks) = &playlist.tracks {
                        let should_shuffle = self.queue.get_shuffle();
                        self.queue.clear();
                        let index = self.queue.append_next(tracks);
                        self.queue.play(index, should_shuffle, should_shuffle)
                    }
                }
            }
            Some(UriType::Show) => {
                if let Ok(s) = self.spotify.api.show(&id) {
                    let mut show: Show = (&s).into();
                    let spotify = self.spotify.clone();
                    show.load_all_episodes(spotify);
                    if let Some(e) = &show.episodes {
                        let should_shuffle = self.queue.get_shuffle();
                        self.queue.clear();
                        let mut ep = e.clone();
                        ep.reverse();
                        let index = self.queue.append_next(
                            &ep.iter()
                                .map(|episode| Playable::Episode(episode.clone()))
                                .collect::<Vec<_>>(),
                        );
                        self.queue.play(index, should_shuffle, should_shuffle)
                    }
                }
            }
            Some(UriType::Episode) => {
                if let Ok(e) = self.spotify.api.episode(&id) {
                    self.queue.clear();
                    self.queue.append(Playable::Episode(Episode::from(&e)));
                    self.queue.play(0, false, false)
                }
            }
            Some(UriType::Artist) => {
                if let Ok(a) = self.spotify.api.artist_top_tracks(&id) {
                    let should_shuffle = self.queue.get_shuffle();
                    self.queue.clear();
                    let index = self.queue.append_next(
                        &a.iter()
                            .map(|track| Playable::Track(track.clone()))
                            .collect::<Vec<_>>(),
                    );
                    self.queue.play(index, should_shuffle, should_shuffle)
                }
            }
            None => {}
        }
    }
}
