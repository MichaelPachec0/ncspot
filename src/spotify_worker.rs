use crate::config;
use crate::events::{Event, EventManager};
use crate::model::playable::Playable;
use crate::pending_load::{Action, PLAY_STALL_WARN_AFTER, PendingTracker};
use crate::queue::QueueEvent;
use crate::spotify::{PlayerEvent, Spotify};
use futures::future::BoxFuture;
use librespot_core::SpotifyUri;
use librespot_core::authentication::Credentials;
use librespot_core::session::Session;
use librespot_playback::mixer::Mixer;
use librespot_playback::player::{Player, PlayerEvent as LibrespotPlayerEvent};
use log::{debug, error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tokio::sync::mpsc;
use tokio::time;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[derive(Debug)]
pub(crate) enum WorkerCommand {
    Load(Playable, bool, u32),
    Play,
    Pause,
    Stop,
    Seek(u32),
    SetVolume(u16),
    Preload(Playable),
    Shutdown,
}

#[derive(Debug)]
enum PlayerStatus {
    Playing,
    Paused,
    Stopped,
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Wait before reconnect attempt `failed_attempts + 1`: 1, 1, 2, 4, 8, 16, then 30 s.
fn reconnect_delay(failed_attempts: u32) -> Duration {
    Duration::from_secs(
        2u64.saturating_pow(failed_attempts.saturating_sub(1))
            .min(30),
    )
}

/// Heartbeat log cadence, measured in UI-refresh ticks. The tick interval is 400ms, so 150 ticks
/// is roughly one minute.
const HEARTBEAT_EVERY_TICKS: u64 = 150;

pub struct Worker {
    events: EventManager,
    player_events: UnboundedReceiverStream<LibrespotPlayerEvent>,
    commands: UnboundedReceiverStream<WorkerCommand>,
    session: Session,
    player: Arc<Player>,
    player_status: PlayerStatus,
    mixer: Arc<dyn Mixer>,
    /// The load and preload the player has not confirmed yet.
    pending: PendingTracker<SpotifyUri>,
    cfg: Arc<config::Config>,
    credentials: Credentials,
    /// Diagnostics: counts UI-refresh ticks for throttled heartbeat logging.
    heartbeat_ticks: u64,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        events: EventManager,
        player_events: mpsc::UnboundedReceiver<LibrespotPlayerEvent>,
        commands: mpsc::UnboundedReceiver<WorkerCommand>,
        session: Session,
        player: Arc<Player>,
        mixer: Arc<dyn Mixer>,
        cfg: Arc<config::Config>,
        credentials: Credentials,
    ) -> Self {
        Self {
            events,
            player_events: UnboundedReceiverStream::new(player_events),
            commands: UnboundedReceiverStream::new(commands),
            player,
            session,
            player_status: PlayerStatus::Stopped,
            mixer,
            pending: PendingTracker::default(),
            cfg,
            credentials,
            heartbeat_ticks: 0,
        }
    }

    pub async fn run_loop(&mut self) {
        let mut ui_refresh = time::interval(Duration::from_millis(400));
        let mut reconnect: Option<BoxFuture<'static, Result<Session, librespot_core::Error>>> =
            None;
        let mut failed_attempts = 0;

        loop {
            if self.session.is_invalid() && reconnect.is_none() {
                reconnect = Some(self.start_reconnect(reconnect_delay(failed_attempts)));
            }

            tokio::select! {
                result = async { reconnect.as_mut().unwrap().await }, if reconnect.is_some() => {
                    reconnect = None;
                    match result {
                        Ok(session) => {
                            info!("Librespot session reconnected");
                            self.player.set_session(session.clone());
                            self.session = session;
                            failed_attempts = 0;
                            let actions = self.pending.on_reconnected(SystemTime::now());
                            self.run_actions(actions);
                        }
                        Err(e) => {
                            failed_attempts += 1;
                            error!("Reconnecting failed: {e}");
                        }
                    }
                },
                cmd = self.commands.next() => match cmd {
                    Some(WorkerCommand::Load(playable, start_playing, position_ms)) => {
                        match SpotifyUri::from_uri(&playable.uri()) {
                            Ok(uri) => {
                                info!(
                                    "player loading track: {uri:?} (start_playing={start_playing}, session.is_invalid()={})",
                                    self.session.is_invalid()
                                );
                                if !uri.is_playable() {
                                    warn!("track is not playable");
                                    self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                                    self.pending.on_stop();
                                } else {
                                    self.player.load(uri.clone(), start_playing, position_ms);
                                    self.pending.on_load(uri, start_playing, position_ms, SystemTime::now());
                                }
                            }
                            Err(e) => {
                                error!("error parsing uri: {e:?}");
                                self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                                self.pending.on_stop();
                            }
                        }
                    }
                    Some(WorkerCommand::Play) => {
                        let action = self.pending.on_play(!self.session.is_invalid(), SystemTime::now());
                        self.run_actions(action.into_iter().collect());
                        self.player.play();
                    }
                    Some(WorkerCommand::Pause) => {
                        self.pending.on_pause();
                        self.player.pause();
                    }
                    Some(WorkerCommand::Stop) => {
                        self.pending.on_stop();
                        self.player.stop();
                    }
                    Some(WorkerCommand::Seek(pos)) => {
                        self.pending.on_seek(pos);
                        self.player.seek(pos);
                    }
                    Some(WorkerCommand::SetVolume(volume)) => {
                        self.mixer.set_volume(volume);
                    }
                    Some(WorkerCommand::Preload(playable)) => {
                        if let Ok(uri) = SpotifyUri::from_uri(&playable.uri()) {
                            debug!("Preloading {uri:?}");
                            self.player.preload(uri.clone());
                            self.pending.on_preload(uri);
                        }
                    }
                    Some(WorkerCommand::Shutdown) => {
                        info!("Librespot session shut down, terminating worker");
                        self.player.stop();
                        self.session.shutdown();
                        self.pending.on_stop();
                        self.events.send(Event::Player(PlayerEvent::Stopped));
                        break;
                    }
                    None => info!("empty stream")
                },
                event = self.player_events.next() => match event {
                    Some(LibrespotPlayerEvent::Playing {
                        play_request_id: _,
                        track_id,
                        position_ms,
                    }) => {
                        let position = Duration::from_millis(position_ms as u64);
                        let playback_start = SystemTime::now() - position;
                        self.events
                            .send(Event::Player(PlayerEvent::Playing(playback_start)));
                        self.player_status = PlayerStatus::Playing;
                        self.pending.on_started(&track_id);
                    }
                    Some(LibrespotPlayerEvent::Paused {
                        play_request_id: _,
                        track_id,
                        position_ms,
                    }) => {
                        let position = Duration::from_millis(position_ms as u64);
                        self.events
                            .send(Event::Player(PlayerEvent::Paused(position)));
                        self.player_status = PlayerStatus::Paused;
                        self.pending.on_started(&track_id);
                    }
                    Some(LibrespotPlayerEvent::Stopped { .. }) => {
                        // No tracker change: a dropped session also stops the player.
                        self.events.send(Event::Player(PlayerEvent::Stopped));
                        self.player_status = PlayerStatus::Stopped;
                    }
                    Some(LibrespotPlayerEvent::EndOfTrack { .. }) => {
                        self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                        self.pending.on_end_of_track();
                    }
                    Some(LibrespotPlayerEvent::Unavailable { track_id, .. }) => {
                        let action = self.pending.on_unavailable(&track_id, self.session.is_invalid());
                        self.run_actions(action.into_iter().collect());
                    }
                    Some(LibrespotPlayerEvent::TimeToPreloadNextTrack { .. }) => {
                        self.events
                            .send(Event::Queue(QueueEvent::PreloadTrackRequest));
                    }
                    Some(LibrespotPlayerEvent::Seeked { play_request_id: _, track_id: _, position_ms}) => {
                        let position = Duration::from_millis(position_ms as u64);
                        let event = match self.player_status {
                            PlayerStatus::Playing => {
                                let playback_start = SystemTime::now() - position;
                                PlayerEvent::Playing(playback_start)
                            },
                            PlayerStatus::Paused => PlayerEvent::Paused(position),
                            PlayerStatus::Stopped => PlayerEvent::Stopped,
                        };
                        self.events.send(Event::Player(event));
                    }
                    Some(event) => {
                        debug!("Unhandled player event: {event:?}");
                    }
                    None => {
                        warn!("Librespot player event channel died, terminating worker");
                        break
                    },
                },
                // Update animated parts of the UI (e.g. statusbar during playback).
                _ = ui_refresh.tick() => {
                    if !matches!(self.player_status, PlayerStatus::Stopped) {
                        self.events.trigger();
                    }

                    // Stall watchdog: a play request with no `Playing` event after a while is
                    // the signature of a stale session whose connection dropped without
                    // `is_invalid()` flipping to true.
                    let actions = self.pending.stall_check(SystemTime::now(), !self.session.is_invalid());
                    self.run_actions(actions);

                    // Heartbeat: prove the worker loop is still alive and report the session's
                    // self-reported validity, so a hang here is distinguishable from a dead loop.
                    self.heartbeat_ticks = self.heartbeat_ticks.wrapping_add(1);
                    if self.heartbeat_ticks.is_multiple_of(HEARTBEAT_EVERY_TICKS) {
                        debug!(
                            "worker heartbeat: player_status={:?}, session.is_invalid()={}, pending_play={}",
                            self.player_status,
                            self.session.is_invalid(),
                            self.pending.pending_uri().is_some(),
                        );
                    }
                },
            }
        }
    }

    /// Build a new session the way the first connect did and connect it after `delay`.
    fn start_reconnect(
        &self,
        delay: Duration,
    ) -> BoxFuture<'static, Result<Session, librespot_core::Error>> {
        warn!("Librespot session invalidated, reconnecting in {delay:?}");
        let cache = self.session.cache();
        // A login's access token expires, the stored credentials don't.
        let credentials = cache
            .and_then(|cache| cache.credentials())
            .unwrap_or_else(|| self.credentials.clone());
        let session = Session::new(
            Spotify::session_config(&self.cfg),
            cache.map(|cache| cache.as_ref().clone()),
        );
        Box::pin(async move {
            time::sleep(delay).await;
            time::timeout(CONNECT_TIMEOUT, session.connect(credentials, true))
                .await
                .unwrap_or_else(|e| Err(librespot_core::Error::deadline_exceeded(e)))
                .map(|_| session)
        })
    }

    fn run_actions(&mut self, actions: Vec<Action<SpotifyUri>>) {
        for action in actions {
            match action {
                Action::Load {
                    uri,
                    start_playing,
                    position_ms,
                } => {
                    warn!("loading {uri:?} again");
                    self.player.load(uri, start_playing, position_ms);
                }
                Action::Preload(uri) => self.player.preload(uri),
                Action::HoldPaused { position_ms } => {
                    warn!("unable to load the current track, holding it");
                    let position = Duration::from_millis(u64::from(position_ms));
                    self.events
                        .send(Event::Player(PlayerEvent::Paused(position)));
                    self.player_status = PlayerStatus::Paused;
                }
                Action::WarnStall => warn!(
                    "playback has NOT started {}s after loading {:?}; session.is_invalid()={}, player_status={:?}. \
                     librespot likely has a stale session (connection dropped without invalidating).",
                    PLAY_STALL_WARN_AFTER.as_secs(),
                    self.pending.pending_uri(),
                    self.session.is_invalid(),
                    self.player_status,
                ),
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        debug!("Worker thread is shutting down, stopping player");
        self.player.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::reconnect_delay;
    use std::time::Duration;

    #[test]
    fn reconnect_delay_doubles_up_to_half_a_minute() {
        let delays: Vec<_> = (0..=7).map(reconnect_delay).collect();
        let expected: Vec<_> = [1, 1, 2, 4, 8, 16, 30, 30].map(Duration::from_secs).into();
        assert_eq!(delays, expected);
    }
}
