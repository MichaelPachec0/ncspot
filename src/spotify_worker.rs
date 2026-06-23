use crate::events::{Event, EventManager};
use crate::model::playable::Playable;
use crate::queue::QueueEvent;
use crate::spotify::PlayerEvent;
use librespot_core::SpotifyUri;
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

/// How long after a play request to warn if playback still hasn't started. Used to surface a
/// librespot session that accepted the load but never produced a `Playing` event (e.g. a stale
/// connection that didn't report itself invalid).
const PLAY_STALL_WARN_AFTER: Duration = Duration::from_secs(5);

/// Heartbeat log cadence, measured in UI-refresh ticks. The tick interval is 400ms, so 150 ticks
/// is roughly one minute.
const HEARTBEAT_EVERY_TICKS: u64 = 150;

/// Diagnostic record of a play request whose playback hasn't started yet. Used by the worker's
/// stall watchdog to detect silently failing loads.
struct PendingPlay {
    /// When the load command was handed to librespot.
    since: SystemTime,
    /// The URI of the track we asked librespot to play.
    uri: String,
    /// Whether the stall warning has already been emitted for this load.
    warned: bool,
}

pub struct Worker {
    events: EventManager,
    player_events: UnboundedReceiverStream<LibrespotPlayerEvent>,
    commands: UnboundedReceiverStream<WorkerCommand>,
    session: Session,
    player: Arc<Player>,
    player_status: PlayerStatus,
    mixer: Arc<dyn Mixer>,
    /// Diagnostics: a play request that hasn't produced a `Playing` event yet, used to detect a
    /// silently stalled librespot session.
    pending_play: Option<PendingPlay>,
    /// Diagnostics: counts UI-refresh ticks for throttled heartbeat logging.
    heartbeat_ticks: u64,
}

impl Worker {
    pub(crate) fn new(
        events: EventManager,
        player_events: mpsc::UnboundedReceiver<LibrespotPlayerEvent>,
        commands: mpsc::UnboundedReceiver<WorkerCommand>,
        session: Session,
        player: Arc<Player>,
        mixer: Arc<dyn Mixer>,
    ) -> Self {
        Self {
            events,
            player_events: UnboundedReceiverStream::new(player_events),
            commands: UnboundedReceiverStream::new(commands),
            player,
            session,
            player_status: PlayerStatus::Stopped,
            mixer,
            pending_play: None,
            heartbeat_ticks: 0,
        }
    }

    pub async fn run_loop(&mut self) {
        let mut ui_refresh = time::interval(Duration::from_millis(400));

        loop {
            if self.session.is_invalid() {
                info!("Librespot session invalidated, terminating worker");
                self.events.send(Event::Player(PlayerEvent::Stopped));
                break;
            }

            tokio::select! {
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
                                    self.pending_play = None;
                                } else {
                                    self.player.load(uri, start_playing, position_ms);
                                    // Arm the stall watchdog so we notice if this load never
                                    // produces a `Playing` event (see the ui_refresh tick arm).
                                    self.pending_play = start_playing.then(|| PendingPlay {
                                        since: SystemTime::now(),
                                        uri: playable.uri(),
                                        warned: false,
                                    });
                                }
                            }
                            Err(e) => {
                                error!("error parsing uri: {e:?}");
                                self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                                self.pending_play = None;
                            }
                        }
                    }
                    Some(WorkerCommand::Play) => {
                        self.player.play();
                    }
                    Some(WorkerCommand::Pause) => {
                        self.player.pause();
                        self.pending_play = None;
                    }
                    Some(WorkerCommand::Stop) => {
                        self.player.stop();
                        self.pending_play = None;
                    }
                    Some(WorkerCommand::Seek(pos)) => {
                        self.player.seek(pos);
                    }
                    Some(WorkerCommand::SetVolume(volume)) => {
                        self.mixer.set_volume(volume);
                    }
                    Some(WorkerCommand::Preload(playable)) => {
                        if let Ok(uri) = SpotifyUri::from_uri(&playable.uri()) {
                            debug!("Preloading {uri:?}");
                            self.player.preload(uri);
                        }
                    }
                    Some(WorkerCommand::Shutdown) => {
                        self.player.stop();
                        self.session.shutdown();
                        self.pending_play = None;
                    }
                    None => info!("empty stream")
                },
                event = self.player_events.next() => match event {
                    Some(LibrespotPlayerEvent::Playing {
                        play_request_id: _,
                        track_id: _,
                        position_ms,
                    }) => {
                        let position = Duration::from_millis(position_ms as u64);
                        let playback_start = SystemTime::now() - position;
                        self.events
                            .send(Event::Player(PlayerEvent::Playing(playback_start)));
                        self.player_status = PlayerStatus::Playing;
                        // Playback started: the load succeeded, disarm the stall watchdog.
                        self.pending_play = None;
                    }
                    Some(LibrespotPlayerEvent::Paused {
                        play_request_id: _,
                        track_id: _,
                        position_ms,
                    }) => {
                        let position = Duration::from_millis(position_ms as u64);
                        self.events
                            .send(Event::Player(PlayerEvent::Paused(position)));
                        self.player_status = PlayerStatus::Paused;
                        self.pending_play = None;
                    }
                    Some(LibrespotPlayerEvent::Stopped { .. }) => {
                        self.events.send(Event::Player(PlayerEvent::Stopped));
                        self.player_status = PlayerStatus::Stopped;
                        self.pending_play = None;
                    }
                    Some(LibrespotPlayerEvent::EndOfTrack { .. }) => {
                        self.events.send(Event::Player(PlayerEvent::FinishedTrack));
                        self.pending_play = None;
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

                    // Stall watchdog: if a play request was issued but no `Playing` event has
                    // arrived after a while, librespot accepted the load but never started
                    // playing. This is the signature of a stale session whose connection dropped
                    // without `is_invalid()` flipping to true.
                    if let Some(pending) = self.pending_play.as_mut()
                        && !pending.warned
                        && pending.since.elapsed().unwrap_or_default() >= PLAY_STALL_WARN_AFTER
                    {
                        warn!(
                            "playback has NOT started {}s after loading {}; \
                             session.is_invalid()={}, player_status={:?}. \
                             librespot likely has a stale session (connection dropped without invalidating).",
                            PLAY_STALL_WARN_AFTER.as_secs(),
                            pending.uri,
                            self.session.is_invalid(),
                            self.player_status,
                        );
                        pending.warned = true;
                    }

                    // Heartbeat: prove the worker loop is still alive and report the session's
                    // self-reported validity, so a hang here is distinguishable from a dead loop.
                    self.heartbeat_ticks = self.heartbeat_ticks.wrapping_add(1);
                    if self.heartbeat_ticks.is_multiple_of(HEARTBEAT_EVERY_TICKS) {
                        debug!(
                            "worker heartbeat: player_status={:?}, session.is_invalid()={}, pending_play={}",
                            self.player_status,
                            self.session.is_invalid(),
                            self.pending_play.is_some(),
                        );
                    }
                },
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
