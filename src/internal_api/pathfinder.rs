//! The Pathfinder client: one persisted GraphQL query over the librespot session, and the
//! outcome its HTTP status and body mean.

use bytes::Bytes;
use http::{HeaderValue, Method, Request, header};
use http_body_util::BodyExt;
use librespot_core::Session;
use serde_json::{Value, json};

use super::error::{InternalError, classify};

pub const URL: &str = "https://api-partner.spotify.com/pathfinder/v2/query";

/// What one query came back as.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// `data.searchV2`, an object.
    Answer(serde_json::Value),
    /// `data.searchV2`, an object, next to a non-empty top-level `errors` array: a GraphQL
    /// partial answer (a server-side failure in some fields). Never a hash failure.
    Partial(serde_json::Value),
    /// HTTP 412: Spotify does not know this hash.
    BadHash,
    /// HTTP 400: the operation name and the hash do not go together, or Spotify rejects
    /// the request itself. A bad hash unless the hash already answered in this session.
    BadRequest,
    /// HTTP 2xx without a `data.searchV2` object: the request was refused (bad input,
    /// a server-side failure). Not a hash failure, and not worth sending again.
    NoResult,
}

pub fn search_variables(term: &str, offset: usize, limit: usize) -> Value {
    json!({
        "searchTerm": term,
        "offset": offset,
        "limit": limit,
        "numberOfTopResults": 5,
        "includeAudiobooks": false,
        "includeArtistHasConcertsField": false,
        "includePreReleases": false,
        "includeLocalConcertsField": false,
        "includeAuthors": false,
    })
}

pub fn request_body(operation: &str, hash: &str, variables: &Value) -> String {
    json!({
        "variables": variables,
        "operationName": operation,
        "extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } },
    })
    .to_string()
}

/// Pure: the outcome for an HTTP status and body. Error texts are fixed strings, never body
/// text, since the body may echo the search term.
pub fn outcome_of(status: u16, body: &[u8]) -> Result<Outcome, InternalError> {
    match status {
        412 => Ok(Outcome::BadHash),
        400 => Ok(Outcome::BadRequest),
        200..=299 => {
            let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
                return Ok(Outcome::NoResult);
            };
            let partial = v
                .get("errors")
                .and_then(Value::as_array)
                .is_some_and(|errors| !errors.is_empty());
            Ok(match v.pointer_mut("/data/searchV2").map(Value::take) {
                Some(found) if found.is_object() && partial => Outcome::Partial(found),
                Some(found) if found.is_object() => Outcome::Answer(found),
                _ => Outcome::NoResult,
            })
        }
        429 => Err(InternalError::RateLimited),
        401 | 403 => Err(InternalError::Denied),
        // A request timeout is worth sending again, unlike the other 4xx statuses.
        408 => Err(InternalError::Transient("pathfinder: http 408".into())),
        // Every other 4xx; the ones with their own meaning are matched above.
        status if (400..=499).contains(&status) => Err(InternalError::NotFound),
        _ => Err(InternalError::Transient(format!(
            "pathfinder: http {status}"
        ))),
    }
}

/// The class of a failure to get the login5 token or the client-token.
pub fn token_error(err: &librespot_core::Error) -> InternalError {
    match classify(err) {
        InternalError::ProtocolChanged(_) => InternalError::Transient("pathfinder: token".into()),
        other => other,
    }
}

fn network() -> InternalError {
    InternalError::Transient("pathfinder: network".into())
}

/// Send one query. A single request: librespot's `request` would sleep and resend on a 429.
pub async fn query(
    session: Session,
    operation: &'static str,
    hash: String,
    variables: Value,
) -> Result<Outcome, InternalError> {
    let token = session
        .login5()
        .auth_token()
        .await
        .map_err(|e| token_error(&e))?;
    // A client-token that is not a valid header value is left out, not an error.
    let client_token = session
        .spclient()
        .client_token()
        .await
        .ok()
        .and_then(|token| HeaderValue::from_str(&token).ok());

    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(URL)
        .header(header::CONTENT_TYPE, "application/json;charset=UTF-8")
        .header(header::ACCEPT, "application/json")
        .header(
            header::AUTHORIZATION,
            format!("{} {}", token.token_type, token.access_token),
        )
        .header("app-platform", "Win32_x86_64")
        .header(
            "spotify-app-version",
            librespot_core::version::spotify_semantic_version(),
        );
    if let Some(client_token) = client_token {
        builder = builder.header("client-token", client_token);
    }
    let request = builder
        .body(Bytes::from(request_body(operation, &hash, &variables)))
        .map_err(|_| InternalError::Transient("pathfinder: request".into()))?;

    let response = session
        .http_client()
        .request_fut(request)
        .map_err(|e| classify(&e))?
        .await
        .map_err(|_| network())?;
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|_| network())?
        .to_bytes();
    outcome_of(status, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_hash_statuses() {
        assert_eq!(outcome_of(412, b""), Ok(Outcome::BadHash));
        assert_eq!(outcome_of(400, b""), Ok(Outcome::BadRequest));
    }

    #[test]
    fn a_request_timeout_is_transient_unlike_other_4xx() {
        assert!(matches!(
            outcome_of(408, b""),
            Err(InternalError::Transient(_))
        ));
        assert_eq!(outcome_of(404, b""), Err(InternalError::NotFound));
        assert_eq!(outcome_of(409, b""), Err(InternalError::NotFound));
    }

    #[test]
    fn an_answer_is_the_search_object() {
        let body = br#"{"data":{"searchV2":{"tracksV2":{"items":[]}}}}"#;
        let Ok(Outcome::Answer(v)) = outcome_of(200, body) else {
            panic!()
        };
        assert!(v["tracksV2"]["items"].is_array());
    }

    #[test]
    fn an_answer_with_errors_is_partial() {
        let body =
            br#"{"data":{"searchV2":{"tracksV2":null}},"errors":[{"message":"SECRETTERM"}]}"#;
        let Ok(Outcome::Partial(v)) = outcome_of(200, body) else {
            panic!()
        };
        assert!(v["tracksV2"].is_null());
        // An empty `errors` array is a plain answer.
        let body = br#"{"data":{"searchV2":{}},"errors":[]}"#;
        assert!(matches!(outcome_of(200, body), Ok(Outcome::Answer(_))));
    }

    #[test]
    fn a_200_without_results_is_no_result() {
        for body in [
            &br#"{"errors":[{"message":"term was SECRETTERM"}]}"#[..],
            br#"{"data":{"searchV2":null}}"#,
            b"not json",
        ] {
            assert_eq!(outcome_of(200, body), Ok(Outcome::NoResult));
        }
    }

    #[test]
    fn other_statuses() {
        assert_eq!(outcome_of(429, b""), Err(InternalError::RateLimited));
        assert_eq!(outcome_of(401, b""), Err(InternalError::Denied));
        assert_eq!(outcome_of(403, b""), Err(InternalError::Denied));
        for status in [404, 405, 413] {
            assert_eq!(
                outcome_of(status, b"SECRETTERM"),
                Err(InternalError::NotFound)
            );
        }
        for status in [500, 503] {
            let err = outcome_of(status, b"SECRETTERM").unwrap_err();
            assert_eq!(
                err,
                InternalError::Transient(format!("pathfinder: http {status}"))
            );
        }
    }

    #[test]
    fn a_login5_failed_precondition_is_not_a_protocol_change() {
        let err = librespot_core::Error::failed_precondition("login5 gave up");
        assert!(matches!(token_error(&err), InternalError::Transient(_)));
        let err = librespot_core::Error::resource_exhausted("slow down");
        assert_eq!(token_error(&err), InternalError::RateLimited);
    }

    #[test]
    fn variables_carry_the_term_verbatim() {
        let term = "caf\u{e9} \"live\" \\ artist:x";
        let v = search_variables(term, 50, 50);
        assert_eq!(v["searchTerm"], term);
        assert_eq!(
            (v["offset"].as_u64(), v["limit"].as_u64()),
            (Some(50), Some(50))
        );
        assert_eq!(v["numberOfTopResults"], 5);
        for flag in [
            "includeAudiobooks",
            "includeArtistHasConcertsField",
            "includePreReleases",
            "includeLocalConcertsField",
            "includeAuthors",
        ] {
            assert_eq!(v[flag], false);
        }
        let body: serde_json::Value =
            serde_json::from_str(&request_body("searchDesktop", "ab", &v)).unwrap();
        assert_eq!(body["variables"]["searchTerm"], term);
        assert_eq!(body["operationName"], "searchDesktop");
        assert_eq!(body["extensions"]["persistedQuery"]["sha256Hash"], "ab");
        assert_eq!(body["extensions"]["persistedQuery"]["version"], 1);
    }
}
