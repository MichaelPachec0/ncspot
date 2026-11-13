use parking_lot::RwLock;
use std::cmp::Ordering;
use std::sync::Arc;

use log::debug;
#[cfg(feature = "notify")]
use log::info;
#[cfg(feature = "notify")]
use notify_rust::Notification;

use rand::prelude::*;
use strum_macros::Display;

use crate::config::Config;
use crate::library::Library;
use crate::model::playable::Playable;
use crate::spotify::PlayerEvent;
use crate::spotify::Spotify;
use crate::traits::ListItem;

/// Repeat behavior for the [Queue].
#[derive(Display, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RepeatSetting {
    #[serde(rename = "off")]
    None,
    #[serde(rename = "playlist")]
    RepeatPlaylist,
    #[serde(rename = "track")]
    RepeatTrack,
}

/// Events that are specific to the [Queue].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueEvent {
    /// Request the player to 'preload' a track, basically making sure that
    /// transitions between tracks can be uninterrupted.
    PreloadTrackRequest,
}

/// A consistent copy of the persistable queue state.
pub struct QueueSnapshot {
    pub queue: Vec<Playable>,
    pub random_order: Option<Vec<usize>>,
    pub current_track: Option<usize>,
}

/// The three values every play-order question needs, borrowed.
#[derive(Clone, Copy)]
struct Fields<'a> {
    queue: &'a [Playable],
    order: Option<&'a [usize]>,
    current: Option<usize>,
}

impl<'a> Fields<'a> {
    /// Position of the current track within the play order (shuffle-aware).
    /// None if nothing is playing or the current track is not in the order.
    fn order_pos(self) -> Option<usize> {
        let current = self.current?;
        match self.order {
            Some(order) => order.iter().position(|&i| i == current),
            None => Some(current),
        }
    }

    /// The `queue` index at play-order position `pos`. None if out of range.
    fn index_at(self, pos: usize) -> Option<usize> {
        match self.order {
            Some(order) => order.get(pos).copied(),
            None => (pos < self.queue.len()).then_some(pos),
        }
    }

    fn next_index(self) -> Option<usize> {
        self.index_at(self.order_pos()? + 1)
    }

    fn previous_index(self) -> Option<usize> {
        self.index_at(self.order_pos()?.checked_sub(1)?)
    }

    fn skip_index(self, offset: isize) -> Option<usize> {
        let pos = self.order_pos()?;
        let len = self.queue.len();
        if len == 0 {
            return None;
        }
        let target = (pos as isize + offset).clamp(0, len as isize - 1) as usize;
        self.index_at(target)
    }

    fn current_item(self) -> Option<&'a Playable> {
        self.queue.get(self.current?)
    }
}

/// Read guards on queue, random_order and current_track, taken in lock order.
struct QueueRead<'a> {
    queue: parking_lot::RwLockReadGuard<'a, Vec<Playable>>,
    order: parking_lot::RwLockReadGuard<'a, Option<Vec<usize>>>,
    current: parking_lot::RwLockReadGuard<'a, Option<usize>>,
}

impl QueueRead<'_> {
    fn fields(&self) -> Fields<'_> {
        Fields {
            queue: &self.queue,
            order: self.order.as_deref(),
            current: *self.current,
        }
    }
}

/// The queue determines the playback order of [Playable] items, and is also used to control
/// playback itself.
pub struct Queue {
    // LOCK ORDER INVARIANT: when holding more than one of these at once, always
    // acquire in this order: queue -> random_order -> ids -> current_track.
    // `ids` is only ever written while `queue` is write-locked, which lets other
    // methods resolve an id->index under `queue.write()` without racing.
    /// The internal data, which doesn't change with shuffle or repeat. This is
    /// the raw data only.
    queue: Arc<RwLock<Vec<Playable>>>,
    /// The playback order of the queue, as indices into `self.queue`.
    random_order: RwLock<Option<Vec<usize>>>,
    current_track: RwLock<Option<usize>>,
    /// Stable per-entry ids, aligned 1:1 with `self.queue`. Runtime-only; not
    /// persisted. Used by MPRIS to address duplicate queue entries uniquely.
    ids: RwLock<Vec<u64>>,
    /// Monotonic source of new entry ids.
    id_counter: std::sync::atomic::AtomicU64,
    /// The Spotify playlist id most recently loaded via ActivatePlaylist, if any.
    /// Reset to None by clear() so it is never stale after a non-activate queue replacement.
    active_playlist: RwLock<Option<String>>,
    spotify: Spotify,
    cfg: Arc<Config>,
    library: Arc<Library>,
}

impl Queue {
    pub fn new(spotify: Spotify, cfg: Arc<Config>, library: Arc<Library>) -> Self {
        let queue_state = cfg.state().queuestate.clone();
        let id_counter = std::sync::atomic::AtomicU64::new(0);
        let ids = (0..queue_state.queue.len())
            .map(|_| id_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
            .collect::<Vec<u64>>();

        Self {
            queue: Arc::new(RwLock::new(queue_state.queue)),
            spotify: spotify.clone(),
            current_track: RwLock::new(queue_state.current_track),
            random_order: RwLock::new(queue_state.random_order),
            ids: RwLock::new(ids),
            id_counter,
            active_playlist: RwLock::new(None),
            cfg,
            library,
        }
    }

    /// Read guards on the queue, play order and current track, taken once in
    /// lock order. Every play-order question is answered from one of these.
    fn read_all(&self) -> QueueRead<'_> {
        let queue = self.queue.read();
        let order = self.random_order.read();
        let current = self.current_track.read();
        QueueRead {
            queue,
            order,
            current,
        }
    }

    /// The song list, for display. Callers must not change its length.
    pub fn list(&self) -> Arc<RwLock<Vec<Playable>>> {
        self.queue.clone()
    }

    /// The index of the next item in `self.queue` that should be played. None
    /// if at the end of the queue.
    pub fn next_index(&self) -> Option<usize> {
        self.read_all().fields().next_index()
    }

    /// The index of the previous item in `self.queue` that should be played.
    /// None if at the start of the queue.
    pub fn previous_index(&self) -> Option<usize> {
        self.read_all().fields().previous_index()
    }

    /// The `self.queue` index `offset` items from the current one in play
    /// order (negative = backward), clamped to the queue bounds. None if
    /// nothing is playing or the queue is empty.
    fn skip_index(&self, offset: isize) -> Option<usize> {
        self.read_all().fields().skip_index(offset)
    }

    /// Skip `offset` items forward (positive) or backward (negative) in play
    /// order and play the destination. Lets a count-prefixed next/previous
    /// load one track instead of stepping through (and loading) `offset`.
    ///
    /// Intentionally ignores repeat mode: it clamps to the queue ends rather
    /// than wrapping like `next()` under `RepeatPlaylist`, and does not flip
    /// `RepeatTrack` the way `next(true)` does. At an end it plays the last
    /// (or first) track rather than stopping as stepping off the end would.
    /// A bulk count-skip is a jump to a position, not a sequence of
    /// end-of-queue transitions.
    pub fn skip(&self, offset: isize) {
        if let Some(index) = self.skip_index(offset) {
            self.play(index, false, false);
        }
    }

    /// The currently playing item from `self.queue`.
    pub fn get_current(&self) -> Option<Playable> {
        self.read_all().fields().current_item().cloned()
    }

    /// The index of the currently playing item from `self.queue`.
    pub fn get_current_index(&self) -> Option<usize> {
        *self.current_track.read()
    }

    fn next_id(&self) -> u64 {
        self.id_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// The stable id of the queue entry at `index`, if any.
    #[cfg_attr(not(feature = "mpris"), allow(dead_code))]
    pub fn id_for_index(&self, index: usize) -> Option<u64> {
        self.ids.read().get(index).copied()
    }

    /// The current index of the entry with stable id `id`, if still present.
    #[cfg_attr(not(feature = "mpris"), allow(dead_code))]
    pub fn index_for_id(&self, id: u64) -> Option<usize> {
        self.ids.read().iter().position(|&i| i == id)
    }

    /// All entry ids in queue order (aligned with `self.queue`).
    #[cfg_attr(not(feature = "mpris"), allow(dead_code))]
    pub fn track_ids(&self) -> Vec<u64> {
        self.ids.read().clone()
    }

    /// Resolve a batch of `/org/ncspot/queue/<id>` paths to (id, Playable) pairs
    /// in a single pass under one read lock. Unknown/foreign paths are skipped.
    #[cfg(feature = "mpris")]
    pub fn playables_for_paths(
        &self,
        paths: &[zbus::zvariant::ObjectPath<'_>],
    ) -> Vec<(u64, crate::model::playable::Playable)> {
        use std::collections::HashMap;
        let q = self.queue.read();
        let ids = self.ids.read();
        let pos: HashMap<u64, usize> = ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        paths
            .iter()
            .filter_map(|p| {
                let id: u64 = p
                    .as_str()
                    .strip_prefix("/org/ncspot/queue/")?
                    .parse()
                    .ok()?;
                let &index = pos.get(&id)?;
                q.get(index).cloned().map(|pl| (id, pl))
            })
            .collect()
    }

    /// Insert `track` as the item that should logically follow the currently
    /// playing item, taking into account shuffle status.
    pub fn insert_after_current(&self, track: Playable) {
        let Some(index) = self.get_current_index() else {
            // No current track — append handles ids/order/emit.
            self.append(track);
            return;
        };
        let id = self.next_id();
        let after_id = {
            let mut q = self.queue.write();
            let mut random_order = self.random_order.write();
            if let Some(order) = random_order.as_mut() {
                let next_i = order.iter().position(|&i| i == index).unwrap();
                for item in order.iter_mut() {
                    if *item > index {
                        *item += 1;
                    }
                }
                order.insert(next_i + 1, index + 1);
            }
            q.insert(index + 1, track);
            self.ids.write().insert(index + 1, id);
            self.ids.read().get(index).copied()
        };
        #[cfg(not(feature = "mpris"))]
        let _ = after_id;
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitTrackAdded { id, after_id });
    }

    /// Add `track` to the end of the queue.
    pub fn append(&self, track: Playable) {
        let id = self.next_id();
        {
            let mut q = self.queue.write();
            let mut random_order = self.random_order.write();
            if let Some(order) = random_order.as_mut() {
                order.push(order.len());
            }
            q.push(track);
            self.ids.write().push(id);
        }
        #[cfg(feature = "mpris")]
        {
            let after_id = {
                let ids = self.ids.read();
                (ids.len() >= 2).then(|| ids[ids.len() - 2])
            };
            self.spotify
                .send_mpris(crate::mpris::MprisCommand::EmitTrackAdded { id, after_id });
        }
    }

    /// One consistent copy, read under the read guards in lock order.
    pub fn snapshot(&self) -> QueueSnapshot {
        let all = self.read_all();
        QueueSnapshot {
            queue: all.queue.clone(),
            random_order: all.order.clone(),
            current_track: *all.current,
        }
    }

    /// Append `tracks` after the currently playing item, taking into account
    /// shuffle status. Returns the first index(in `self.queue`) of added items.
    pub fn append_next(&self, tracks: &[Playable]) -> usize {
        let first = {
            let mut q = self.queue.write();

            let first = match *self.current_track.read() {
                Some(index) => index + 1,
                None => q.len(),
            };
            let k = tracks.len();

            {
                let mut random_order = self.random_order.write();
                if let Some(order) = random_order.as_mut() {
                    for v in order.iter_mut() {
                        if *v >= first {
                            *v += k;
                        }
                    }
                    let at = match *self.current_track.read() {
                        Some(cur) => order
                            .iter()
                            .position(|&v| v == cur)
                            .map_or(order.len(), |p| p + 1),
                        None => order.len(),
                    };
                    order.splice(at..at, first..first + k);
                }
            }

            for (i, track) in (first..).zip(tracks.iter()) {
                q.insert(i, track.clone());
            }

            let new_ids: Vec<u64> = (0..tracks.len()).map(|_| self.next_id()).collect();
            self.ids.write().splice(first..first, new_ids);

            first
        };
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitTrackListReplaced);
        first
    }

    /// Remove the item at `index` in `self.queue`. No-op if out of bounds.
    pub fn remove(&self, index: usize) {
        let removed_id = {
            let mut q = self.queue.write();
            if index >= q.len() {
                return;
            }
            #[cfg(feature = "mpris")]
            let rid = self.ids.read().get(index).copied();
            #[cfg(not(feature = "mpris"))]
            let rid: Option<u64> = None;
            q.remove(index);
            self.ids.write().remove(index);
            rid
        };
        self.after_remove(index, removed_id);
    }

    /// Remove the queue entry with stable id `id`, resolving the index atomically.
    /// No-op if the id is no longer present.
    pub fn remove_by_id(&self, id: u64) {
        let (index, removed_id) = {
            let mut q = self.queue.write();
            let index = match self.ids.read().iter().position(|&i| i == id) {
                Some(i) => i,
                None => return,
            };
            q.remove(index);
            self.ids.write().remove(index);
            (index, Some(id))
        };
        self.after_remove(index, removed_id);
    }

    /// Shared post-removal bookkeeping: stop/advance playback as needed, fix the
    /// current index, regenerate shuffle order, and emit TrackRemoved.
    fn after_remove(&self, index: usize, removed_id: Option<u64>) {
        let _ = removed_id;
        let len = self.queue.read().len();
        if len == 0 {
            self.stop();
            if let Some(order) = self.random_order.write().as_mut() {
                order.clear();
            }
            #[cfg(feature = "mpris")]
            if let Some(id) = removed_id {
                self.spotify
                    .send_mpris(crate::mpris::MprisCommand::EmitTrackRemoved(id));
            }
            return;
        }

        let current = *self.current_track.read();
        if let Some(current_track) = current {
            match current_track.cmp(&index) {
                Ordering::Equal => {
                    if current_track == len {
                        if self.get_repeat() == RepeatSetting::RepeatPlaylist {
                            if self.get_shuffle() {
                                // current_track is stale (== len); clear it so the
                                // regenerated order holds only valid indices.
                                *self.current_track.write() = None;
                                self.generate_random_order();
                            }
                            let first = self
                                .random_order
                                .read()
                                .as_ref()
                                .and_then(|o| o.first().copied())
                                .unwrap_or(0);
                            self.play(first, false, false);
                        } else {
                            self.stop();
                        }
                    } else {
                        self.play(index, false, false);
                    }
                }
                Ordering::Greater => {
                    let mut current = self.current_track.write();
                    current.replace(current_track - 1);
                }
                _ => (),
            }
        }

        if self.get_shuffle() {
            self.generate_random_order();
        }

        #[cfg(feature = "mpris")]
        if let Some(id) = removed_id {
            self.spotify
                .send_mpris(crate::mpris::MprisCommand::EmitTrackRemoved(id));
        }
    }

    /// Insert `track` at `index` (clamped to the queue length), maintaining
    /// ids, random_order and current_track, and emit a single TrackAdded
    /// carrying the predecessor's id. Returns the new entry's id.
    pub fn insert_at(&self, index: usize, track: Playable) -> u64 {
        let id = self.next_id();
        let after_id = {
            let mut q = self.queue.write();
            let index = index.min(q.len());

            let mut random_order = self.random_order.write();
            if let Some(order) = random_order.as_mut() {
                for item in order.iter_mut() {
                    if *item >= index {
                        *item += 1;
                    }
                }
                order.push(index);
            }
            drop(random_order);

            q.insert(index, track);

            let mut ids = self.ids.write();
            let after_id = (index > 0).then(|| ids[index - 1]);
            ids.insert(index, id);
            drop(ids);

            let mut current = self.current_track.write();
            if let Some(ci) = *current
                && ci >= index
            {
                current.replace(ci + 1);
            }
            after_id
        };
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitTrackAdded { id, after_id });
        #[cfg(not(feature = "mpris"))]
        let _ = after_id;
        id
    }

    /// Resolve `after_id` -> index and insert `track` immediately after it.
    /// `after_id == None` inserts at the front. Returns the index inserted at,
    /// or None if `after_id` is no longer present in the queue.
    ///
    /// The id->index resolution is race-free (done under `queue.write()` so ids
    /// cannot change during the lookup), but `insert_at` re-acquires the lock.
    /// The residual gap is benign: `insert_at` clamps the index, so it never
    /// panics and the track lands at the correct position.
    pub fn insert_after_id(&self, after_id: Option<u64>, track: Playable) -> Option<usize> {
        // Acquire queue.write() to resolve the id->index atomically; ids are
        // only mutated while queue is write-locked, so the position is stable
        // for this critical section. insert_at re-acquires the lock; the
        // residual gap is benign because insert_at clamps the index.
        let index = {
            let q = self.queue.write();
            let index = match after_id {
                None => 0,
                Some(aid) => {
                    let pos = self.ids.read().iter().position(|&i| i == aid)?;
                    pos + 1
                }
            };
            drop(q);
            index
        };
        self.insert_at(index, track);
        Some(index)
    }

    /// Play the entry with stable id `id`. No-op if the id is no longer present.
    pub fn play_by_id(&self, id: u64) {
        if let Some(index) = self.index_for_id(id) {
            self.play(index, false, false);
        }
    }

    /// Clear all the items from the queue and stop playback.
    pub fn clear(&self) {
        self.stop();

        {
            let mut q = self.queue.write();
            q.clear();
            self.ids.write().clear();

            let mut random_order = self.random_order.write();
            if let Some(o) = random_order.as_mut() {
                o.clear()
            }
        }
        // Any non-ActivatePlaylist queue replacement clears the active playlist.
        self.set_active_playlist(None);
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitTrackListReplaced);
    }

    /// The amount of items in `self.queue`.
    pub fn len(&self) -> usize {
        self.queue.read().len()
    }

    /// Shift the item at `from` in `self.queue` to `to`.
    pub fn shift(&self, from: usize, to: usize) {
        {
            let mut queue = self.queue.write();
            if from == to || from >= queue.len() || to >= queue.len() {
                return;
            }
            let item = queue.remove(from);
            queue.insert(to, item);
            let mut ids = self.ids.write();
            let id = ids.remove(from);
            ids.insert(to, id);

            // Keep the shuffle order pointing at the same logical tracks after the move.
            let mut random_order = self.random_order.write();
            if let Some(order) = random_order.as_mut() {
                for v in order.iter_mut() {
                    if *v == from {
                        *v = to;
                    } else if from < to && *v > from && *v <= to {
                        *v -= 1;
                    } else if from > to && *v >= to && *v < from {
                        *v += 1;
                    }
                }
            }
            drop(random_order);

            // if the currently playing track is affected by the shift, update its
            // index
            let mut current = self.current_track.write();
            if let Some(index) = *current {
                if index == from {
                    current.replace(to);
                } else if from < to && index > from && index <= to {
                    current.replace(index - 1);
                } else if from > to && index >= to && index < from {
                    current.replace(index + 1);
                }
            }
        }
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitTrackListReplaced);
    }

    /// Play the item at `index` in `self.queue`.
    ///
    /// `reshuffle`: Reshuffle the current order of the queue.
    /// `shuffle_index`: If this is true, `index` isn't actually used, but is
    /// chosen at random as a valid index in the queue.
    pub fn play(&self, mut index: usize, reshuffle: bool, shuffle_index: bool) {
        let queue_length = self.queue.read().len();
        // The length of the queue must be bigger than 0 or gen_range panics!
        if queue_length > 0 && shuffle_index && self.get_shuffle() {
            let mut rng = rand::rng();
            index = rng.random_range(0..queue_length);
        }

        let track = self.queue.read().get(index).cloned();
        if let Some(track) = &track {
            self.spotify.load(track, true, 0);
            let mut current = self.current_track.write();
            current.replace(index);
            self.spotify.update_track();

            #[cfg(feature = "notify")]
            if self.cfg.values().notify.unwrap_or(false) {
                std::thread::spawn({
                    // use same parser as track_format, Playable::format
                    let format = self
                        .cfg
                        .values()
                        .notification_format
                        .clone()
                        .unwrap_or_default();
                    let default_title = crate::config::NotificationFormat::default().title.unwrap();
                    let title = format.title.unwrap_or_else(|| default_title.clone());

                    let default_body = crate::config::NotificationFormat::default().body.unwrap();
                    let body = format.body.unwrap_or_else(|| default_body.clone());

                    let summary_txt = Playable::format(track, &title, &self.library);
                    let body_txt = Playable::format(track, &body, &self.library);
                    let cover_url = track.cover_url();
                    move || send_notification(&summary_txt, &body_txt, cover_url)
                });
            }

            // Send a Seeked signal at start of new track
            #[cfg(feature = "mpris")]
            self.spotify.notify_seeked(0);
        }

        if reshuffle && self.get_shuffle() {
            self.generate_random_order()
        }
    }

    /// Toggle the playback. If playback is currently stopped, this will either
    /// play the next song if one is available, or restart from the start.
    pub fn toggleplayback(&self) {
        match self.spotify.get_current_status() {
            PlayerEvent::Playing(_) | PlayerEvent::Paused(_) => {
                self.spotify.toggleplayback();
            }
            PlayerEvent::Stopped => match self.next_index() {
                Some(_) => self.next(false),
                None => self.play(0, false, false),
            },
            _ => (),
        }
    }

    /// Stop playback.
    pub fn stop(&self) {
        let mut current = self.current_track.write();
        *current = None;
        self.spotify.stop();
    }

    /// Play the next song in the queue. Stops playback if there are no playable tracks
    /// remaining.
    ///
    /// `manual`: If this is true, normal queue logic like repeat will not be
    /// used, and the next track will actually be played. This should be used
    /// when going to the next entry in the queue is the wanted behavior.
    pub fn next(&self, manual: bool) {
        // Taken before the guards below: the getter locks queue itself.
        let next_index = self.next_index();
        let q = self.queue.read();
        let current = *self.current_track.read();
        let repeat = self.cfg.state().repeat;

        if repeat == RepeatSetting::RepeatTrack && !manual {
            if let Some(index) = current
                && q[index].is_playable()
            {
                self.play(index, false, false);
            }
        } else if let Some(index) = next_index {
            self.play(index, false, false);
            if repeat == RepeatSetting::RepeatTrack && manual {
                self.set_repeat(RepeatSetting::RepeatPlaylist);
            }
        } else if repeat == RepeatSetting::RepeatPlaylist
            && !q.is_empty()
            && q.iter().any(|track| track.is_playable())
        {
            let random_order = self.random_order.read();
            self.play(
                random_order.as_ref().map(|o| o[0]).unwrap_or(0),
                false,
                false,
            );
        } else {
            self.spotify.stop();
        }
    }

    /// Play the previous item in the queue.
    pub fn previous(&self) {
        // Taken before the guards below: the getter locks queue itself.
        let previous_index = self.previous_index();
        let q = self.queue.read();
        let current = *self.current_track.read();
        let repeat = self.cfg.state().repeat;

        if let Some(index) = previous_index {
            self.play(index, false, false);
        } else if repeat == RepeatSetting::RepeatPlaylist && !q.is_empty() {
            if self.get_shuffle() {
                let random_order = self.random_order.read();
                self.play(
                    random_order.as_ref().map(|o| o[q.len() - 1]).unwrap_or(0),
                    false,
                    false,
                );
            } else {
                self.play(q.len() - 1, false, false);
            }
        } else if let Some(index) = current {
            self.play(index, false, false);
        }
    }

    /// Get the current repeat behavior.
    pub fn get_repeat(&self) -> RepeatSetting {
        self.cfg.state().repeat
    }

    /// Set the current repeat behavior and save it to the configuration.
    pub fn set_repeat(&self, new: RepeatSetting) {
        self.cfg.with_state_mut(|s| s.repeat = new);
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitLoopStatus);
    }

    /// Get the current shuffle behavior.
    pub fn get_shuffle(&self) -> bool {
        self.cfg.state().shuffle
    }

    /// Get the current order that is used to shuffle.
    pub fn get_random_order(&self) -> Option<Vec<usize>> {
        self.random_order.read().clone()
    }

    /// (Re)generate the random shuffle order.
    fn generate_random_order(&self) {
        let q = self.queue.read();
        let mut order: Vec<usize> = Vec::with_capacity(q.len());
        let mut random: Vec<usize> = (0..q.len()).collect();

        if let Some(current) = *self.current_track.read() {
            order.push(current);
            random.remove(current);
        }

        let mut rng = rand::rng();
        random.shuffle(&mut rng);
        order.extend(random);

        let mut random_order = self.random_order.write();
        *random_order = Some(order);
    }

    /// Set the current shuffle behavior.
    pub fn set_shuffle(&self, new: bool) {
        self.cfg.with_state_mut(|s| s.shuffle = new);
        if new {
            self.generate_random_order();
        } else {
            let mut random_order = self.random_order.write();
            *random_order = None;
        }
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitShuffleStatus);
    }

    /// Handle events that are specific to the queue.
    pub fn handle_event(&self, event: QueueEvent) {
        match event {
            QueueEvent::PreloadTrackRequest => {
                let next = {
                    let all = self.read_all();
                    let f = all.fields();
                    f.next_index().and_then(|i| f.queue.get(i).cloned())
                };
                if let Some(track) = next {
                    debug!("Preloading track {track} as requested by librespot");
                    self.spotify.preload(&track);
                }
            }
        }
    }

    /// The playlist id the queue was last loaded from via ActivatePlaylist, if any.
    #[cfg_attr(not(feature = "mpris"), allow(dead_code))]
    pub fn active_playlist(&self) -> Option<String> {
        self.active_playlist.read().clone()
    }

    /// Record (or clear) the active source playlist and notify MPRIS.
    #[cfg_attr(not(feature = "mpris"), allow(dead_code))]
    pub fn set_active_playlist(&self, id: Option<String>) {
        {
            let mut g = self.active_playlist.write();
            if *g == id {
                return;
            }
            *g = id;
        }
        #[cfg(feature = "mpris")]
        self.spotify
            .send_mpris(crate::mpris::MprisCommand::EmitActivePlaylistChanged);
    }

    /// Get the spotify session.
    pub fn get_spotify(&self) -> Spotify {
        self.spotify.clone()
    }
}

/// Id of the last notification shown by ncspot
#[cfg(all(unix, not(target_os = "macos")))]
static LAST_NOTIFICATION_ID: parking_lot::Mutex<Option<u32>> = parking_lot::Mutex::new(None);

/// Send a notification using the desktops default notification method.
///
/// `summary_txt`: A short title for the notification.
/// `body_txt`: The actual content of the notification.
/// `cover_url`: A URL to an image to show in the notification.
///
/// reuses the id of the last notification shown if possible
#[cfg(feature = "notify")]
pub fn send_notification(summary_txt: &str, body_txt: &str, cover_url: Option<String>) {
    let mut n = Notification::new();
    n.appname("ncspot").summary(summary_txt).body(body_txt);

    // album cover image
    if let Some(u) = cover_url {
        let path = crate::utils::cache_path_for_url(u.to_string());
        if !path.exists()
            && let Err(e) = crate::utils::download(u, path.clone())
        {
            log::error!("Failed to download cover: {e}");
        }
        n.icon(path.to_str().unwrap());
    }

    // XDG desktop entry hints
    #[cfg(all(unix, not(target_os = "macos")))]
    n.urgency(notify_rust::Urgency::Low)
        .hint(notify_rust::Hint::Transient(true))
        .hint(notify_rust::Hint::DesktopEntry("ncspot".into()));

    // reuse the id of the previous notification
    // so we replace it and not create a new one
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(notification_id) = *LAST_NOTIFICATION_ID.lock() {
        n.id(notification_id);
    }

    match n.show() {
        Ok(handle) => {
            // only available for XDG
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                info!("Created notification: {}", handle.id());
                *LAST_NOTIFICATION_ID.lock() = Some(handle.id());
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            drop(handle);
        }
        Err(e) => log::error!("Failed to send notification cover: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::RwLock;
    use std::sync::Arc;

    use super::*;
    use crate::config::Config;
    use crate::events::EventManager;
    use crate::library::Library;
    use crate::model::track::Track;
    use crate::spotify::Spotify;

    fn make_track(id: u32) -> Playable {
        Playable::Track(Track {
            id: Some(format!("id_{id}")),
            uri: format!("spotify:track:id_{id}"),
            title: format!("Track {id}"),
            track_number: id,
            disc_number: 1,
            duration: 180_000,
            artists: vec!["Artist".to_string()],
            artist_ids: vec![],
            album: Some("Album".to_string()),
            album_id: None,
            album_artists: vec![],
            cover_url: None,
            url: String::new(),
            added_at: None,
            list_index: 0,
            is_local: false,
            // Must be Some(true) so Spotify::load() doesn't fire events.
            is_playable: Some(true),
        })
    }

    fn track_id(p: &Playable) -> &str {
        match p {
            Playable::Track(t) => t.id.as_deref().unwrap_or(""),
            Playable::Episode(_) => "",
        }
    }

    fn make_queue(tracks: Vec<Playable>, current: Option<usize>) -> Queue {
        let cfg = Config::new_for_test();
        let ev = EventManager::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        let library = Library::new_for_test(ev, spotify.clone(), cfg.clone());
        let id_counter = std::sync::atomic::AtomicU64::new(0);
        let ids = (0..tracks.len())
            .map(|_| id_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
            .collect::<Vec<u64>>();
        Queue {
            queue: Arc::new(RwLock::new(tracks)),
            random_order: RwLock::new(None),
            current_track: RwLock::new(current),
            ids: RwLock::new(ids),
            id_counter,
            active_playlist: RwLock::new(None),
            spotify,
            cfg,
            library,
        }
    }

    // --- next_index / previous_index ---

    #[test]
    fn test_next_index_basic() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(0));
        assert_eq!(q.next_index(), Some(1));
    }

    #[test]
    fn test_next_index_at_end_returns_none() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(2));
        assert_eq!(q.next_index(), None);
    }

    #[test]
    fn test_next_index_no_current_returns_none() {
        let q = make_queue(vec![make_track(0), make_track(1)], None);
        assert_eq!(q.next_index(), None);
    }

    #[test]
    fn test_previous_index_basic() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(2));
        assert_eq!(q.previous_index(), Some(1));
    }

    #[test]
    fn test_previous_index_at_start_returns_none() {
        let q = make_queue(vec![make_track(0), make_track(1)], Some(0));
        assert_eq!(q.previous_index(), None);
    }

    #[test]
    fn test_previous_index_no_current_returns_none() {
        let q = make_queue(vec![make_track(0)], None);
        assert_eq!(q.previous_index(), None);
    }

    // --- next / previous with shuffle ---

    #[test]
    fn test_next_index_respects_random_order() {
        // Queue [0,1,2,3], current_track=2, random_order=[2,0,3,1]
        // Position of 2 in order = 0, next position = 1, order[1] = 0 → Some(0)
        let q = make_queue(
            vec![make_track(0), make_track(1), make_track(2), make_track(3)],
            Some(2),
        );
        *q.random_order.write() = Some(vec![2, 0, 3, 1]);
        assert_eq!(q.next_index(), Some(0));
    }

    #[test]
    fn test_previous_index_respects_random_order() {
        // Queue [0,1,2,3], current_track=0, random_order=[2,0,3,1]
        // Position of 0 in order = 1, prev position = 0, order[0] = 2 → Some(2)
        let q = make_queue(
            vec![make_track(0), make_track(1), make_track(2), make_track(3)],
            Some(0),
        );
        *q.random_order.write() = Some(vec![2, 0, 3, 1]);
        assert_eq!(q.previous_index(), Some(2));
    }

    #[test]
    fn test_next_index_at_end_of_shuffle_order_returns_none() {
        // Queue [0,1,2,3], current_track=1, random_order=[2,0,3,1]
        // Position of 1 in order = 3 (last), next = 4 → None
        let q = make_queue(
            vec![make_track(0), make_track(1), make_track(2), make_track(3)],
            Some(1),
        );
        *q.random_order.write() = Some(vec![2, 0, 3, 1]);
        assert_eq!(q.next_index(), None);
    }

    // --- skip_index (count-prefixed next/previous) ---

    #[test]
    fn test_skip_index_forward_and_backward() {
        let q = make_queue(
            vec![make_track(0), make_track(1), make_track(2), make_track(3)],
            Some(1),
        );
        assert_eq!(q.skip_index(2), Some(3));
        assert_eq!(q.skip_index(-1), Some(0));
    }

    #[test]
    fn test_skip_index_clamps_to_queue_bounds() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(1));
        assert_eq!(q.skip_index(100), Some(2));
        assert_eq!(q.skip_index(-100), Some(0));
    }

    #[test]
    fn test_skip_index_no_current_returns_none() {
        let q = make_queue(vec![make_track(0), make_track(1)], None);
        assert_eq!(q.skip_index(1), None);
    }

    #[test]
    fn test_skip_index_respects_random_order() {
        // Queue [0,1,2,3], current_track=2, random_order=[2,0,3,1]
        // Position of 2 in order = 0, +2 = 2, order[2] = 3 → Some(3)
        let q = make_queue(
            vec![make_track(0), make_track(1), make_track(2), make_track(3)],
            Some(2),
        );
        *q.random_order.write() = Some(vec![2, 0, 3, 1]);
        assert_eq!(q.skip_index(2), Some(3));
    }

    // --- len, get_current ---

    #[test]
    fn test_len() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], None);
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn test_get_current_returns_right_track() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(1));
        let current = q.get_current().expect("should have current");
        assert_eq!(track_id(&current), "id_1");
    }

    #[test]
    fn test_get_current_none_when_no_current() {
        let q = make_queue(vec![make_track(0)], None);
        assert!(q.get_current().is_none());
    }

    // --- append / append_next / insert_after_current ---

    #[test]
    fn test_append_increases_len() {
        let q = make_queue(vec![make_track(0)], None);
        q.append(make_track(1));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn test_append_adds_to_end() {
        let q = make_queue(vec![make_track(0), make_track(1)], None);
        q.append(make_track(2));
        let queue = q.queue.read();
        assert_eq!(track_id(&queue[2]), "id_2");
    }

    #[test]
    fn test_append_next_inserts_after_current() {
        // [0, 1, 2], current=1 → append_next([3, 4]) → [0, 1, 3, 4, 2]
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(1));
        let first = q.append_next(&[make_track(3), make_track(4)]);
        assert_eq!(first, 2);
        let queue = q.queue.read();
        let ids: Vec<&str> = queue.iter().map(track_id).collect();
        assert_eq!(ids, ["id_0", "id_1", "id_3", "id_4", "id_2"]);
    }

    #[test]
    fn test_append_next_no_current_appends_at_end() {
        let q = make_queue(vec![make_track(0), make_track(1)], None);
        let first = q.append_next(&[make_track(2)]);
        assert_eq!(first, 2);
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn test_insert_after_current() {
        // [0, 1], current=0 → insert 99 → [0, 99, 1], current still 0, next is 99
        let q = make_queue(vec![make_track(0), make_track(1)], Some(0));
        q.insert_after_current(make_track(99));
        assert_eq!(q.len(), 3);
        assert_eq!(q.get_current_index(), Some(0));
        let queue = q.queue.read();
        assert_eq!(track_id(&queue[1]), "id_99");
        assert_eq!(track_id(&queue[2]), "id_1");
    }

    #[test]
    fn test_insert_after_current_no_current_appends() {
        let q = make_queue(vec![make_track(0)], None);
        q.insert_after_current(make_track(1));
        assert_eq!(q.len(), 2);
    }

    // --- remove ---

    #[test]
    fn test_remove_item_after_current_keeps_index() {
        // [0, 1, 2], current=0, remove(2) → current stays 0
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(0));
        q.remove(2);
        assert_eq!(q.len(), 2);
        assert_eq!(q.get_current_index(), Some(0));
    }

    #[test]
    fn test_remove_item_before_current_adjusts_index() {
        // [0, 1, 2], current=2, remove(0) → current becomes 1
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(2));
        q.remove(0);
        assert_eq!(q.len(), 2);
        assert_eq!(q.get_current_index(), Some(1));
    }

    // --- shift ---

    #[test]
    fn test_shift_moves_item() {
        // [0, 1, 2], shift(2, 0) → [2, 0, 1]
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], None);
        q.shift(2, 0);
        let queue = q.queue.read();
        let ids: Vec<&str> = queue.iter().map(track_id).collect();
        assert_eq!(ids, ["id_2", "id_0", "id_1"]);
    }

    #[test]
    fn test_shift_updates_current_when_current_is_moved() {
        // [0, 1, 2], current=2, shift(2, 0) → current becomes 0
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(2));
        q.shift(2, 0);
        assert_eq!(q.get_current_index(), Some(0));
    }

    #[test]
    fn test_shift_remaps_random_order() {
        // queue [0,1,2,3], explicit shuffle order [2,0,3,1]
        let q = make_queue(
            vec![make_track(0), make_track(1), make_track(2), make_track(3)],
            Some(0),
        );
        *q.random_order.write() = Some(vec![2, 0, 3, 1]);
        // capture the playable each order-slot points at, by track id
        let before: Vec<String> = q
            .get_random_order()
            .unwrap()
            .iter()
            .map(|&i| track_id(&q.queue.read()[i]).to_string())
            .collect();
        // move queue[0] to position 2
        q.shift(0, 2);
        let after: Vec<String> = q
            .get_random_order()
            .unwrap()
            .iter()
            .map(|&i| track_id(&q.queue.read()[i]).to_string())
            .collect();
        // the shuffle SEQUENCE (which tracks play in which order) must be unchanged
        assert_eq!(before, after);
        // and random_order is still a permutation of 0..len
        let mut order = q.get_random_order().unwrap();
        order.sort_unstable();
        assert_eq!(order, (0..q.len()).collect::<Vec<_>>());
    }

    // --- shuffle ---

    #[test]
    fn test_shuffle_generates_random_order() {
        let q = make_queue(
            vec![make_track(0), make_track(1), make_track(2), make_track(3)],
            Some(1),
        );
        q.set_shuffle(true);
        let order = q.get_random_order().expect("random_order should be Some");
        assert_eq!(order.len(), 4);
        // Current track (index 1) must be first.
        assert_eq!(order[0], 1);
        // All queue indices appear exactly once.
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, [0, 1, 2, 3]);
    }

    #[test]
    fn test_shuffle_disable_clears_order() {
        let q = make_queue(vec![make_track(0), make_track(1)], Some(0));
        q.set_shuffle(true);
        assert!(q.get_random_order().is_some());
        q.set_shuffle(false);
        assert!(q.get_random_order().is_none());
    }

    #[test]
    fn test_append_in_shuffle_mode_maintains_valid_order() {
        // After appending to a shuffled queue, random_order must contain every
        // queue index exactly once.
        let q = make_queue(vec![make_track(0), make_track(1)], Some(0));
        q.set_shuffle(true);
        q.append(make_track(2));
        let order = q.get_random_order().expect("random_order should be Some");
        assert_eq!(order.len(), 3);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            [0, 1, 2],
            "random_order must contain every queue index once"
        );
    }

    // --- insert_at / insert_after_id / remove_by_id ---

    #[test]
    fn test_insert_at_middle_maintains_alignment_and_order() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(1));
        q.set_shuffle(true);
        let id = q.insert_at(1, make_track(9));
        assert_eq!(q.index_for_id(id), Some(1));
        assert_eq!(q.track_ids().len(), q.len());
        // current_track (was index 1) shifted to 2 because we inserted at 1
        assert_eq!(q.get_current_index(), Some(2));
        let mut order = q.get_random_order().unwrap();
        order.sort_unstable();
        assert_eq!(order, (0..q.len()).collect::<Vec<_>>());
    }

    #[test]
    fn test_insert_after_id_resolves_and_returns_index() {
        let q = make_queue(vec![make_track(0), make_track(1)], Some(0));
        let id0 = q.id_for_index(0).unwrap();
        let idx = q.insert_after_id(Some(id0), make_track(9));
        assert_eq!(idx, Some(1));
        assert_eq!(track_id(&q.queue.read()[1]), "id_9");
    }

    #[test]
    fn test_insert_after_id_unknown_returns_none() {
        let q = make_queue(vec![make_track(0)], Some(0));
        assert_eq!(q.insert_after_id(Some(123_456), make_track(9)), None);
        assert_eq!(q.len(), 1); // nothing inserted
    }

    #[test]
    fn test_remove_by_id_removes_correct_entry() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(0));
        let id1 = q.id_for_index(1).unwrap();
        q.remove_by_id(id1);
        assert_eq!(q.len(), 2);
        assert_eq!(q.index_for_id(id1), None);
        assert_eq!(track_id(&q.queue.read()[1]), "id_2");
    }

    #[test]
    fn test_remove_by_id_unknown_is_noop() {
        let q = make_queue(vec![make_track(0)], Some(0));
        q.remove_by_id(999); // must not panic
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn test_remove_out_of_bounds_index_is_noop() {
        let q = make_queue(vec![make_track(0)], Some(0));
        q.remove(5); // previously panicked; must be a no-op now
        assert_eq!(q.len(), 1);
    }

    // --- clear ---

    #[test]
    fn test_clear_empties_queue() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(1));
        q.clear();
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn test_clear_empties_random_order_when_shuffled() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(1));
        q.set_shuffle(true);
        q.clear();
        // clear() clears the order contents but leaves it as Some([]).
        assert_eq!(q.get_random_order(), Some(vec![]));
    }

    #[test]
    fn test_active_playlist_set_and_cleared_by_clear() {
        let q = make_queue(vec![make_track(0)], Some(0));
        q.set_active_playlist(Some("pl_abc".to_string()));
        assert_eq!(q.active_playlist(), Some("pl_abc".to_string()));
        q.clear();
        assert_eq!(q.active_playlist(), None);
    }

    #[test]
    fn test_ids_aligned_after_append() {
        let q = make_queue(vec![make_track(0), make_track(1)], Some(0));
        q.append(make_track(2));
        assert_eq!(q.track_ids().len(), q.len());
        // ids are unique
        let mut ids = q.track_ids();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before);
    }

    #[test]
    fn test_id_is_stable_across_remove_of_other() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(0));
        let id_of_2 = q.id_for_index(2).unwrap();
        q.remove(0); // remove the first entry
        // the entry formerly at index 2 keeps its id, now at index 1
        assert_eq!(q.index_for_id(id_of_2), Some(1));
    }

    #[test]
    fn test_index_for_unknown_id_is_none() {
        let q = make_queue(vec![make_track(0)], Some(0));
        assert_eq!(q.index_for_id(99_999), None);
    }

    #[test]
    fn test_ids_aligned_after_shift() {
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(0));
        let id_first = q.id_for_index(0).unwrap();
        q.shift(0, 2);
        assert_eq!(q.index_for_id(id_first), Some(2));
        assert_eq!(q.track_ids().len(), q.len());
    }

    #[test]
    fn test_append_next_aligns_ids() {
        let q = make_queue(vec![make_track(0), make_track(1)], Some(0));
        q.append_next(&[make_track(2), make_track(3)]);
        assert_eq!(q.track_ids().len(), q.len());
        let mut ids = q.track_ids();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "ids must stay unique");
    }

    #[cfg(feature = "mpris")]
    #[test]
    fn test_playables_for_paths_o1_resolution() {
        use zbus::zvariant::ObjectPath;
        let q = make_queue(vec![make_track(0), make_track(1), make_track(2)], Some(0));
        let id2 = q.id_for_index(2).unwrap();
        let id0 = q.id_for_index(0).unwrap();
        let paths = vec![
            ObjectPath::from_string_unchecked(format!("/org/ncspot/queue/{id2}")),
            ObjectPath::from_static_str_unchecked("/org/ncspot/queue/999999"), // unknown -> skipped
            ObjectPath::from_string_unchecked(format!("/org/ncspot/queue/{id0}")),
        ];
        let got = q.playables_for_paths(&paths);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, id2);
        assert_eq!(track_id(&got[0].1), "id_2");
        assert_eq!(got[1].0, id0);
        assert_eq!(track_id(&got[1].1), "id_0");
    }

    #[test]
    fn test_append_then_insert_after_current_alignment() {
        let q = make_queue(vec![make_track(0), make_track(1)], Some(0));
        q.set_shuffle(true);
        q.append(make_track(2));
        q.insert_after_current(make_track(3));
        // ids and queue stay equal length and unique
        assert_eq!(q.track_ids().len(), q.len());
        let mut ids = q.track_ids();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before);
        // random_order is a valid permutation of 0..len
        let mut order = q.get_random_order().unwrap();
        order.sort_unstable();
        assert_eq!(order, (0..q.len()).collect::<Vec<_>>());
    }

    // --- Task 4: order/current fixes and snapshot ---

    fn assert_invariant(q: &Queue) {
        let len = q.queue.read().len();
        assert_eq!(q.ids.read().len(), len);
        if let Some(order) = q.random_order.read().as_ref() {
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..len).collect::<Vec<_>>());
        }
        if let Some(c) = *q.current_track.read() {
            assert!(c < len);
        }
    }

    #[test]
    fn append_next_keeps_a_valid_shuffle_order() {
        let q = make_queue((0..4).map(make_track).collect(), Some(1));
        *q.random_order.write() = Some(vec![1, 3, 0, 2]);
        let first = q.append_next(&[make_track(10), make_track(11)]);
        assert_eq!(first, 2);
        // old indices >= 2 moved up by 2; the new ones follow the current song
        assert_eq!(*q.random_order.read(), Some(vec![1, 2, 3, 5, 0, 4]));
        assert_invariant(&q);
    }

    #[test]
    fn append_next_without_a_current_song_appends_to_the_order() {
        let q = make_queue((0..2).map(make_track).collect(), None);
        *q.random_order.write() = Some(vec![1, 0]);
        assert_eq!(q.append_next(&[make_track(10)]), 2);
        assert_eq!(*q.random_order.read(), Some(vec![1, 0, 2]));
    }

    #[test]
    fn shift_keeps_the_current_song_when_it_lies_between() {
        let q = make_queue((0..5).map(make_track).collect(), Some(2));
        q.shift(0, 4);
        assert_eq!(q.get_current_index(), Some(1));
        assert_eq!(track_id(&q.get_current().unwrap()), "id_2");
        q.shift(4, 0);
        assert_eq!(track_id(&q.get_current().unwrap()), "id_2");
    }

    #[test]
    fn shift_to_the_same_or_a_missing_row_does_nothing() {
        let q = make_queue((0..3).map(make_track).collect(), Some(1));
        let ids = q.track_ids();
        q.shift(1, 1);
        q.shift(0, 3);
        q.shift(7, 0);
        assert_eq!(q.track_ids(), ids);
        assert_eq!(q.get_current_index(), Some(1));
    }

    #[test]
    fn removing_the_last_playing_song_wraps_with_repeat_playlist() {
        let q = make_queue((0..3).map(make_track).collect(), Some(2));
        q.set_repeat(RepeatSetting::RepeatPlaylist);
        q.remove(2);
        assert_eq!(q.get_current_index(), Some(0));
        assert_invariant(&q);
    }

    #[test]
    fn removing_the_last_playing_song_stops_without_repeat() {
        let q = make_queue((0..3).map(make_track).collect(), Some(2));
        q.remove(2);
        assert_eq!(q.get_current_index(), None);
    }

    #[test]
    fn snapshot_is_one_consistent_state() {
        let q = make_queue((0..3).map(make_track).collect(), Some(1));
        q.set_shuffle(true);
        let s = q.snapshot();
        assert_eq!(s.queue.len(), 3);
        assert_eq!(s.current_track, Some(1));
        let mut order = s.random_order.unwrap();
        order.sort_unstable();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn removing_the_last_playing_song_wraps_in_shuffle_order() {
        let q = make_queue((0..4).map(make_track).collect(), Some(3));
        q.set_shuffle(true);
        q.set_repeat(RepeatSetting::RepeatPlaylist);
        q.remove(3);
        assert_invariant(&q);
        let order = q.get_random_order().unwrap();
        assert_eq!(q.get_current_index(), Some(order[0]));
    }

    #[test]
    fn removing_the_playing_song_in_shuffle_keeps_a_valid_order() {
        let q = make_queue((0..4).map(make_track).collect(), Some(3));
        q.set_shuffle(true);
        q.remove(3);
        assert_eq!(q.get_current_index(), None);
        assert_invariant(&q);
    }

    #[test]
    fn emptying_the_queue_in_shuffle_leaves_an_empty_order() {
        let q = make_queue(vec![make_track(0)], Some(0));
        q.set_shuffle(true);
        q.remove(0);
        assert_invariant(&q);
        assert_eq!(q.get_current_index(), None);
    }

    #[test]
    fn getters_do_not_panic_on_a_cleared_shuffled_queue() {
        let q = make_queue((0..3).map(make_track).collect(), Some(1));
        q.set_shuffle(true);
        q.clear();
        assert_eq!(q.next_index(), None);
        assert_eq!(q.previous_index(), None);
        assert!(q.get_current().is_none());
        q.handle_event(QueueEvent::PreloadTrackRequest);
    }

    #[test]
    fn getters_return_none_for_a_current_song_missing_from_the_order() {
        let q = make_queue((0..3).map(make_track).collect(), Some(2));
        *q.random_order.write() = Some(vec![0, 1]);
        assert_eq!(q.next_index(), None);
        assert_eq!(q.previous_index(), None);
    }

    #[test]
    fn get_current_returns_none_past_the_end() {
        let q = make_queue((0..2).map(make_track).collect(), Some(5));
        assert!(q.get_current().is_none());
    }
}
