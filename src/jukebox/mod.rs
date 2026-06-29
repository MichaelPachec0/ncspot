pub mod analysis;
pub mod driver;
pub mod graph;
pub mod model;
pub mod remixer;
#[cfg(feature = "jukebox-graphics")]
pub mod render;
pub mod settings;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::events::EventManager;
use crate::jukebox::analysis::AnalysisManager;
use crate::jukebox::driver::{Driver, DriverAction, SystemClock, XorShiftRandom};
use crate::jukebox::graph::{Edge, SongGraph};
use crate::jukebox::remixer::remix;
use crate::jukebox::settings::{JukeboxSettings, PartialJukeboxSettings, SettingsSource};
use crate::model::playable::Playable;
use crate::queue::Queue;
use crate::traits::ListItem;

const TICK_MS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Linear,
    Radial,
    Split,
}

impl ViewMode {
    pub fn next(self) -> Self {
        match self {
            Self::Linear => Self::Radial,
            Self::Radial => Self::Split,
            Self::Split => Self::Linear,
        }
    }
}

#[derive(Clone)]
pub struct SongState {
    pub track_title: String,
    pub graph: Arc<SongGraph>,
    pub current_beat: usize,
    pub beats_played: u64,
    pub jumps: u64,
    pub branch_chance: f64,
    pub listen_time_ms: u64,
    pub last_branch: Option<Edge>,
    pub bouncing: bool,
    pub no_analysis: bool,
    // Read by the Split-panel read-out (wired in Task 4).
    #[allow(dead_code)]
    pub effective: JukeboxSettings,
    #[allow(dead_code)]
    pub source: SettingsSource,
    #[allow(dead_code)]
    pub per_song: Option<PartialJukeboxSettings>,
}

struct JukeboxInner {
    enabled: AtomicBool,
    graphics: AtomicBool,
    bouncing: AtomicBool,
    rebuild: AtomicBool,
    view_mode: RwLock<ViewMode>,
    settings: RwLock<JukeboxSettings>,
    state: RwLock<Option<SongState>>,
    seek_request: Mutex<Option<usize>>,
}

impl JukeboxInner {
    fn new(settings: JukeboxSettings, enabled: bool, graphics: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            graphics: AtomicBool::new(graphics),
            bouncing: AtomicBool::new(false),
            rebuild: AtomicBool::new(false),
            view_mode: RwLock::new(ViewMode::Linear),
            settings: RwLock::new(settings),
            state: RwLock::new(None),
            seek_request: Mutex::new(None),
        }
    }
}

pub struct Jukebox {
    inner: Arc<JukeboxInner>,
}

impl Jukebox {
    pub fn new(
        queue: Arc<Queue>,
        analysis: Arc<AnalysisManager>,
        events: EventManager,
        cfg: Arc<Config>,
    ) -> Arc<Self> {
        let jb_cfg = cfg.values().jukebox.clone().unwrap_or_default();
        // Persisted (modal) tuning settings win over the config file once present; the
        // modal's Reset clears them. The enabled/graphics toggles are intentionally NOT
        // persisted, so the jukebox always starts disabled.
        let settings = cfg
            .state()
            .jukebox
            .clone()
            .unwrap_or_else(|| JukeboxSettings::from_config(&jb_cfg));
        let enabled = jb_cfg.enabled.unwrap_or(false);
        let graphics = jb_cfg.graphics.unwrap_or(true);
        let inner = Arc::new(JukeboxInner::new(settings, enabled, graphics));

        let loop_inner = inner.clone();
        std::thread::Builder::new()
            .name("jukebox-driver".into())
            .spawn(move || run_driver_loop(loop_inner, queue, analysis, events, cfg))
            .expect("failed to spawn jukebox driver thread");

        Arc::new(Self { inner })
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }
    pub fn set_enabled(&self, on: bool) {
        self.inner.enabled.store(on, Ordering::Relaxed);
    }
    pub fn toggle(&self) -> bool {
        let new = !self.is_enabled();
        self.set_enabled(new);
        new
    }
    pub fn toggle_bounce(&self) {
        let new = !self.inner.bouncing.load(Ordering::Relaxed);
        self.inner.bouncing.store(new, Ordering::Relaxed);
    }
    pub fn request_seek_to_beat(&self, beat: usize) {
        *self.inner.seek_request.lock().unwrap() = Some(beat);
    }
    pub fn view_mode(&self) -> ViewMode {
        *self.inner.view_mode.read().unwrap()
    }
    pub fn cycle_view_mode(&self) -> ViewMode {
        let mut g = self.inner.view_mode.write().unwrap();
        *g = g.next();
        *g
    }
    pub fn graphics_enabled(&self) -> bool {
        self.inner.graphics.load(Ordering::Relaxed)
    }
    pub fn toggle_graphics(&self) -> bool {
        let new = !self.graphics_enabled();
        self.inner.graphics.store(new, Ordering::Relaxed);
        new
    }
    pub fn state(&self) -> Option<SongState> {
        self.inner.state.read().unwrap().clone()
    }
    pub fn settings(&self) -> JukeboxSettings {
        self.inner.settings.read().unwrap().clone()
    }
    pub fn apply_settings(&self, settings: JukeboxSettings) {
        *self.inner.settings.write().unwrap() = settings;
        self.request_rebuild();
    }
    /// Force the driver to rebuild the current track on its next tick.
    pub fn request_rebuild(&self) {
        self.inner.rebuild.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn test_instance() -> Self {
        Self {
            inner: Arc::new(JukeboxInner::new(JukeboxSettings::default(), false, false)),
        }
    }
}

/// Resolve the effective settings for `track_id`: the global `base` wins when the override
/// toggle is on or no per-song entry exists; otherwise the per-song override is overlaid.
pub fn resolve_effective(
    base: &JukeboxSettings,
    override_per_song: bool,
    overrides: &HashMap<String, PartialJukeboxSettings>,
    track_id: &str,
) -> (
    JukeboxSettings,
    SettingsSource,
    Option<PartialJukeboxSettings>,
) {
    if override_per_song {
        return (base.clone(), SettingsSource::Global, None);
    }
    match overrides.get(track_id) {
        Some(p) => (p.resolve(base), SettingsSource::PerSong, Some(p.clone())),
        None => (base.clone(), SettingsSource::Global, None),
    }
}

fn publish_no_analysis(inner: &JukeboxInner, events: &EventManager, playable: &Playable) {
    let title = playable.track().map(|t| t.title).unwrap_or_default();
    *inner.state.write().unwrap() = Some(SongState {
        track_title: title,
        graph: Arc::new(SongGraph::default()),
        current_beat: 0,
        beats_played: 0,
        jumps: 0,
        branch_chance: 0.0,
        listen_time_ms: 0,
        last_branch: None,
        bouncing: false,
        no_analysis: true,
        effective: JukeboxSettings::default(),
        source: SettingsSource::Global,
        per_song: None,
    });
    events.trigger();
}

/// Build a driver for `track_id` from cached/fetched analysis. Returns the driver and the
/// shared graph, or None if no analysis is available.
fn build_driver(
    analysis: &AnalysisManager,
    cfg: &Config,
    settings: &JukeboxSettings,
    track_id: &str,
    seed: u64,
) -> Option<(Driver, Arc<SongGraph>)> {
    let order = cfg
        .values()
        .jukebox
        .clone()
        .unwrap_or_default()
        .analysis_source_order();
    let a = analysis.fetch(&order, track_id)?;
    let remixed = remix(&a);
    if remixed.beats.is_empty() {
        return None;
    }
    let g = graph::generate(settings, &remixed);
    let shared = Arc::new(g.clone());
    let driver = Driver::new(
        g,
        settings.clone(),
        Box::new(XorShiftRandom::new(seed | 1)),
        Box::new(SystemClock),
    );
    Some((driver, shared))
}

fn run_driver_loop(
    inner: Arc<JukeboxInner>,
    queue: Arc<Queue>,
    analysis: Arc<AnalysisManager>,
    events: EventManager,
    cfg: Arc<Config>,
) {
    let mut driver: Option<Driver> = None;
    let mut current_track_id: Option<String> = None;
    let mut current_graph: Arc<SongGraph> = Arc::new(SongGraph::default());
    let mut current_title = String::new();
    let mut current_effective = JukeboxSettings::default();
    let mut current_source = SettingsSource::Global;
    let mut current_partial: Option<PartialJukeboxSettings> = None;
    let mut started = Instant::now();
    let mut seek_cooldown: u32 = 0;
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;

    loop {
        std::thread::sleep(Duration::from_millis(TICK_MS));

        if !inner.enabled.load(Ordering::Relaxed) {
            if driver.is_some() {
                driver = None;
                current_track_id = None;
                *inner.state.write().unwrap() = None;
                events.trigger();
            }
            continue;
        }

        let spotify = queue.get_spotify();
        let Some(playable) = queue.get_current() else {
            continue;
        };
        let track_id = playable.id();

        if current_track_id != track_id {
            current_track_id = track_id.clone();
            driver = None;

            let Some(id) = track_id else {
                publish_no_analysis(&inner, &events, &playable);
                continue;
            };
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let (eff, src, partial) = {
                let base = inner.settings.read().unwrap().clone();
                let st = cfg.state();
                resolve_effective(
                    &base,
                    st.jukebox_override_per_song,
                    &st.jukebox_overrides,
                    &id,
                )
            };
            match build_driver(&analysis, &cfg, &eff, &id, seed) {
                Some((d, g)) => {
                    driver = Some(d);
                    current_graph = g;
                    current_title = playable.track().map(|t| t.title).unwrap_or_default();
                    current_effective = eff;
                    current_source = src;
                    current_partial = partial;
                    started = Instant::now();
                }
                None => {
                    publish_no_analysis(&inner, &events, &playable);
                    continue;
                }
            }
        }

        if inner.rebuild.swap(false, Ordering::Relaxed)
            && let Some(id) = current_track_id.clone()
        {
            let (eff, src, partial) = {
                let base = inner.settings.read().unwrap().clone();
                let st = cfg.state();
                resolve_effective(
                    &base,
                    st.jukebox_override_per_song,
                    &st.jukebox_overrides,
                    &id,
                )
            };
            if let Some((d, g)) = build_driver(&analysis, &cfg, &eff, &id, seed | 2) {
                driver = Some(d);
                current_graph = g;
                current_effective = eff;
                current_source = src;
                current_partial = partial;
                started = Instant::now();
            }
        }

        let Some(d) = driver.as_mut() else {
            continue;
        };
        d.set_bouncing(inner.bouncing.load(Ordering::Relaxed));

        if let Some(beat) = inner.seek_request.lock().unwrap().take()
            && let Some(b) = d.graph().beats.get(beat)
        {
            spotify.seek(b.start_ms.max(0.0) as u32);
            seek_cooldown = 3;
        }

        if seek_cooldown > 0 {
            seek_cooldown -= 1;
        } else {
            let progress = spotify.get_current_progress().as_millis() as f64;
            if let DriverAction::Jump { seek_ms } = d.process(progress) {
                spotify.seek(seek_ms.max(0.0) as u32);
                seek_cooldown = 2;
            }
        }

        *inner.state.write().unwrap() = Some(SongState {
            track_title: current_title.clone(),
            graph: current_graph.clone(),
            current_beat: d.current_beat().unwrap_or(0),
            beats_played: d.beats_played(),
            jumps: d.jumps(),
            branch_chance: d.branch_chance(),
            listen_time_ms: started.elapsed().as_millis() as u64,
            last_branch: d.last_branch(),
            bouncing: inner.bouncing.load(Ordering::Relaxed),
            no_analysis: false,
            effective: current_effective.clone(),
            source: current_source,
            per_song: current_partial.clone(),
        });
        events.trigger();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_effective_precedence() {
        use crate::jukebox::settings::{PartialJukeboxSettings, SettingsSource};
        use std::collections::HashMap;

        let base = JukeboxSettings::default();
        let mut overrides = HashMap::new();
        overrides.insert(
            "trk".to_string(),
            PartialJukeboxSettings {
                max_branch_distance: Some(70),
                ..PartialJukeboxSettings::default()
            },
        );

        // Per-song present, toggle off -> per-song wins.
        let (eff, src, partial) = resolve_effective(&base, false, &overrides, "trk");
        assert_eq!(eff.max_branch_distance, 70);
        assert_eq!(src, SettingsSource::PerSong);
        assert!(partial.is_some());

        // Toggle on -> global wins, per-song ignored.
        let (eff, src, partial) = resolve_effective(&base, true, &overrides, "trk");
        assert_eq!(eff.max_branch_distance, base.max_branch_distance);
        assert_eq!(src, SettingsSource::Global);
        assert!(partial.is_none());

        // No entry for this track -> global.
        let (_, src, partial) = resolve_effective(&base, false, &overrides, "other");
        assert_eq!(src, SettingsSource::Global);
        assert!(partial.is_none());
    }

    #[test]
    fn toggle_flips_enabled() {
        let j = Jukebox::test_instance();
        assert!(!j.is_enabled());
        assert!(j.toggle());
        assert!(j.is_enabled());
        assert!(!j.toggle());
    }

    #[test]
    fn view_mode_cycles_linear_radial_split() {
        let j = Jukebox::test_instance();
        assert_eq!(j.view_mode(), ViewMode::Linear);
        assert_eq!(j.cycle_view_mode(), ViewMode::Radial);
        assert_eq!(j.cycle_view_mode(), ViewMode::Split);
        assert_eq!(j.cycle_view_mode(), ViewMode::Linear);
    }

    #[test]
    fn seek_request_is_stored() {
        let j = Jukebox::test_instance();
        j.request_seek_to_beat(7);
        assert_eq!(*j.inner.seek_request.lock().unwrap(), Some(7));
    }

    #[test]
    fn toggle_graphics_flips() {
        let j = Jukebox::test_instance();
        assert!(!j.graphics_enabled());
        assert!(j.toggle_graphics());
        assert!(j.graphics_enabled());
    }
}
