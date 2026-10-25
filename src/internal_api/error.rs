//! Error classes of internal API calls. The class alone decides the fallback.

use std::fmt;

use librespot_core::error::ErrorKind;
use librespot_core::http_client::HttpClientError;

/// Why an internal API call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InternalError {
    /// Spotify changed something on its side (HTTP 412, a response that does not parse, a
    /// required field missing). The area falls back to the Web API for the rest of the session.
    ProtocolChanged(String),
    /// The librespot session is not connected (yet) or was invalidated.
    NoSession,
    /// The item does not exist on the internal API (or the request was malformed).
    NotFound,
    /// HTTP 401 or 403: a region restriction for one item, or librespot refreshing its token.
    Denied,
    /// HTTP 429 that librespot did not retry by itself (no `Retry-After`).
    RateLimited,
    /// Timeouts, 5xx and network errors.
    Transient(String),
}

impl fmt::Display for InternalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolChanged(why) => write!(f, "protocol changed: {why}"),
            Self::NoSession => write!(f, "no session"),
            Self::NotFound => write!(f, "not found"),
            Self::Denied => write!(f, "denied"),
            Self::RateLimited => write!(f, "rate limited"),
            Self::Transient(why) => write!(f, "transient: {why}"),
        }
    }
}

/// Map a librespot error to its class. librespot already turns protobuf decode errors into
/// `FailedPrecondition`, so those count as a protocol change too. librespot files HTTP 451
/// under `Unavailable` with 500 and 503; a 451 is an item blocked for this account or region,
/// so it is `NotFound` (Web API for this call), not a transient error that is retried.
pub fn classify(err: &librespot_core::Error) -> InternalError {
    if let Some(HttpClientError::StatusCode(code)) = err.error.downcast_ref::<HttpClientError>()
        && code.as_u16() == 451
    {
        return InternalError::NotFound;
    }
    match err.kind {
        ErrorKind::FailedPrecondition => InternalError::ProtocolChanged(err.to_string()),
        ErrorKind::NotFound | ErrorKind::InvalidArgument | ErrorKind::OutOfRange => {
            InternalError::NotFound
        }
        ErrorKind::PermissionDenied | ErrorKind::Unauthenticated => InternalError::Denied,
        ErrorKind::ResourceExhausted => InternalError::RateLimited,
        _ => InternalError::Transient(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_core::Error;

    #[test]
    fn failed_precondition_is_protocol_changed() {
        let err = Error::failed_precondition("412");
        assert!(matches!(classify(&err), InternalError::ProtocolChanged(_)));
    }

    #[test]
    fn not_found_invalid_argument_and_out_of_range_are_not_found() {
        assert_eq!(classify(&Error::not_found("x")), InternalError::NotFound);
        assert_eq!(
            classify(&Error::invalid_argument("x")),
            InternalError::NotFound
        );
        assert_eq!(classify(&Error::out_of_range("x")), InternalError::NotFound);
    }

    #[test]
    fn permission_denied_and_unauthenticated_are_denied() {
        assert_eq!(
            classify(&Error::permission_denied("x")),
            InternalError::Denied
        );
        assert_eq!(
            classify(&Error::unauthenticated("x")),
            InternalError::Denied
        );
    }

    #[test]
    fn resource_exhausted_is_rate_limited() {
        assert_eq!(
            classify(&Error::resource_exhausted("x")),
            InternalError::RateLimited
        );
    }

    #[test]
    fn http_451_is_not_found_other_unavailable_is_transient() {
        let status = |code| {
            Error::from(HttpClientError::StatusCode(
                http::StatusCode::from_u16(code).unwrap(),
            ))
        };
        assert_eq!(classify(&status(451)), InternalError::NotFound);
        for code in [500, 503, 421] {
            assert!(
                matches!(classify(&status(code)), InternalError::Transient(_)),
                "{code}"
            );
        }
        assert!(matches!(
            classify(&status(412)),
            InternalError::ProtocolChanged(_)
        ));
    }

    #[test]
    fn everything_else_is_transient() {
        for err in [
            Error::deadline_exceeded("x"),
            Error::unavailable("x"),
            Error::internal("x"),
            Error::unknown("x"),
            Error::aborted("x"),
        ] {
            assert!(
                matches!(classify(&err), InternalError::Transient(_)),
                "{err}"
            );
        }
    }
}
