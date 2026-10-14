//! Spotify's internal API (the one the official desktop client uses), reached over the
//! librespot session. Used for bulk metadata so it does not count against a Web API app quota;
//! the Web API stays as the fallback for every call.

pub mod error;
pub mod fallback;

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
