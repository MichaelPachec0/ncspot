//! Spotify's internal API (the one the official desktop client uses), reached over the
//! librespot session. Used for bulk metadata so it does not count against a Web API app quota;
//! the Web API stays as the fallback for every call.

pub mod bridge;
pub mod collection;
pub mod convert;
pub mod error;
pub mod fallback;
pub mod metadata;
pub mod playlist;

#[cfg(test)]
mod live_tests;

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use librespot_core::Session;
use log::warn;

use crate::config::ConfigValues;
use crate::rate_limit::{
    Attempt, ClientLabel, MAX_RATE_LIMIT_RETRIES, ThrottleState, backoff, random_jitter,
    run_rate_limited,
};
use error::InternalError;

/// Whether the internal API is used when the configuration has no `internal_api` key.
pub const INTERNAL_API_DEFAULT: bool = true;

/// The internal API switches, resolved once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Master switch.
    pub enabled: bool,
    /// Whether `enabled` came from the configuration file (else from the default).
    pub from_config: bool,
    /// `[internal_api_areas] metadata`.
    pub metadata: bool,
    /// `[internal_api_areas] library`.
    pub library: bool,
    /// `[internal_api_areas] playlists`.
    pub playlists: bool,
}

impl Settings {
    pub fn from_values(values: &ConfigValues) -> Self {
        let areas = values.internal_api_areas.clone().unwrap_or_default();
        Self {
            enabled: values.internal_api.unwrap_or(INTERNAL_API_DEFAULT),
            from_config: values.internal_api.is_some(),
            metadata: areas.metadata.unwrap_or(true),
            library: areas.library.unwrap_or(true),
            playlists: areas.playlists.unwrap_or(true),
        }
    }

    /// Whether calls in `area` try the internal API.
    pub fn area_enabled(&self, area: Area) -> bool {
        self.enabled
            && match area {
                Area::Metadata => self.metadata,
                Area::Library => self.library,
                Area::Playlists => self.playlists,
            }
    }

    /// Startup log line, for example `internal API: on (default)`.
    pub fn describe(&self) -> String {
        let state = if self.enabled { "on" } else { "off" };
        let source = if self.from_config {
            "config"
        } else {
            "default"
        };
        format!("internal API: {state} ({source})")
    }
}

/// A group of `WebApi` methods that is switched and latched together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Area {
    /// Tracks, albums, artists, shows, episodes and the current user.
    Metadata,
    /// The library lists (collection sets).
    Library,
    /// The playlist list (rootlist) and playlist contents.
    Playlists,
}

impl Area {
    /// Name used in logs, the status bar message and the `[internal_api_areas]` table.
    pub fn name(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Library => "library",
            Self::Playlists => "playlists",
        }
    }
}

/// What happened to the calls of one area: answered by the internal API, or sent to the Web
/// API after an internal failure. Calls skipped because the area is off or latched count as
/// neither.
#[cfg(any(test, feature = "harness"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct AreaCounts {
    pub internal_ok: u64,
    pub fallback: u64,
    pub latched: bool,
}

#[derive(Debug, Default)]
struct AreaStats {
    internal_ok: AtomicU64,
    fallback: AtomicU64,
}

/// The internal API client: the librespot session, its own rate-limit window, and one latch
/// per area. Clones share all state.
#[derive(Clone)]
pub struct InternalApi {
    settings: Settings,
    session: Arc<RwLock<Option<Session>>>,
    pub(crate) throttle: Arc<RwLock<ThrottleState>>,
    latched_metadata: Arc<AtomicBool>,
    metadata_stats: Arc<AreaStats>,
    latched_library: Arc<AtomicBool>,
    library_stats: Arc<AreaStats>,
    latched_playlists: Arc<AtomicBool>,
    playlists_stats: Arc<AreaStats>,
}

impl InternalApi {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            session: Arc::new(RwLock::new(None)),
            throttle: Arc::new(RwLock::new(ThrottleState::default())),
            latched_metadata: Arc::new(AtomicBool::new(false)),
            metadata_stats: Arc::new(AreaStats::default()),
            latched_library: Arc::new(AtomicBool::new(false)),
            library_stats: Arc::new(AreaStats::default()),
            latched_playlists: Arc::new(AtomicBool::new(false)),
            playlists_stats: Arc::new(AreaStats::default()),
        }
    }

    /// Use the session handle that [crate::spotify::Spotify] fills when its worker connects.
    pub fn set_session(&mut self, session: Arc<RwLock<Option<Session>>>) {
        self.session = session;
    }

    /// The resolved switches.
    pub fn settings(&self) -> Settings {
        self.settings
    }

    /// The connected session, if any.
    pub fn session(&self) -> Option<Session> {
        self.session
            .read()
            .unwrap()
            .clone()
            .filter(|session| !session.is_invalid())
    }

    fn latch_flag(&self, area: Area) -> &AtomicBool {
        match area {
            Area::Metadata => &self.latched_metadata,
            Area::Library => &self.latched_library,
            Area::Playlists => &self.latched_playlists,
        }
    }

    /// Whether calls in `area` go straight to the Web API.
    pub fn skip(&self, area: Area) -> bool {
        !self.settings.area_enabled(area) || self.latch_flag(area).load(Ordering::Relaxed)
    }

    fn stats(&self, area: Area) -> &AreaStats {
        match area {
            Area::Metadata => &self.metadata_stats,
            Area::Library => &self.library_stats,
            Area::Playlists => &self.playlists_stats,
        }
    }

    /// Count a call in `area` answered by the internal API.
    pub(crate) fn count_ok(&self, area: Area) {
        self.stats(area).internal_ok.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a call in `area` that fell back to the Web API after an internal failure.
    pub(crate) fn count_fallback(&self, area: Area) {
        self.stats(area).fallback.fetch_add(1, Ordering::Relaxed);
    }

    /// Counters of `area` since startup.
    #[cfg(any(test, feature = "harness"))]
    pub fn area_counts(&self, area: Area) -> AreaCounts {
        let stats = self.stats(area);
        AreaCounts {
            internal_ok: stats.internal_ok.load(Ordering::Relaxed),
            fallback: stats.fallback.load(Ordering::Relaxed),
            latched: self.latch_flag(area).load(Ordering::Relaxed),
        }
    }

    /// Latch `area` for the rest of the session. True when this call set it.
    pub fn latch(&self, area: Area) -> bool {
        !self.latch_flag(area).swap(true, Ordering::Relaxed)
    }
}

/// Run `attempt` with the rate-limit gate of `throttle` (see [run_rate_limited]) and retry
/// transient failures on background threads, at most [MAX_RATE_LIMIT_RETRIES] times with
/// [backoff] delays (`sleep` is injected for tests). Other errors return at once.
pub fn run_attempts<T>(
    on_ui_thread: bool,
    throttle: &RwLock<ThrottleState>,
    mut attempt: impl FnMut() -> Result<T, InternalError>,
    mut on_429: impl FnMut(Duration),
    mut sleep: impl FnMut(Duration),
) -> Result<T, InternalError> {
    let mut transient_retries = 0;
    loop {
        let last_error: RefCell<Option<InternalError>> = RefCell::new(None);
        let result = run_rate_limited(
            ClientLabel::Internal,
            throttle,
            on_ui_thread,
            random_jitter,
            || match attempt() {
                Ok(value) => Attempt::Done(value),
                Err(InternalError::RateLimited) => Attempt::RateLimited(None),
                Err(err) => {
                    *last_error.borrow_mut() = Some(err);
                    Attempt::Failed(None)
                }
            },
            &mut on_429,
        );
        match result {
            Ok(value) => return Ok(value),
            Err(_) => match last_error.into_inner() {
                // run_rate_limited gave up on (or refused to send because of) a 429.
                None => return Err(InternalError::RateLimited),
                Some(InternalError::Transient(why))
                    if !on_ui_thread && transient_retries < MAX_RATE_LIMIT_RETRIES =>
                {
                    warn!(
                        "internal API call failed ({why}), retry {}",
                        transient_retries + 1
                    );
                    sleep(backoff(transient_retries));
                    transient_retries += 1;
                }
                Some(err) => return Err(err),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigValues;
    use crate::rate_limit::ThrottleState;
    use std::cell::RefCell;
    use std::sync::RwLock;

    fn settings(toml: &str) -> Settings {
        let values: ConfigValues = toml::from_str(toml).expect("valid toml");
        Settings::from_values(&values)
    }

    #[test]
    fn missing_key_uses_the_default() {
        let s = settings("");
        assert_eq!(s.enabled, INTERNAL_API_DEFAULT);
        assert!(!s.from_config);
        assert_eq!(s.area_enabled(Area::Metadata), INTERNAL_API_DEFAULT);
    }

    #[test]
    fn explicit_values_win() {
        assert!(settings("internal_api = true").enabled);
        let off = settings("internal_api = false");
        assert!(!off.enabled);
        assert!(off.from_config);
    }

    #[test]
    fn area_keys_default_to_true() {
        let s = settings("internal_api = true\n[internal_api_areas]\n");
        assert!(s.area_enabled(Area::Metadata));
    }

    #[test]
    fn area_can_be_turned_off() {
        let s = settings("internal_api = true\n[internal_api_areas]\nmetadata = false\n");
        assert!(!s.area_enabled(Area::Metadata));
    }

    #[test]
    fn master_off_ignores_the_area_table() {
        let s = settings("internal_api = false\n[internal_api_areas]\nmetadata = true\n");
        assert!(!s.area_enabled(Area::Metadata));
    }

    #[test]
    fn describe_names_state_and_source() {
        assert_eq!(
            settings("internal_api = false").describe(),
            "internal API: off (config)"
        );
        assert_eq!(
            settings("").describe(),
            format!(
                "internal API: {} (default)",
                if INTERNAL_API_DEFAULT { "on" } else { "off" }
            )
        );
    }

    fn attempts(
        on_ui: bool,
        script: Vec<Result<u8, InternalError>>,
    ) -> (Result<u8, InternalError>, usize, usize, usize) {
        let throttle = RwLock::new(ThrottleState::default());
        let script = RefCell::new(script.into_iter());
        let calls = RefCell::new(0);
        let r429 = RefCell::new(0);
        let sleeps = RefCell::new(0);
        let out = run_attempts(
            on_ui,
            &throttle,
            || {
                *calls.borrow_mut() += 1;
                script.borrow_mut().next().expect("script ran out")
            },
            |_| *r429.borrow_mut() += 1,
            |_| *sleeps.borrow_mut() += 1,
        );
        (
            out,
            calls.into_inner(),
            r429.into_inner(),
            sleeps.into_inner(),
        )
    }

    #[test]
    fn success_returns_at_once() {
        assert_eq!(attempts(false, vec![Ok(1)]), (Ok(1), 1, 0, 0));
    }

    #[test]
    fn transient_retries_on_background_threads_then_succeeds() {
        let t = || Err(InternalError::Transient("x".into()));
        let (out, calls, _, sleeps) = attempts(false, vec![t(), t(), Ok(4)]);
        assert_eq!((out, calls, sleeps), (Ok(4), 3, 2));
    }

    #[test]
    fn transient_gives_up_after_the_retry_limit() {
        let t = || Err(InternalError::Transient("x".into()));
        let script = (0..=MAX_RATE_LIMIT_RETRIES).map(|_| t()).collect();
        let (out, calls, _, _) = attempts(false, script);
        assert!(matches!(out, Err(InternalError::Transient(_))));
        assert_eq!(calls, MAX_RATE_LIMIT_RETRIES as usize + 1);
    }

    #[test]
    fn ui_thread_never_retries_transient() {
        let (out, calls, _, sleeps) =
            attempts(true, vec![Err(InternalError::Transient("x".into()))]);
        assert!(matches!(out, Err(InternalError::Transient(_))));
        assert_eq!((calls, sleeps), (1, 0));
    }

    #[test]
    fn non_retryable_errors_return_at_once() {
        for err in [
            InternalError::NotFound,
            InternalError::Denied,
            InternalError::NoSession,
            InternalError::ProtocolChanged("x".into()),
        ] {
            let (out, calls, _, _) = attempts(false, vec![Err(err.clone())]);
            assert_eq!((out, calls), (Err(err), 1));
        }
    }

    #[test]
    fn rate_limited_records_a_window_and_fails_on_the_ui_thread() {
        let (out, calls, r429, _) = attempts(true, vec![Err(InternalError::RateLimited)]);
        assert_eq!((out, calls, r429), (Err(InternalError::RateLimited), 1, 1));
    }

    #[test]
    fn skip_covers_master_area_and_latch() {
        let on = Settings {
            enabled: true,
            from_config: true,
            metadata: true,
            library: true,
            playlists: true,
        };
        let api = InternalApi::new(on);
        assert!(!api.skip(Area::Metadata));
        assert!(api.latch(Area::Metadata));
        assert!(
            !api.latch(Area::Metadata),
            "second latch reports already latched"
        );
        assert!(api.skip(Area::Metadata));
        assert!(
            InternalApi::new(Settings {
                enabled: false,
                ..on
            })
            .skip(Area::Metadata)
        );
        assert!(
            InternalApi::new(Settings {
                metadata: false,
                ..on
            })
            .skip(Area::Metadata)
        );
    }

    #[test]
    fn library_area_follows_its_switch_and_the_master_switch() {
        let mut values = crate::config::ConfigValues::default();
        assert!(Settings::from_values(&values).area_enabled(Area::Library));
        values.internal_api_areas = Some(crate::config::InternalApiAreas {
            library: Some(false),
            ..Default::default()
        });
        let s = Settings::from_values(&values);
        assert!(!s.area_enabled(Area::Library));
        assert!(s.area_enabled(Area::Metadata));
        values.internal_api_areas = None;
        values.internal_api = Some(false);
        assert!(!Settings::from_values(&values).area_enabled(Area::Library));
    }

    #[test]
    fn playlists_area_follows_its_switch_and_the_master_switch() {
        let mut values = crate::config::ConfigValues::default();
        assert!(Settings::from_values(&values).area_enabled(Area::Playlists));
        values.internal_api_areas = Some(crate::config::InternalApiAreas {
            playlists: Some(false),
            ..Default::default()
        });
        let s = Settings::from_values(&values);
        assert!(!s.area_enabled(Area::Playlists));
        assert!(s.area_enabled(Area::Library));
        assert_eq!(Area::Playlists.name(), "playlists");
    }

    #[test]
    fn library_latch_is_separate_from_metadata() {
        let api = InternalApi::new(Settings::from_values(&Default::default()));
        assert!(api.latch(Area::Library));
        assert!(api.skip(Area::Library));
        assert!(!api.skip(Area::Metadata));
        assert!(api.area_counts(Area::Library).latched);
    }

    #[test]
    fn no_session_handle_means_no_session() {
        let api = InternalApi::new(Settings {
            enabled: true,
            from_config: true,
            metadata: true,
            library: true,
            playlists: true,
        });
        assert!(api.session().is_none());
    }
}
