//! Spotify's internal API (the one the official desktop client uses), reached over the
//! librespot session. Used for bulk metadata so it does not count against a Web API app quota;
//! the Web API stays as the fallback for every call.

pub mod bridge;
pub mod error;
pub mod fallback;

use crate::config::ConfigValues;

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
}

impl Settings {
    pub fn from_values(values: &ConfigValues) -> Self {
        let areas = values.internal_api_areas.clone().unwrap_or_default();
        Self {
            enabled: values.internal_api.unwrap_or(INTERNAL_API_DEFAULT),
            from_config: values.internal_api.is_some(),
            metadata: areas.metadata.unwrap_or(true),
        }
    }

    /// Whether calls in `area` try the internal API.
    pub fn area_enabled(&self, area: Area) -> bool {
        self.enabled
            && match area {
                Area::Metadata => self.metadata,
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
}

impl Area {
    /// Name used in logs, the status bar message and the `[internal_api_areas]` table.
    pub fn name(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigValues;

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
}
