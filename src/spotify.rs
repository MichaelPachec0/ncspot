use parking_lot::RwLock;
use std::collections::VecDeque;
use std::error::Error;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use std::{env, fmt};

use futures::FutureExt;
use futures::channel::oneshot;
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_playback::audio_backend;
use librespot_playback::audio_backend::SinkBuilder;
use librespot_playback::config::Bitrate;
use librespot_playback::config::PlayerConfig;
use librespot_playback::mixer::MixerConfig;
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::player::Player;
use log::{debug, error, info, warn};
use tokio::sync::mpsc;
use url::Url;

use crate::application::ASYNC_RUNTIME;
use crate::authentication::SPOTIFY_CLIENT_ID;
use crate::config;
use crate::events::{Event, EventManager};
use crate::model::playable::Playable;
#[cfg(feature = "mpris")]
use crate::mpris::{MprisCommand, MprisManager};
use crate::session_slot::SessionSlot;
use crate::spotify_api::WebApi;
use crate::spotify_worker::{Worker, WorkerCommand, WorkerEnd};
use crate::traits::ListItem;

/// One percent of the maximum supported [Player] volume, used when setting the volume to a certain
/// percent.
pub const VOLUME_PERCENT: u16 = ((u16::MAX as f64) * 1.0 / 100.0) as u16;

/// How long to wait before retrying after a failed session connection. This avoids hammering DNS
/// when the network is briefly unavailable (e.g. just after suspend/resume), while still
/// recovering automatically once connectivity returns.
const SESSION_RETRY_DELAY: Duration = Duration::from_secs(5);

/// How many unexpected worker ends (panic, dead player channel) are restarted within
/// [WORKER_RESTART_WINDOW] before giving up.
const MAX_WORKER_RESTARTS: usize = 3;
/// The window [MAX_WORKER_RESTARTS] counts over.
const WORKER_RESTART_WINDOW: Duration = Duration::from_secs(600);
/// Shown once the worker is no longer restarted.
const WORKER_GAVE_UP_NOTE: &str =
    "Player stopped after repeated failures. Run :reconnect or restart ncspot.";

/// Counts unexpected worker ends so a worker that keeps failing is not restarted forever.
#[derive(Default)]
pub(crate) struct RestartBudget {
    ends: VecDeque<Instant>,
    gave_up: bool,
}

impl RestartBudget {
    /// Count an unexpected end at `now`. False (and gave_up set) once more than
    /// MAX_WORKER_RESTARTS fall within WORKER_RESTART_WINDOW.
    fn charge(&mut self, now: Instant) -> bool {
        while let Some(&end) = self.ends.front() {
            if now.saturating_duration_since(end) >= WORKER_RESTART_WINDOW {
                self.ends.pop_front();
            } else {
                break;
            }
        }
        self.ends.push_back(now);
        let ok = self.ends.len() <= MAX_WORKER_RESTARTS;
        if !ok {
            self.gave_up = true;
        }
        ok
    }

    fn gave_up(&self) -> bool {
        self.gave_up
    }

    fn reset(&mut self) {
        self.ends.clear();
        self.gave_up = false;
    }
}

/// What to do after a worker task ended.
#[derive(Debug, PartialEq)]
enum EndAction {
    Nothing,
    RestartNow,
    RestartAfterDelay,
    GiveUp,
}

/// `end` is Err(()) for a panic, Ok(None) when the session-error path handled itself.
/// `ours` is false for a worker a newer one replaced: it ends silently, without charging the
/// budget, so it can never replace or stop the worker that is running now.
fn end_action(
    end: Result<Option<WorkerEnd>, ()>,
    ours: bool,
    budget: &mut RestartBudget,
    now: Instant,
) -> EndAction {
    if !ours {
        return EndAction::Nothing;
    }
    match end {
        Ok(None) | Ok(Some(WorkerEnd::Orphaned)) => EndAction::Nothing,
        Ok(Some(WorkerEnd::Shutdown)) => EndAction::RestartNow,
        Ok(Some(WorkerEnd::PlayerChannelDied)) | Err(()) => {
            if budget.charge(now) {
                EndAction::RestartAfterDelay
            } else {
                EndAction::GiveUp
            }
        }
    }
}

/// What `:reconnect` does.
#[derive(Debug, PartialEq)]
enum ReconnectAction {
    SendShutdown,
    Nothing,
    StartWorker,
}

/// `has_channel`: a worker is running. Without one and without having given up, a restart is
/// already pending, and starting another worker would leave two alive.
fn reconnect_action(has_channel: bool, gave_up: bool) -> ReconnectAction {
    match (has_channel, gave_up) {
        (true, _) => ReconnectAction::SendShutdown,
        (false, false) => ReconnectAction::Nothing,
        (false, true) => ReconnectAction::StartWorker,
    }
}

type WorkerSender = mpsc::UnboundedSender<WorkerCommand>;

/// If `channel` still holds the sender of the worker that owns `own`, clear it and return
/// true. Check and clear happen under one write guard, so a newer worker's sender stored in
/// between is never cleared.
fn release_if_ours(
    channel: &RwLock<Option<WorkerSender>>,
    own: &mpsc::WeakUnboundedSender<WorkerCommand>,
) -> bool {
    let Some(own) = own.upgrade() else {
        return false;
    };
    let mut slot = channel.write();
    if slot
        .as_ref()
        .is_some_and(|current| current.same_channel(&own))
    {
        *slot = None;
        true
    } else {
        false
    }
}

/// Events sent by the [Player].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum PlayerEvent {
    Playing(SystemTime),
    Paused(Duration),
    Stopped,
    FinishedTrack,
}

/// Wrapper around a worker thread that exposes methods to safely control it.
#[derive(Clone)]
pub struct Spotify {
    events: EventManager,
    /// The credentials for the currently logged in user, used to authenticate to the Spotify API.
    #[cfg(feature = "mpris")]
    mpris: Arc<parking_lot::Mutex<Option<MprisManager>>>,
    credentials: Credentials,
    cfg: Arc<config::Config>,
    /// Playback status of the [Player] owned by the worker thread.
    status: Arc<RwLock<PlayerEvent>>,
    pub api: WebApi,
    /// The amount of the current [Playable] that had elapsed when last paused.
    elapsed: Arc<RwLock<Option<Duration>>>,
    /// The amount of the current [Playable] that has been played in total.
    since: Arc<RwLock<Option<SystemTime>>>,
    /// Channel to send commands to the worker thread.
    channel: Arc<RwLock<Option<mpsc::UnboundedSender<WorkerCommand>>>>,
    /// The active librespot session, set once the worker has successfully connected and cleared
    /// when the worker exits. Callers may clone it cheaply (Session is Arc-backed).
    session: Arc<RwLock<Option<Session>>>,
    /// Keeps `session` current across the worker's reconnects and tracks the reconnect state.
    slot: SessionSlot<Session>,
    /// Caps how often a failing worker is restarted.
    budget: Arc<parking_lot::Mutex<RestartBudget>>,
}

/// The credentials a worker restart uses. A browser login's access token expires after about
/// an hour, the reusable credentials librespot caches after connecting do not, so those win
/// when they are for the same user.
pub(crate) fn restart_credentials(
    saved: Option<Credentials>,
    user: Option<&str>,
    current: Credentials,
) -> Credentials {
    match (saved, user) {
        (Some(saved), Some(user)) if saved.username.as_deref() == Some(user) => saved,
        _ => current,
    }
}

impl Spotify {
    pub fn new(
        events: EventManager,
        credentials: Credentials,
        cfg: Arc<config::Config>,
    ) -> Result<Self, Box<dyn Error>> {
        let session = Arc::new(RwLock::new(None));
        let mut spotify = Self {
            events,
            #[cfg(feature = "mpris")]
            mpris: Default::default(),
            credentials,
            cfg: cfg.clone(),
            status: Arc::new(RwLock::new(PlayerEvent::Stopped)),
            api: WebApi::new(&cfg),
            elapsed: Arc::new(RwLock::new(None)),
            since: Arc::new(RwLock::new(None)),
            channel: Arc::new(RwLock::new(None)),
            slot: SessionSlot::new(session.clone()),
            session,
            budget: Default::default(),
        };
        spotify.api.set_session(spotify.session.clone());

        let (user_tx, user_rx) = oneshot::channel();
        spotify.start_worker(Some(user_tx))?;
        let user = ASYNC_RUNTIME.get().unwrap().block_on(user_rx).ok();
        let saved = Cache::new(Some(config::cache_path("librespot")), None, None, None)
            .ok()
            .and_then(|cache| cache.credentials());
        spotify.credentials =
            restart_credentials(saved, user.as_deref(), spotify.credentials.clone());
        let volume = cfg.state().volume;
        spotify.set_volume(volume, false);

        spotify.api.set_worker_channel(spotify.channel.clone());
        spotify.api.set_events(spotify.events.clone());
        spotify
            .api
            .update_token()
            .map(move |h| ASYNC_RUNTIME.get().unwrap().block_on(h).ok());

        spotify.api.set_user(user);

        Ok(spotify)
    }

    /// Create a disconnected Spotify instance for use in tests. No worker thread is started and
    /// no network connections are made.
    #[cfg(test)]
    pub fn new_for_test(cfg: Arc<config::Config>, events: EventManager) -> Self {
        let mut api = WebApi::new(&cfg);
        let session = Arc::new(RwLock::new(None));
        api.set_session(Arc::clone(&session));
        Self {
            events,
            #[cfg(feature = "mpris")]
            mpris: Default::default(),
            credentials: Credentials::with_password("test_user", "test_pass"),
            cfg,
            status: Arc::new(RwLock::new(PlayerEvent::Stopped)),
            api,
            elapsed: Arc::new(RwLock::new(None)),
            since: Arc::new(RwLock::new(None)),
            channel: Arc::new(RwLock::new(None)),
            slot: SessionSlot::new(session.clone()),
            session,
            budget: Default::default(),
        }
    }

    /// Start the worker thread. If `user_tx` is given, it will receive the username of the logged
    /// in user.
    pub fn start_worker(
        &self,
        user_tx: Option<oneshot::Sender<String>>,
    ) -> Result<(), Box<dyn Error>> {
        let cfg = self.cfg.clone();
        let backend_name = cfg.values().backend.clone();
        // Before the channel is stored: on failure no sender may be left without a worker,
        // or every later command (and `:reconnect`) would go to a dead channel.
        let backend = Self::init_backend(backend_name)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let own = tx.downgrade();
        *self.channel.write() = Some(tx);
        let worker_channel = self.channel.clone();
        let session_handle = self.session.clone();
        let slot = self.slot.clone();
        let events = self.events.clone();
        let volume = self.volume();
        let credentials = self.credentials.clone();
        ASYNC_RUNTIME.get().unwrap().spawn(Self::worker(
            worker_channel,
            session_handle,
            events,
            rx,
            cfg,
            credentials,
            user_tx,
            volume,
            backend,
            slot,
            own,
            self.budget.clone(),
        ));
        Ok(())
    }

    /// Generate the librespot [SessionConfig] used when creating a [Session].
    pub fn session_config(cfg: &config::Config) -> SessionConfig {
        let mut session_config = librespot_core::SessionConfig {
            client_id: SPOTIFY_CLIENT_ID.to_string(),
            ..Default::default()
        };
        match cfg
            .values()
            .proxy
            .clone()
            .map(|proxy| proxy.trim().to_owned())
            .or_else(|| env::var("http_proxy").ok())
        {
            Some(proxy) if !proxy.is_empty() => {
                info!("Setting proxy {proxy}");
                session_config.proxy = Url::parse(&proxy).ok();
            }
            _ => debug!("No proxy set"),
        }
        if let Some(ap_port) = cfg.values().ap_port {
            session_config.ap_port = Some(ap_port)
        }
        session_config
    }

    pub fn test_credentials(
        cfg: &config::Config,
        credentials: Credentials,
    ) -> Result<Session, librespot_core::Error> {
        let config = Self::session_config(cfg);
        let _guard = ASYNC_RUNTIME.get().unwrap().enter();
        let session = Session::new(config, None);
        ASYNC_RUNTIME
            .get()
            .unwrap()
            .block_on(session.connect(credentials, true))
            .map(|_| session)
    }

    /// Create a [Session] that respects the user configuration in `cfg` and with the given
    /// credentials.
    async fn create_session(
        cfg: &config::Config,
        credentials: Credentials,
    ) -> Result<Session, librespot_core::Error> {
        let librespot_cache_path = config::cache_path("librespot");
        let audio_cache_path = match cfg.values().audio_cache {
            Some(false) => None,
            _ => Some(librespot_cache_path.join("files")),
        };
        let cache = Cache::new(
            Some(librespot_cache_path.clone()),
            Some(librespot_cache_path.join("volume")),
            audio_cache_path,
            cfg.values()
                .audio_cache_size
                .map(|size| size as u64 * 1048576),
        )
        .expect("Could not create cache");
        debug!("opening spotify session");
        let session_config = Self::session_config(cfg);
        let session = Session::new(session_config, Some(cache));
        session.connect(credentials, true).await.map(|_| session)
    }

    /// Create and initialize the requested audio backend.
    fn init_backend(desired_backend: Option<String>) -> Result<SinkBuilder, Box<dyn Error>> {
        let backend = if let Some(name) = desired_backend {
            audio_backend::BACKENDS
                .iter()
                .find(|backend| name == backend.0)
                .ok_or(format!(
                    r#"configured audio backend "{name}" can't be found"#
                ))?
        } else {
            audio_backend::BACKENDS
                .first()
                .ok_or("no available audio backends found")?
        };

        let backend_name = backend.0;

        info!("Initializing audio backend {backend_name}");
        if backend_name == "pulseaudio" {
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { env::set_var("PULSE_PROP_application.name", "ncspot") };
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { env::set_var("PULSE_PROP_stream.description", "ncurses Spotify client") };
            // TODO: Audit that the environment access only happens in single-threaded code.
            unsafe { env::set_var("PULSE_PROP_media.role", "music") };
        }

        Ok(backend.1)
    }

    /// Create and run the worker thread.
    #[allow(clippy::too_many_arguments)]
    async fn worker(
        worker_channel: Arc<RwLock<Option<mpsc::UnboundedSender<WorkerCommand>>>>,
        session_handle: Arc<RwLock<Option<Session>>>,
        events: EventManager,
        commands: mpsc::UnboundedReceiver<WorkerCommand>,
        cfg: Arc<config::Config>,
        credentials: Credentials,
        user_tx: Option<oneshot::Sender<String>>,
        volume: u16,
        backend: SinkBuilder,
        slot: SessionSlot<Session>,
        own: mpsc::WeakUnboundedSender<WorkerCommand>,
        budget: Arc<parking_lot::Mutex<RestartBudget>>,
    ) {
        let body = async {
            let bitrate_str = cfg.values().bitrate.unwrap_or(320).to_string();
            let bitrate = Bitrate::from_str(&bitrate_str);
            if bitrate.is_err() {
                error!("invalid bitrate, will use 320 instead")
            }

            let player_config = PlayerConfig {
                gapless: cfg.values().gapless.unwrap_or(true),
                bitrate: bitrate.unwrap_or(Bitrate::Bitrate320),
                normalisation: cfg.values().volnorm.unwrap_or(false),
                normalisation_pregain_db: cfg.values().volnorm_pregain.unwrap_or(0.0),
                ..Default::default()
            };

            let session = match Self::create_session(&cfg, credentials.clone()).await {
                Ok(session) => session,
                Err(e) => {
                    // A transient DNS/network failure here is common right after a dropped
                    // connection, suspend/resume, or a network change, exactly when the worker
                    // gets restarted. Previously this used `.expect()`, so such a hiccup panicked
                    // the worker task *before* it could request another restart, leaving a dangling
                    // command channel behind. The result was that every later playback command was
                    // silently dropped and the app appeared frozen until restarted.
                    //
                    // Instead, recover: back off briefly (so we don't hammer DNS while the network
                    // is down) and ask the application to restart the worker, which retries the
                    // connection.
                    error!("Could not create session: {e}. Retrying in {SESSION_RETRY_DELAY:?}.");

                    // Unblock startup immediately. `Spotify::new` waits on `user_rx` only to learn
                    // the username, so drop the sender now rather than after the back-off; otherwise
                    // the main thread (and the UI) would block for SESSION_RETRY_DELAY on a failed
                    // first connection.
                    drop(user_tx);

                    // Stop accepting commands into a channel whose worker will never drain them, so
                    // they surface as a logged "no channel" error instead of being silently buffered
                    // and then dropped when this task returns.
                    release_if_ours(&worker_channel, &own);

                    // Back off in the background (this task is now detached from startup) before
                    // asking the application to restart the worker. This path restarts itself, so
                    // the end handling below must do nothing more.
                    tokio::time::sleep(SESSION_RETRY_DELAY).await;
                    events.send(Event::SessionDied);
                    return None;
                }
            };
            // Set the handle before unblocking startup: the library's first current_user call runs
            // right after Spotify::new returns and must find the session.
            *session_handle.write() = Some(session.clone());
            user_tx.map(|tx| tx.send(session.username()));

            let mixer_factory_opt = librespot_playback::mixer::find(Some(SoftMixer::NAME));
            let factory = mixer_factory_opt.expect("could not find softvol mixer factory");
            let mixer = factory(MixerConfig::default()).expect("could not create softvol mixer");

            mixer.set_volume(volume);

            let audio_format: librespot_playback::config::AudioFormat = Default::default();
            let backend_cfg = cfg.clone();
            let player = Player::new(
                player_config,
                session.clone(),
                mixer.get_soft_volume(),
                move || (backend)(backend_cfg.values().backend_device.clone(), audio_format),
            );
            let player_events = player.get_player_event_channel();

            let mut worker = Worker::new(
                events.clone(),
                player_events,
                commands,
                session,
                player,
                mixer,
                cfg,
                credentials,
                slot.clone(),
            );
            debug!("worker thread ready.");
            Some(worker.run_loop().await)
        };

        // Its own statement, so the finished future (which owns `user_tx`) is dropped before any
        // delay below; otherwise a startup panic would block `Spotify::new` on `user_rx`.
        let end = std::panic::AssertUnwindSafe(body).catch_unwind().await;
        let end = end.map_err(|payload| {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            error!("worker thread panicked: {msg}");
        });

        // Only clear state that still belongs to this worker: a newer worker may already own it.
        // A worker that is not ours (superseded, or its session-error path already released the
        // slot) ends without touching anything.
        let ours = release_if_ours(&worker_channel, &own);
        if ours {
            *session_handle.write() = None;
            slot.reset();
        }

        let action = end_action(end, ours, &mut budget.lock(), Instant::now());
        match action {
            EndAction::Nothing => {}
            EndAction::RestartNow => {
                info!("worker shut down, restarting");
                events.send(Event::SessionDied);
            }
            EndAction::RestartAfterDelay => {
                error!("worker thread died, restarting in {SESSION_RETRY_DELAY:?}");
                tokio::time::sleep(SESSION_RETRY_DELAY).await;
                events.send(Event::SessionDied);
            }
            EndAction::GiveUp => {
                error!("worker thread died too often, not restarting");
                events.send(Event::StatusNote(WORKER_GAVE_UP_NOTE.into()));
            }
        }
    }

    /// Get the current playback status of the [Player].
    pub fn get_current_status(&self) -> PlayerEvent {
        let status = self.status.read();
        (*status).clone()
    }

    /// Get the total amount of the current [Playable] that has been played.
    pub fn get_current_progress(&self) -> Duration {
        self.get_elapsed().unwrap_or_else(|| Duration::from_secs(0))
            + self
                .get_since()
                .map(|t| t.elapsed().unwrap())
                .unwrap_or_else(|| Duration::from_secs(0))
    }

    fn set_elapsed(&self, new_elapsed: Option<Duration>) {
        let mut elapsed = self.elapsed.write();
        *elapsed = new_elapsed;
    }

    fn get_elapsed(&self) -> Option<Duration> {
        let elapsed = self.elapsed.read();
        *elapsed
    }

    fn set_since(&self, new_since: Option<SystemTime>) {
        let mut since = self.since.write();
        *since = new_since;
    }

    fn get_since(&self) -> Option<SystemTime> {
        let since = self.since.read();
        *since
    }

    /// Load `track` into the [Player]. Start playing immediately if
    /// `start_playing` is true. Start playing from `position_ms` in the song.
    pub fn load(&self, track: &Playable, start_playing: bool, position_ms: u32) {
        info!("loading track: {track:?}");

        if !track.is_playable() {
            warn!("track {track:?} can not be played, skipping..");
            self.events.send(Event::Player(PlayerEvent::FinishedTrack));
            return;
        }

        self.send_worker(WorkerCommand::Load(
            track.clone(),
            start_playing,
            position_ms,
        ));

        #[cfg(feature = "mpris")]
        self.send_mpris(MprisCommand::EmitMetadataStatus);
    }

    /// Update the cached status of the [Player]. This makes sure the status
    /// doesn't have to be retrieved every time from the thread, which would be harder and more
    /// expensive.
    pub fn update_status(&self, new_status: PlayerEvent) {
        match new_status {
            PlayerEvent::Paused(position) => {
                self.set_elapsed(Some(position));
                self.set_since(None);
            }
            PlayerEvent::Playing(playback_start) => {
                self.set_since(Some(playback_start));
                self.set_elapsed(None);
            }
            PlayerEvent::Stopped | PlayerEvent::FinishedTrack => {
                self.set_elapsed(None);
                self.set_since(None);
            }
        }

        let mut status = self.status.write();
        *status = new_status;

        #[cfg(feature = "mpris")]
        self.send_mpris(MprisCommand::EmitPlaybackStatus);
    }

    /// Reset the time tracking stats for the current song. This should be called when a new song is
    /// loaded.
    pub fn update_track(&self) {
        self.set_elapsed(None);
        self.set_since(None);
    }

    /// Start playback of the [Player].
    pub fn play(&self) {
        info!("play()");
        self.send_worker(WorkerCommand::Play);
    }

    /// Toggle playback (play/pause) of the [Player].
    pub fn toggleplayback(&self) {
        match self.get_current_status() {
            PlayerEvent::Playing(_) => self.pause(),
            PlayerEvent::Paused(_) => self.play(),
            _ => (),
        }
    }

    /// Send an [MprisCommand] to the mpris thread.
    #[cfg(feature = "mpris")]
    pub(crate) fn send_mpris(&self, cmd: MprisCommand) {
        debug!("Sending mpris command: {cmd:?}");
        match self.mpris.lock().as_ref() {
            Some(mpris_manager) => {
                mpris_manager.send(cmd);
            }
            _ => {
                warn!("mpris context is unitialized");
            }
        }
    }

    /// Send a [WorkerCommand] to the worker thread.
    fn send_worker(&self, cmd: WorkerCommand) {
        info!("sending command to worker: {cmd:?}");
        let channel = self.channel.read();
        match channel.as_ref() {
            Some(channel) => {
                if let Err(e) = channel.send(cmd) {
                    error!("can't send command to spotify worker: {e}, dropping command");
                }
            }
            None => error!("no channel to worker available"),
        }
    }

    /// Pause playback of the [Player].
    pub fn pause(&self) {
        info!("pause()");
        self.send_worker(WorkerCommand::Pause);
    }

    /// Stop playback of the [Player].
    pub fn stop(&self) {
        info!("stop()");
        self.send_worker(WorkerCommand::Stop);
    }

    /// Seek in the currently played [Playable] played by the [Player].
    pub fn seek(&self, position_ms: u32) {
        self.send_worker(WorkerCommand::Seek(position_ms));
        #[cfg(feature = "mpris")]
        self.notify_seeked(position_ms);
    }

    /// Seek relatively to the current playback position of the [Player].
    pub fn seek_relative(&self, delta: i32) {
        let progress = self.get_current_progress();
        let new = (progress.as_secs() * 1000) as i32 + progress.subsec_millis() as i32 + delta;
        self.seek(std::cmp::max(0, new) as u32);
    }

    /// Get the current volume of the [Player].
    pub fn volume(&self) -> u16 {
        self.cfg.state().volume
    }

    /// Send a Seeked signal on Mpris interface
    #[cfg(feature = "mpris")]
    pub fn notify_seeked(&self, position_ms: u32) {
        let new_position = Duration::from_millis(position_ms.into());
        let command = MprisCommand::EmitSeekedStatus(
            new_position
                .as_micros()
                .try_into()
                .expect("track position exceeds MPRIS datatype"),
        );
        self.send_mpris(command);
    }

    /// Set the current volume of the [Player]. If `notify` is true, also notify MPRIS clients about
    /// the update.
    pub fn set_volume(&self, volume: u16, notify: bool) {
        info!("setting volume to {volume}");
        self.cfg.with_state_mut(|s| s.volume = volume);
        self.send_worker(WorkerCommand::SetVolume(volume));
        // HACK: This is a bit of a hack to prevent duplicate update signals when updating from the
        // MPRIS implementation.
        if notify {
            #[cfg(feature = "mpris")]
            self.send_mpris(MprisCommand::EmitVolumeStatus)
        }
    }

    /// Preload the given [Playable] in the [Player]. This makes sure it can be played immediately
    /// after the current [Playable] is finished.
    pub fn preload(&self, track: &Playable) {
        self.send_worker(WorkerCommand::Preload(track.clone()));
    }

    /// Shut down the worker thread.
    pub fn shutdown(&self) {
        self.send_worker(WorkerCommand::Shutdown);
    }

    /// Restart the player: shut the running worker down (its end restarts it at once), or
    /// start a new one after the restart budget gave up. Does nothing while a restart is
    /// already pending, so two workers are never alive at once.
    pub fn reconnect(&self) {
        let has_channel = self.channel.read().is_some();
        // Held across the decision and the start, so a new worker that ends at once charges
        // the budget only after it was reset. `start_worker` is sync and never takes it.
        let mut budget = self.budget.lock();
        match reconnect_action(has_channel, budget.gave_up()) {
            ReconnectAction::SendShutdown => {
                drop(budget);
                self.shutdown();
            }
            ReconnectAction::Nothing => info!("reconnect: a restart is already pending"),
            ReconnectAction::StartWorker => match self.start_worker(None) {
                Ok(()) => budget.reset(),
                Err(e) => {
                    drop(budget);
                    self.events.send(Event::StatusNote(format!(
                        "Could not restart the player: {e}"
                    )));
                }
            },
        }
    }

    #[cfg(feature = "mpris")]
    pub fn set_mpris(&mut self, mpris: MprisManager) {
        *self.mpris.lock() = Some(mpris);
    }

    /// Whether the worker is reconnecting its session.
    pub fn is_reconnecting(&self) -> bool {
        self.slot.is_reconnecting()
    }

    /// Reconnects since startup.
    #[cfg(feature = "harness")]
    pub fn reconnects(&self) -> u64 {
        self.slot.reconnects()
    }

    /// Shut the current session down, so the worker sees it invalid and reconnects.
    #[cfg(feature = "harness")]
    pub fn drop_session(&self) {
        if let Some(session) = self.session.read().clone() {
            session.shutdown();
        }
    }

    /// Whether the slot holds a session that is not invalid.
    #[cfg(feature = "harness")]
    pub fn session_valid(&self) -> bool {
        self.session
            .read()
            .as_ref()
            .is_some_and(|s| !s.is_invalid())
    }

    /// Return a clone of the current librespot [Session], or `None` if the worker has not yet
    /// connected (or has exited). [Session] is cheaply cloneable (Arc-backed).
    pub fn session(&self) -> Option<Session> {
        self.session.read().clone()
    }
}

/// A type of Spotify URI.
#[derive(Copy, Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum UriType {
    Album,
    Artist,
    Track,
    Playlist,
    Show,
    Episode,
}

#[derive(Debug)]
pub struct UriParseError;

impl fmt::Display for UriParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid Spotify URI")
    }
}

impl Error for UriParseError {}

impl FromStr for UriType {
    type Err = UriParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with("spotify:album:") {
            Ok(Self::Album)
        } else if s.starts_with("spotify:artist:") {
            Ok(Self::Artist)
        } else if s.starts_with("spotify:track:") {
            Ok(Self::Track)
        } else if s.starts_with("spotify:") && s.contains(":playlist:") {
            Ok(Self::Playlist)
        } else if s.starts_with("spotify:show:") {
            Ok(Self::Show)
        } else if s.starts_with("spotify:episode:") {
            Ok(Self::Episode)
        } else {
            Err(UriParseError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_album_uri() {
        let uri_type = "spotify:album:29F5MF6Q9VYlryDsYEQz6a".parse();
        assert!(matches!(uri_type, Ok(UriType::Album)));
    }

    #[test]
    fn parse_invalid_uri() {
        let uri_type: Result<UriType, _> = "kayava".parse();
        assert!(matches!(uri_type, Err(UriParseError)));
    }

    #[test]

    fn parse_playlist_uri() {
        let uri_type = "spotify:playlist:37i9dQZF1DX36Xw4IJIVKA".parse();
        assert!(matches!(uri_type, Ok(UriType::Playlist)));
    }

    #[test]
    fn budget_allows_three_ends_in_the_window_and_refuses_the_fourth() {
        let t = Instant::now();
        let mut b = RestartBudget::default();
        assert!(b.charge(t));
        assert!(b.charge(t + Duration::from_secs(60)));
        assert!(b.charge(t + Duration::from_secs(120)));
        assert!(!b.charge(t + Duration::from_secs(180)));
        assert!(b.gave_up());
    }

    #[test]
    fn budget_forgets_ends_older_than_the_window() {
        let t = Instant::now();
        let mut b = RestartBudget::default();
        for s in [0, 1, 2] {
            assert!(b.charge(t + Duration::from_secs(s)));
        }
        // the first end is exactly 10 minutes old: it no longer counts
        assert!(b.charge(t + Duration::from_secs(600)));
        assert!(!b.gave_up());
    }

    #[test]
    fn budget_reset_clears_gave_up() {
        let t = Instant::now();
        let mut b = RestartBudget::default();
        for _ in 0..4 {
            b.charge(t);
        }
        b.reset();
        assert!(!b.gave_up());
        assert!(b.charge(t));
    }

    #[test]
    fn end_action_per_kind() {
        let t = Instant::now();
        let mut b = RestartBudget::default();
        assert_eq!(end_action(Ok(None), true, &mut b, t), EndAction::Nothing);
        assert_eq!(
            end_action(Ok(Some(WorkerEnd::Orphaned)), true, &mut b, t),
            EndAction::Nothing
        );
        assert_eq!(
            end_action(Ok(Some(WorkerEnd::Shutdown)), true, &mut b, t),
            EndAction::RestartNow
        );
        // none of the above charged the budget
        assert_eq!(
            end_action(Ok(Some(WorkerEnd::PlayerChannelDied)), true, &mut b, t),
            EndAction::RestartAfterDelay
        );
        assert_eq!(
            end_action(Err(()), true, &mut b, t),
            EndAction::RestartAfterDelay
        );
        assert_eq!(
            end_action(Err(()), true, &mut b, t),
            EndAction::RestartAfterDelay
        );
        assert_eq!(end_action(Err(()), true, &mut b, t), EndAction::GiveUp);
        assert!(b.gave_up());
    }

    #[test]
    fn end_action_of_a_superseded_worker_is_nothing_and_free() {
        let t = Instant::now();
        let mut b = RestartBudget::default();
        for _ in 0..4 {
            assert_eq!(end_action(Err(()), false, &mut b, t), EndAction::Nothing);
            assert_eq!(
                end_action(Ok(Some(WorkerEnd::PlayerChannelDied)), false, &mut b, t),
                EndAction::Nothing
            );
        }
        assert!(!b.gave_up());
        // the budget is untouched: three restarts are still allowed
        for _ in 0..3 {
            assert_eq!(
                end_action(Err(()), true, &mut b, t),
                EndAction::RestartAfterDelay
            );
        }
    }

    #[test]
    fn release_if_ours_clears_our_sender() {
        let (tx, _rx) = mpsc::unbounded_channel::<WorkerCommand>();
        let own = tx.downgrade();
        let channel = RwLock::new(Some(tx));
        assert!(release_if_ours(&channel, &own));
        assert!(channel.read().is_none());
    }

    #[test]
    fn release_if_ours_leaves_a_newer_sender_alone() {
        let (old_tx, _old_rx) = mpsc::unbounded_channel::<WorkerCommand>();
        let own = old_tx.downgrade();
        let (new_tx, _new_rx) = mpsc::unbounded_channel::<WorkerCommand>();
        let channel = RwLock::new(Some(new_tx.clone()));
        assert!(!release_if_ours(&channel, &own));
        assert!(channel.read().as_ref().unwrap().same_channel(&new_tx));
        drop(old_tx);
    }

    #[test]
    fn release_if_ours_on_an_empty_slot_is_false() {
        let (tx, _rx) = mpsc::unbounded_channel::<WorkerCommand>();
        let own = tx.downgrade();
        let channel = RwLock::new(None);
        assert!(!release_if_ours(&channel, &own));
        assert!(channel.read().is_none());
        drop(tx);
    }

    #[test]
    fn release_if_ours_after_our_sender_dropped_is_false() {
        let (tx, _rx) = mpsc::unbounded_channel::<WorkerCommand>();
        let own = tx.downgrade();
        drop(tx);
        let (new_tx, _new_rx) = mpsc::unbounded_channel::<WorkerCommand>();
        let channel = RwLock::new(Some(new_tx));
        assert!(!release_if_ours(&channel, &own));
        assert!(channel.read().is_some());
    }

    #[test]
    fn start_worker_with_a_bad_backend_leaves_no_channel() {
        let cfg = config::Config::new_for_test_with_values(config::ConfigValues {
            backend: Some("no-such-backend".into()),
            ..Default::default()
        });
        let spotify = Spotify::new_for_test(cfg, EventManager::new_for_test());
        assert!(spotify.start_worker(None).is_err());
        assert!(spotify.channel.read().is_none());
    }

    #[test]
    fn reconnect_action_per_state() {
        assert_eq!(reconnect_action(true, false), ReconnectAction::SendShutdown);
        assert_eq!(reconnect_action(true, true), ReconnectAction::SendShutdown);
        // no channel and not given up: a restart is already pending
        assert_eq!(reconnect_action(false, false), ReconnectAction::Nothing);
        assert_eq!(reconnect_action(false, true), ReconnectAction::StartWorker);
    }
}

#[cfg(test)]
mod credential_tests {
    use super::restart_credentials;
    use librespot_core::authentication::Credentials;

    fn creds(user: &str, secret: &str) -> Credentials {
        Credentials::with_password(user, secret)
    }

    #[test]
    fn saved_credentials_of_the_same_user_win() {
        let got = restart_credentials(Some(creds("u1", "saved")), Some("u1"), creds("u1", "token"));
        assert_eq!(got.auth_data, b"saved".to_vec());
    }

    #[test]
    fn saved_credentials_of_another_user_are_ignored() {
        let got = restart_credentials(Some(creds("u2", "saved")), Some("u1"), creds("u1", "token"));
        assert_eq!(got.auth_data, b"token".to_vec());
    }

    #[test]
    fn no_saved_credentials_or_no_user_keep_the_current_ones() {
        let got = restart_credentials(None, Some("u1"), creds("u1", "token"));
        assert_eq!(got.auth_data, b"token".to_vec());
        let got = restart_credentials(Some(creds("u1", "saved")), None, creds("u1", "token"));
        assert_eq!(got.auth_data, b"token".to_vec());
    }
}
