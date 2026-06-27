use std::collections::HashMap;
use std::sync::Arc;

use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, Value};

use crate::library::Library;
use crate::model::episode::Episode;
use crate::model::playable::Playable;
use crate::model::track::Track;
use crate::queue::Queue;
use crate::spotify::{Spotify, UriType};

use super::metadata::build_metadata;
use super::{no_track_path, track_path_for_id};

/// Parse `/org/ncspot/queue/<id>` → `id`.
pub(crate) fn parse_queue_path(path: &ObjectPath<'_>) -> Option<u64> {
    path.as_str()
        .strip_prefix("/org/ncspot/queue/")
        .and_then(|s| s.parse::<u64>().ok())
}

/// Resolve a single Spotify track/episode URI to a Playable. Returns None for
/// container URIs (album/playlist/artist/show) — TrackList adds one item.
pub(crate) fn resolve_single_playable(spotify: &Spotify, uri: &str) -> Option<Playable> {
    let id = &uri[uri.rfind(':').map(|i| i + 1).unwrap_or(0)..];
    match uri.parse::<UriType>().ok()? {
        UriType::Track => spotify
            .api
            .track(id)
            .ok()
            .map(|t| Playable::Track(Track::from(&t))),
        UriType::Episode => spotify
            .api
            .episode(id)
            .ok()
            .map(|e| Playable::Episode(Episode::from(&e))),
        _ => None,
    }
}

pub struct MprisTrackList {
    pub queue: Arc<Queue>,
    pub library: Arc<Library>,
    pub spotify: Spotify,
}

#[interface(name = "org.mpris.MediaPlayer2.TrackList")]
impl MprisTrackList {
    #[zbus(property)]
    pub(super) fn tracks(&self) -> Vec<ObjectPath<'static>> {
        self.queue
            .track_ids()
            .into_iter()
            .map(track_path_for_id)
            .collect()
    }

    #[zbus(property)]
    fn can_edit_tracks(&self) -> bool {
        true
    }

    fn get_tracks_metadata(
        &self,
        track_ids: Vec<ObjectPath<'_>>,
    ) -> Vec<HashMap<String, Value<'static>>> {
        track_ids
            .iter()
            .filter_map(|path| {
                let id = parse_queue_path(path)?;
                let index = self.queue.index_for_id(id)?;
                let playable = self.queue.queue.read().unwrap().get(index).cloned()?;
                Some(build_metadata(
                    Some(&playable),
                    track_path_for_id(id),
                    &self.spotify,
                    &self.library,
                ))
            })
            .collect()
    }

    fn add_track(&self, uri: &str, after_track: ObjectPath<'_>, set_as_current: bool) {
        let Some(playable) = resolve_single_playable(&self.spotify, uri) else {
            return;
        };
        // Insert after the entry named by `after_track`, or at the front for NoTrack.
        if after_track.as_str() == no_track_path().as_str() {
            // Insert at front: append then shift to 0.
            self.queue.append(playable);
            let last = self.queue.len().saturating_sub(1);
            self.queue.shift(last, 0);
            if set_as_current {
                self.queue.play(0, false, false);
            }
        } else if let Some(id) = parse_queue_path(&after_track) {
            if let Some(index) = self.queue.index_for_id(id) {
                self.queue.append(playable);
                let last = self.queue.len().saturating_sub(1);
                let dest = index + 1;
                self.queue.shift(last, dest);
                if set_as_current {
                    self.queue.play(dest, false, false);
                }
            }
        }
    }

    fn remove_track(&self, track_id: ObjectPath<'_>) {
        if let Some(id) = parse_queue_path(&track_id) {
            if let Some(index) = self.queue.index_for_id(id) {
                self.queue.remove(index);
            }
        }
    }

    fn go_to(&self, track_id: ObjectPath<'_>) {
        if let Some(id) = parse_queue_path(&track_id) {
            if let Some(index) = self.queue.index_for_id(id) {
                self.queue.play(index, false, false);
            }
        }
    }

    #[zbus(signal)]
    pub(super) async fn track_list_replaced(
        ctx: &SignalEmitter<'_>,
        tracks: Vec<ObjectPath<'_>>,
        current_track: ObjectPath<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub(super) async fn track_added(
        ctx: &SignalEmitter<'_>,
        metadata: HashMap<String, Value<'_>>,
        after_track: ObjectPath<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub(super) async fn track_removed(
        ctx: &SignalEmitter<'_>,
        track_id: ObjectPath<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub(super) async fn track_metadata_changed(
        ctx: &SignalEmitter<'_>,
        track_id: ObjectPath<'_>,
        metadata: HashMap<String, Value<'_>>,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_queue_path_ok() {
        let p = ObjectPath::from_static_str_unchecked("/org/ncspot/queue/42");
        assert_eq!(parse_queue_path(&p), Some(42));
    }

    #[test]
    fn test_parse_queue_path_rejects_other() {
        let p = ObjectPath::from_static_str_unchecked("/org/ncspot/playlist/abc");
        assert_eq!(parse_queue_path(&p), None);
        let none = no_track_path();
        assert_eq!(parse_queue_path(&none), None);
    }
}
