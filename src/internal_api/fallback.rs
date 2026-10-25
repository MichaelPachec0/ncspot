//! What to do after an internal API call: return it, fall back to the Web API, or fail.

use log::debug;

use super::error::InternalError;

/// The decision after a failed internal call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Spotify changed the protocol: latch the area, then use the Web API for this call.
    LatchThenWeb,
    /// Use the Web API for this call only.
    WebOnce,
    /// Return the failure; never move load onto the Web API for rate limits or transient errors.
    Fail,
}

/// The action for a failed internal call of class `err`.
pub fn decide_after(err: &InternalError) -> Action {
    match err {
        InternalError::ProtocolChanged(_) => Action::LatchThenWeb,
        InternalError::NoSession | InternalError::NotFound | InternalError::Denied => {
            Action::WebOnce
        }
        InternalError::RateLimited | InternalError::Transient(_) => Action::Fail,
    }
}

/// Run one `WebApi` call: `internal` first unless `skip_internal` (master switch off, area
/// disabled or latched), then `web` when the rules say so. `on_latch` runs once, before the
/// Web API call, when the internal error is a protocol change.
pub fn run_with_fallback<T>(
    skip_internal: bool,
    internal: impl FnOnce() -> Result<T, InternalError>,
    web: impl FnOnce() -> Option<T>,
    on_latch: impl FnOnce(&InternalError),
) -> Option<T> {
    if skip_internal {
        return web();
    }
    match internal() {
        Ok(value) => Some(value),
        Err(err) => match decide_after(&err) {
            Action::LatchThenWeb => {
                on_latch(&err);
                web()
            }
            Action::WebOnce => {
                debug!("internal API call fell back to the Web API: {err}");
                web()
            }
            Action::Fail => {
                debug!("internal API call failed: {err}");
                None
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn run(skip: bool, internal: Result<u32, InternalError>) -> (Option<u32>, bool, bool, bool) {
        let internal_called = Cell::new(false);
        let web_called = Cell::new(false);
        let latched = Cell::new(false);
        let out = run_with_fallback(
            skip,
            || {
                internal_called.set(true);
                internal
            },
            || {
                web_called.set(true);
                Some(2)
            },
            |_| latched.set(true),
        );
        (out, internal_called.get(), web_called.get(), latched.get())
    }

    #[test]
    fn skip_uses_web_without_trying_internal() {
        assert_eq!(run(true, Ok(1)), (Some(2), false, true, false));
    }

    #[test]
    fn ok_returns_internal_result() {
        assert_eq!(run(false, Ok(1)), (Some(1), true, false, false));
    }

    #[test]
    fn protocol_changed_latches_and_uses_web() {
        let r = run(false, Err(InternalError::ProtocolChanged("x".into())));
        assert_eq!(r, (Some(2), true, true, true));
    }

    #[test]
    fn no_session_not_found_denied_use_web_once() {
        for err in [
            InternalError::NoSession,
            InternalError::NotFound,
            InternalError::Denied,
        ] {
            assert_eq!(run(false, Err(err)), (Some(2), true, true, false));
        }
    }

    #[test]
    fn rate_limited_and_transient_fail_without_web() {
        for err in [
            InternalError::RateLimited,
            InternalError::Transient("x".into()),
        ] {
            assert_eq!(run(false, Err(err)), (None, true, false, false));
        }
    }

    #[test]
    fn decide_after_covers_every_class() {
        assert_eq!(
            decide_after(&InternalError::ProtocolChanged(String::new())),
            Action::LatchThenWeb
        );
        assert_eq!(decide_after(&InternalError::NoSession), Action::WebOnce);
        assert_eq!(decide_after(&InternalError::NotFound), Action::WebOnce);
        assert_eq!(decide_after(&InternalError::Denied), Action::WebOnce);
        assert_eq!(decide_after(&InternalError::RateLimited), Action::Fail);
        assert_eq!(
            decide_after(&InternalError::Transient(String::new())),
            Action::Fail
        );
    }
}
