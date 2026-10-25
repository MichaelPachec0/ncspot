//! What the playback worker still owes the player: the load that has not started yet and the
//! preload, with the retry and stall rules around them. Pure, so it is tested without
//! librespot; the worker executes the returned actions.

use std::time::{Duration, SystemTime};

/// How long after a play request to warn if playback still has not started (a stale session
/// that accepted the load but never produced `Playing`).
pub const PLAY_STALL_WARN_AFTER: Duration = Duration::from_secs(5);

/// What the worker should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action<U> {
    Load {
        uri: U,
        start_playing: bool,
        position_ms: u32,
    },
    Preload(U),
    /// Give up on the pending load for now and show it paused at this position.
    HoldPaused {
        position_ms: u32,
    },
    /// Log the stall warning.
    WarnStall,
}

#[derive(Debug, Clone)]
struct PendingLoad<U> {
    uri: U,
    start_playing: bool,
    position_ms: u32,
    /// When the load (or the play request that made it a playing load) was issued.
    since: SystemTime,
    warned: bool,
    /// A load of this uri already failed once.
    failed: bool,
}

#[derive(Debug, Clone)]
struct Preload<U> {
    uri: U,
    retried: bool,
}

/// The pending load and preload. `U` is the track uri type (`SpotifyUri` in the worker).
#[derive(Debug, Clone)]
pub struct PendingTracker<U> {
    load: Option<PendingLoad<U>>,
    preload: Option<Preload<U>>,
}

impl<U> Default for PendingTracker<U> {
    fn default() -> Self {
        Self {
            load: None,
            preload: None,
        }
    }
}

impl<U: Clone + PartialEq> PendingTracker<U> {
    /// A new load replaces whatever was pending, preload included.
    pub fn on_load(&mut self, uri: U, start_playing: bool, position_ms: u32, now: SystemTime) {
        self.preload = None;
        self.load = Some(PendingLoad {
            uri,
            start_playing,
            position_ms,
            since: now,
            warned: false,
            failed: false,
        });
    }

    /// Play: the pending load should start playing; a load that failed is reloaded when the
    /// session is usable.
    pub fn on_play(&mut self, session_valid: bool, now: SystemTime) -> Option<Action<U>> {
        let load = self.load.as_mut()?;
        if !load.start_playing {
            load.start_playing = true;
            load.since = now;
        }
        (load.failed && session_valid).then(|| Action::Load {
            uri: load.uri.clone(),
            start_playing: true,
            position_ms: load.position_ms,
        })
    }

    pub fn on_pause(&mut self) {
        if let Some(load) = self.load.as_mut() {
            load.start_playing = false;
        }
    }

    pub fn on_seek(&mut self, position_ms: u32) {
        if let Some(load) = self.load.as_mut() {
            load.position_ms = position_ms;
        }
    }

    pub fn on_stop(&mut self) {
        self.load = None;
        self.preload = None;
    }

    pub fn on_preload(&mut self, uri: U) {
        self.preload = Some(Preload {
            uri,
            retried: false,
        });
    }

    /// `Playing` or `Paused` for `track`: its load is done.
    pub fn on_started(&mut self, track: &U) {
        if self.load.as_ref().is_some_and(|load| load.uri == *track) {
            self.load = None;
        }
    }

    pub fn on_end_of_track(&mut self) {
        self.load = None;
    }

    /// The player could not load `track`.
    pub fn on_unavailable(&mut self, track: &U, session_invalid: bool) -> Option<Action<U>> {
        if let Some(load) = self.load.as_mut().filter(|load| load.uri == *track) {
            let action = if load.failed || session_invalid {
                Action::HoldPaused {
                    position_ms: load.position_ms,
                }
            } else {
                Action::Load {
                    uri: load.uri.clone(),
                    start_playing: load.start_playing,
                    position_ms: load.position_ms,
                }
            };
            load.failed = true;
            return Some(action);
        }
        let preload = self.preload.as_mut().filter(|p| p.uri == *track)?;
        if preload.retried || session_invalid {
            return None;
        }
        preload.retried = true;
        Some(Action::Preload(preload.uri.clone()))
    }

    /// A new session is in the player: re-issue what is pending and reset the retry state.
    pub fn on_reconnected(&mut self, now: SystemTime) -> Vec<Action<U>> {
        let mut actions = Vec::new();
        if let Some(load) = self.load.as_mut() {
            load.failed = false;
            load.warned = false;
            load.since = now;
            actions.push(Action::Load {
                uri: load.uri.clone(),
                start_playing: load.start_playing,
                position_ms: load.position_ms,
            });
        }
        if let Some(preload) = self.preload.as_mut() {
            preload.retried = false;
            actions.push(Action::Preload(preload.uri.clone()));
        }
        actions
    }

    /// The stall rules for a playing load that has not started: warn once after
    /// [PLAY_STALL_WARN_AFTER].
    pub fn stall_check(&mut self, now: SystemTime, _session_valid: bool) -> Vec<Action<U>> {
        let mut actions = Vec::new();
        let Some(load) = self.load.as_mut().filter(|load| load.start_playing) else {
            return actions;
        };
        let waited = now.duration_since(load.since).unwrap_or_default();
        if !load.warned && waited >= PLAY_STALL_WARN_AFTER {
            load.warned = true;
            actions.push(Action::WarnStall);
        }
        actions
    }

    pub fn pending_uri(&self) -> Option<&U> {
        self.load.as_ref().map(|load| &load.uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000)
    }

    fn load(uri: &str, start: bool, pos: u32) -> Action<String> {
        Action::Load {
            uri: uri.to_string(),
            start_playing: start,
            position_ms: pos,
        }
    }

    #[test]
    fn seek_and_pause_follow_the_pending_load() {
        let mut t = PendingTracker::default();
        t.on_load("a".to_string(), true, 0, t0());
        t.on_seek(5_000);
        t.on_pause();
        assert_eq!(t.on_reconnected(t0()), vec![load("a", false, 5_000)]);
    }

    #[test]
    fn playing_another_track_keeps_the_pending_load() {
        let mut t = PendingTracker::default();
        t.on_load("a".to_string(), true, 0, t0());
        t.on_started(&"b".to_string());
        assert_eq!(t.pending_uri(), Some(&"a".to_string()));
        t.on_started(&"a".to_string());
        assert_eq!(t.pending_uri(), None);
    }

    #[test]
    fn unavailable_retries_once_then_holds_paused() {
        let mut t = PendingTracker::default();
        let a = "a".to_string();
        t.on_load(a.clone(), true, 7, t0());
        assert_eq!(t.on_unavailable(&a, false), Some(load("a", true, 7)));
        assert_eq!(
            t.on_unavailable(&a, false),
            Some(Action::HoldPaused { position_ms: 7 })
        );
    }

    #[test]
    fn unavailable_while_the_session_is_invalid_holds_at_once() {
        let mut t = PendingTracker::default();
        let a = "a".to_string();
        t.on_load(a.clone(), true, 0, t0());
        assert_eq!(
            t.on_unavailable(&a, true),
            Some(Action::HoldPaused { position_ms: 0 })
        );
    }

    #[test]
    fn play_after_a_failed_load_reloads_it_playing() {
        let mut t = PendingTracker::default();
        let a = "a".to_string();
        t.on_load(a.clone(), false, 3, t0());
        t.on_unavailable(&a, false);
        assert_eq!(t.on_play(true, t0()), Some(load("a", true, 3)));
        assert_eq!(t.on_play(false, t0()), None, "no reload on a dead session");
    }

    #[test]
    fn a_preload_is_retried_once_while_the_session_is_valid() {
        let mut t = PendingTracker::default();
        let b = "b".to_string();
        t.on_preload(b.clone());
        assert_eq!(t.on_unavailable(&b, true), None);
        assert_eq!(
            t.on_unavailable(&b, false),
            Some(Action::Preload(b.clone()))
        );
        assert_eq!(t.on_unavailable(&b, false), None);
    }

    #[test]
    fn reconnect_reissues_load_and_preload_and_resets_retries() {
        let mut t = PendingTracker::default();
        let (a, b) = ("a".to_string(), "b".to_string());
        t.on_load(a.clone(), true, 9, t0());
        t.on_preload(b.clone());
        t.on_unavailable(&a, false);
        t.on_unavailable(&b, false);
        assert_eq!(
            t.on_reconnected(t0()),
            vec![load("a", true, 9), Action::Preload(b.clone())]
        );
        assert_eq!(t.on_unavailable(&a, false), Some(load("a", true, 9)));
        assert_eq!(t.on_unavailable(&b, false), Some(Action::Preload(b)));
    }

    #[test]
    fn stop_and_a_new_load_forget_everything() {
        let mut t = PendingTracker::default();
        t.on_load("a".to_string(), true, 0, t0());
        t.on_preload("b".to_string());
        t.on_stop();
        assert!(t.on_reconnected(t0()).is_empty());
        t.on_preload("b".to_string());
        t.on_load("c".to_string(), true, 0, t0());
        assert_eq!(t.on_reconnected(t0()), vec![load("c", true, 0)]);
    }

    #[test]
    fn end_of_track_clears_the_pending_load() {
        let mut t = PendingTracker::default();
        t.on_load("a".to_string(), true, 0, t0());
        t.on_end_of_track();
        assert_eq!(t.pending_uri(), None);
    }

    #[test]
    fn stall_warning_fires_once_after_five_seconds_for_a_playing_load() {
        let mut t = PendingTracker::default();
        t.on_load("a".to_string(), true, 0, t0());
        assert!(
            t.stall_check(t0() + Duration::from_secs(4), true)
                .is_empty()
        );
        assert_eq!(
            t.stall_check(t0() + Duration::from_secs(5), true),
            vec![Action::WarnStall]
        );
        assert!(
            t.stall_check(t0() + Duration::from_secs(6), true)
                .is_empty()
        );
    }

    #[test]
    fn a_paused_load_never_stalls() {
        let mut t = PendingTracker::default();
        t.on_load("a".to_string(), false, 0, t0());
        assert!(
            t.stall_check(t0() + Duration::from_secs(60), true)
                .is_empty()
        );
    }
}
