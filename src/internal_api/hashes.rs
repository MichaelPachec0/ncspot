//! The persisted-query hashes of Spotify's Pathfinder endpoint. Spotify retires hashes
//! without notice, so every operation has a list of candidates: the last one that worked, the
//! user's overrides from `pathfinder_hashes.json`, then the built-in ones. A hash that Spotify
//! refuses is left out for the rest of the session.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use log::warn;
use parking_lot::Mutex;

use super::error::InternalError;
use super::pathfinder::Outcome;

/// One Pathfinder search operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    Desktop,
    Tracks,
    Artists,
    Albums,
    Playlists,
    Podcasts,
}

impl Operation {
    pub const ALL: [Self; 6] = [
        Self::Desktop,
        Self::Tracks,
        Self::Artists,
        Self::Albums,
        Self::Playlists,
        Self::Podcasts,
    ];

    /// The operation name Spotify uses, also the key in the override file.
    pub fn name(self) -> &'static str {
        match self {
            Self::Desktop => "searchDesktop",
            Self::Tracks => "searchTracks",
            Self::Artists => "searchArtists",
            Self::Albums => "searchAlbums",
            Self::Playlists => "searchPlaylists",
            Self::Podcasts => "searchPodcasts",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|op| op.name() == name)
    }

    /// Built-in hashes, newest first.
    pub fn builtin(self) -> &'static [&'static str] {
        match self {
            Self::Desktop => &[
                "eef7cc54888d91bdd6802623477873caa3948ae173a0c34fd86827b267e94c03",
                "fcad5a3e0d5af727fb76966f06971c19cfa2275e6ff7671196753e008611873c",
                "3c9d3f60dac5dea3876b6db3f534192b1c1d90032c4233c1bbaba526db41eb31",
                "4801118d4a100f756e833d33984436a3899cff359c532f8fd3aaf174b60b3b49",
            ],
            Self::Tracks => &["b02683192a98dde7966b5e6655a79eeb62713eab703eda9902c932818dd52751"],
            Self::Artists => &["7bf95d754fdbe32c8b161fbbe54d1ae50974900df4dce4c8f1afcbcad153224d"],
            Self::Albums => &["202cb3305e31e5a0767ba7925f28bd728cf8f8b0217e6da43909056071cd70e9"],
            Self::Playlists => {
                &["d520014e748f9ea44f7707d8df1819867ac1205e8b7f3e28f22fe5fc858921b1"]
            }
            Self::Podcasts => &["0195d9f61b43606d490bca64c3456e3593528cea6cc05c7e822c7c42beed0f4e"],
        }
    }
}

/// Most override hashes kept per operation.
pub const MAX_OVERRIDES: usize = 8;
/// Name of the override file in the configuration folder. ncspot only reads it.
pub const OVERRIDE_FILE: &str = "pathfinder_hashes.json";
pub type Overrides = HashMap<Operation, Vec<String>>;
/// Called with (operation name, hash) when the last working hash changes.
pub type Persist = Arc<dyn Fn(&str, &str) + Send + Sync>;

fn is_hash(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The usable overrides in `text` and the warnings to log (no file contents in them beyond a
/// key name or a count).
pub fn parse_overrides(text: &str) -> (Overrides, Vec<String>) {
    let mut overrides = Overrides::new();
    let mut warnings = Vec::new();
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => {
            warnings.push(format!("{OVERRIDE_FILE} is not valid JSON, ignored"));
            return (overrides, warnings);
        }
    };
    let Some(object) = value.as_object() else {
        warnings.push(format!("{OVERRIDE_FILE} is not a JSON object, ignored"));
        return (overrides, warnings);
    };
    for (key, value) in object {
        let Some(op) = Operation::from_name(key) else {
            warnings.push(format!("{OVERRIDE_FILE}: unknown key \"{key}\" ignored"));
            continue;
        };
        let Some(entries) = value.as_array() else {
            warnings.push(format!("{OVERRIDE_FILE}: {key}: 1 entry skipped"));
            continue;
        };
        let mut valid: Vec<String> = Vec::new();
        for hash in entries
            .iter()
            .filter_map(|entry| entry.as_str().filter(|s| is_hash(s)))
        {
            if !valid.iter().any(|seen| seen == hash) {
                valid.push(hash.to_string());
            }
        }
        let skipped = entries.len() - valid.len();
        if skipped > 0 {
            warnings.push(format!("{OVERRIDE_FILE}: {key}: {skipped} entries skipped"));
        }
        if valid.len() > MAX_OVERRIDES {
            warnings.push(format!(
                "{OVERRIDE_FILE}: {key}: only the first {MAX_OVERRIDES} hashes are used"
            ));
            valid.truncate(MAX_OVERRIDES);
        }
        if !valid.is_empty() {
            overrides.insert(op, valid);
        }
    }
    (overrides, warnings)
}

/// Read `path`; a missing file is silent, every warning is logged at `warn`.
pub fn load_overrides(path: &Path) -> Overrides {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Overrides::new(),
        Err(err) => {
            warn!("could not read {OVERRIDE_FILE}: {}", err.kind());
            return Overrides::new();
        }
    };
    let (overrides, warnings) = parse_overrides(&text);
    for warning in warnings {
        warn!("{warning}");
    }
    overrides
}

#[derive(Default)]
struct State {
    bad: HashSet<String>,
    working: Option<String>,
    /// Hashes that gave a readable answer in this session.
    answered: HashSet<String>,
}

struct Shared {
    overrides: Overrides,
    state: Mutex<HashMap<Operation, State>>,
    persist: Persist,
}

/// The hashes of every operation. Clones share all state.
#[derive(Clone)]
pub struct HashStore {
    shared: Arc<Shared>,
}

impl Default for HashStore {
    /// No overrides, no saved hashes, nothing persisted.
    fn default() -> Self {
        Self::new(Overrides::new(), &HashMap::new(), Arc::new(|_, _| {}))
    }
}

impl HashStore {
    pub fn new(overrides: Overrides, saved: &HashMap<String, String>, persist: Persist) -> Self {
        let mut state = HashMap::new();
        for op in Operation::ALL {
            let working = saved.get(op.name()).cloned();
            state.insert(
                op,
                State {
                    bad: HashSet::new(),
                    working,
                    answered: HashSet::new(),
                },
            );
        }
        let this = Self {
            shared: Arc::new(Shared {
                overrides,
                state: Mutex::new(state),
                persist,
            }),
        };
        // A saved hash that is in no list is never a candidate.
        {
            let mut state = this.shared.state.lock();
            for op in Operation::ALL {
                let known = this.listed(op);
                let entry = state.entry(op).or_default();
                if entry.working.as_ref().is_some_and(|w| !known.contains(w)) {
                    entry.working = None;
                }
            }
        }
        this
    }

    /// Overrides then built-in hashes, each once.
    fn listed(&self, op: Operation) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let own = self
            .shared
            .overrides
            .get(&op)
            .into_iter()
            .flatten()
            .cloned();
        for hash in own.chain(op.builtin().iter().map(|h| h.to_string())) {
            if !out.contains(&hash) {
                out.push(hash);
            }
        }
        out
    }

    /// Hashes to try, in order, without the ones marked bad.
    pub fn candidates(&self, op: Operation) -> Vec<String> {
        let mut list = self.listed(op);
        let state = self.shared.state.lock();
        let Some(entry) = state.get(&op) else {
            return list;
        };
        if let Some(working) = &entry.working
            && let Some(at) = list.iter().position(|h| h == working)
        {
            let hash = list.remove(at);
            list.insert(0, hash);
        }
        list.retain(|h| !entry.bad.contains(h));
        list
    }

    pub fn mark_bad(&self, op: Operation, hash: &str) {
        let mut state = self.shared.state.lock();
        let entry = state.entry(op).or_default();
        entry.bad.insert(hash.to_string());
        if entry.working.as_deref() == Some(hash) {
            entry.working = None;
        }
    }

    /// Remember `hash` as working; calls `persist` only when it differs from the last one.
    pub fn mark_working(&self, op: Operation, hash: &str) {
        let changed = {
            let mut state = self.shared.state.lock();
            let entry = state.entry(op).or_default();
            entry.answered.insert(hash.to_string());
            if entry.working.as_deref() == Some(hash) {
                false
            } else {
                entry.working = Some(hash.to_string());
                true
            }
        };
        // Outside the lock: persist takes the configuration's state lock.
        if changed {
            (self.shared.persist)(op.name(), hash);
        }
    }

    pub fn is_bad(&self, op: Operation, hash: &str) -> bool {
        let state = self.shared.state.lock();
        state.get(&op).is_some_and(|entry| entry.bad.contains(hash))
    }

    /// Whether `hash` gave a readable answer for `op` in this session.
    pub fn has_answered(&self, op: Operation, hash: &str) -> bool {
        let state = self.shared.state.lock();
        state
            .get(&op)
            .is_some_and(|entry| entry.answered.contains(hash))
    }

    /// True once every candidate of `op` was marked bad.
    pub fn is_retired(&self, op: Operation) -> bool {
        self.candidates(op).is_empty()
    }
}

#[derive(Debug, PartialEq)]
pub enum RotateError {
    /// Every candidate is bad; nothing was or will be sent for this operation.
    Retired,
    /// The request was refused (`Outcome::NoResult`, an unreadable `Outcome::Partial`, or
    /// `Outcome::BadRequest` for a hash that already answered); no hash was marked.
    NoResult,
    Failed(InternalError),
}

/// Try the candidates of `op` in order. `send(hash)` sends one query; `read(answer)` returns
/// `None` when the answer's required sections are missing or unreadable.
pub fn rotate<T>(
    store: &HashStore,
    op: Operation,
    mut send: impl FnMut(&str) -> Result<Outcome, InternalError>,
    mut read: impl FnMut(&serde_json::Value) -> Option<T>,
) -> Result<T, RotateError> {
    if store.is_retired(op) {
        return Err(RotateError::Retired);
    }
    for hash in store.candidates(op) {
        // The list is a snapshot: another thread may have marked this hash since.
        if store.is_bad(op, &hash) {
            continue;
        }
        match send(&hash) {
            Ok(Outcome::Answer(answer)) => {
                if let Some(value) = read(&answer) {
                    store.mark_working(op, &hash);
                    return Ok(value);
                }
            }
            // A partial answer is a server-side failure: read what came back, never blame
            // the hash. The cost: an answer whose shape changed and that also carries errors
            // fails the search instead of moving on to the next hash, so it is logged.
            Ok(Outcome::Partial(answer)) => {
                return match read(&answer) {
                    Some(value) => {
                        store.mark_working(op, &hash);
                        Ok(value)
                    }
                    None => {
                        warn!(
                            "{}: an answer with errors could not be read, hash {} kept; if \
                             this repeats, the answer shape may have changed",
                            op.name(),
                            hash.get(..8).unwrap_or(&hash)
                        );
                        Err(RotateError::NoResult)
                    }
                };
            }
            Ok(Outcome::BadHash) => {}
            // A hash that already answered is not the cause of a 400: the request is.
            Ok(Outcome::BadRequest) if store.has_answered(op, &hash) => {
                warn!(
                    "{}: request rejected (http 400), hash {} kept",
                    op.name(),
                    hash.get(..8).unwrap_or(&hash)
                );
                return Err(RotateError::NoResult);
            }
            Ok(Outcome::BadRequest) => {}
            Ok(Outcome::NoResult) => return Err(RotateError::NoResult),
            Err(err) => return Err(RotateError::Failed(err)),
        }
        warn!(
            "{}: hash {} was not accepted, trying the next one",
            op.name(),
            hash.get(..8).unwrap_or(&hash)
        );
        store.mark_bad(op, &hash);
    }
    warn!("{}: every known hash is retired", op.name());
    Err(RotateError::Retired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn h(n: u8) -> String {
        n.to_string().repeat(64)[..64].to_string()
    }

    #[allow(clippy::type_complexity)]
    fn store<'a>(
        overrides: impl IntoIterator<Item = (Operation, Vec<String>)>,
        saved: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> (HashStore, Arc<Mutex<Vec<(String, String)>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let persist: Persist = Arc::new(move |op, hash| {
            sink.lock().push((op.to_string(), hash.to_string()));
        });
        let saved = saved
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (
            HashStore::new(overrides.into_iter().collect(), &saved, persist),
            log,
        )
    }

    #[test]
    fn order_is_last_working_then_overrides_then_builtin() {
        let b = Operation::Desktop.builtin();
        let (s, _) = store(
            [(Operation::Desktop, vec![h(1), h(2)])],
            [("searchDesktop", b[1])],
        );
        let c = s.candidates(Operation::Desktop);
        assert_eq!(c[0], b[1]);
        assert_eq!(&c[1..3], &[h(1), h(2)]);
        assert_eq!(c.len(), 2 + b.len());
    }

    #[test]
    fn duplicate_hashes_are_one_candidate() {
        let b = Operation::Tracks.builtin();
        let (s, _) = store(
            [(Operation::Tracks, vec![h(1), h(1), b[0].to_string()])],
            [],
        );
        assert_eq!(
            s.candidates(Operation::Tracks),
            vec![h(1), b[0].to_string()]
        );
    }

    #[test]
    fn a_saved_hash_in_no_list_is_ignored() {
        let (s, _) = store([], [("searchTracks", h(9).as_str())]);
        assert_eq!(s.candidates(Operation::Tracks), Operation::Tracks.builtin());
    }

    #[test]
    fn rotation_skips_bad_hashes_and_saves_the_one_that_was_read() {
        let (s, saved) = store([], []);
        let b = Operation::Desktop.builtin();
        let mut sent = Vec::new();
        let got = rotate(
            &s,
            Operation::Desktop,
            |hash| {
                sent.push(hash.to_string());
                Ok(if sent.len() == 1 {
                    Outcome::BadHash
                } else {
                    Outcome::Answer(json!({}))
                })
            },
            |_| Some(7),
        );
        assert_eq!(got, Ok(7));
        assert_eq!(sent, vec![b[0], b[1]]);
        assert_eq!(
            *saved.lock(),
            vec![("searchDesktop".to_string(), b[1].to_string())]
        );
        // The bad hash is never sent again, and the working one goes first.
        assert_eq!(s.candidates(Operation::Desktop)[0], b[1]);
        assert!(!s.candidates(Operation::Desktop).contains(&b[0].to_string()));
    }

    #[test]
    fn an_unreadable_answer_is_a_bad_hash_and_is_not_saved() {
        let (s, saved) = store([], []);
        let got: Result<u8, _> = rotate(
            &s,
            Operation::Tracks,
            |_| Ok(Outcome::Answer(json!({}))),
            |_| None,
        );
        assert_eq!(got, Err(RotateError::Retired));
        assert!(saved.lock().is_empty());
        assert!(s.is_retired(Operation::Tracks));
    }

    #[test]
    fn another_error_ends_the_rotation_and_marks_nothing() {
        let (s, _) = store([], []);
        let mut calls = 0;
        let got: Result<u8, _> = rotate(
            &s,
            Operation::Desktop,
            |_| {
                calls += 1;
                Err(InternalError::RateLimited)
            },
            |_| Some(1),
        );
        assert_eq!(got, Err(RotateError::Failed(InternalError::RateLimited)));
        assert_eq!(calls, 1);
        assert_eq!(
            s.candidates(Operation::Desktop).len(),
            Operation::Desktop.builtin().len()
        );
    }

    #[test]
    fn a_refused_request_marks_nothing() {
        let (s, saved) = store([], []);
        let mut calls = 0;
        let got: Result<u8, _> = rotate(
            &s,
            Operation::Desktop,
            |_| {
                calls += 1;
                Ok(Outcome::NoResult)
            },
            |_| Some(1),
        );
        assert_eq!((got, calls), (Err(RotateError::NoResult), 1));
        assert_eq!(
            s.candidates(Operation::Desktop).len(),
            Operation::Desktop.builtin().len()
        );
        assert!(saved.lock().is_empty() && !s.is_retired(Operation::Desktop));
    }

    #[test]
    fn a_readable_partial_answer_is_an_answer() {
        let (s, saved) = store([], []);
        let b = Operation::Desktop.builtin();
        let mut calls = 0;
        let got = rotate(
            &s,
            Operation::Desktop,
            |_| {
                calls += 1;
                Ok(Outcome::Partial(json!({})))
            },
            |_| Some(3),
        );
        assert_eq!((got, calls), (Ok(3), 1));
        assert_eq!(
            *saved.lock(),
            vec![("searchDesktop".to_string(), b[0].to_string())]
        );
    }

    #[test]
    fn an_unreadable_partial_answer_marks_nothing() {
        let (s, saved) = store([], []);
        let mut calls = 0;
        let got: Result<u8, _> = rotate(
            &s,
            Operation::Desktop,
            |_| {
                calls += 1;
                Ok(Outcome::Partial(json!({})))
            },
            |_| None,
        );
        assert_eq!((got, calls), (Err(RotateError::NoResult), 1));
        assert_eq!(
            s.candidates(Operation::Desktop).len(),
            Operation::Desktop.builtin().len()
        );
        assert!(saved.lock().is_empty() && !s.is_retired(Operation::Desktop));
    }

    #[test]
    fn a_400_for_a_hash_that_never_answered_is_a_bad_hash() {
        let (s, saved) = store([], []);
        let b = Operation::Desktop.builtin();
        let mut sent = Vec::new();
        let got = rotate(
            &s,
            Operation::Desktop,
            |hash| {
                sent.push(hash.to_string());
                Ok(if sent.len() == 1 {
                    Outcome::BadRequest
                } else {
                    Outcome::Answer(json!({}))
                })
            },
            |_| Some(7),
        );
        assert_eq!(got, Ok(7));
        assert_eq!(sent, vec![b[0], b[1]]);
        assert!(s.is_bad(Operation::Desktop, b[0]));
        assert_eq!(saved.lock().len(), 1);
    }

    #[test]
    fn a_400_for_a_hash_that_answered_fails_the_call_and_marks_nothing() {
        let (s, _) = store([], []);
        let b = Operation::Desktop.builtin();
        let first = rotate(
            &s,
            Operation::Desktop,
            |_| Ok(Outcome::Answer(json!({}))),
            |_| Some(1),
        );
        assert_eq!(first, Ok(1));
        let mut calls = 0;
        let got: Result<u8, _> = rotate(
            &s,
            Operation::Desktop,
            |_| {
                calls += 1;
                Ok(Outcome::BadRequest)
            },
            |_| Some(1),
        );
        assert_eq!((got, calls), (Err(RotateError::NoResult), 1));
        assert!(!s.is_bad(Operation::Desktop, b[0]));
        assert_eq!(s.candidates(Operation::Desktop).len(), b.len());
    }

    #[test]
    fn a_saved_hash_from_an_earlier_session_has_not_answered_yet() {
        let b = Operation::Desktop.builtin();
        let (s, _) = store([], [("searchDesktop", b[0])]);
        assert!(!s.has_answered(Operation::Desktop, b[0]));
        let _: Result<u8, _> = rotate(
            &s,
            Operation::Desktop,
            |_| Ok(Outcome::BadRequest),
            |_| Some(1),
        );
        assert!(s.is_retired(Operation::Desktop));
    }

    #[test]
    fn a_hash_marked_bad_during_the_rotation_is_not_sent() {
        let (s, _) = store([], []);
        let b = Operation::Desktop.builtin();
        let mut sent = Vec::new();
        let got = rotate(
            &s,
            Operation::Desktop,
            |hash| {
                sent.push(hash.to_string());
                if sent.len() == 1 {
                    // As another thread would, while this rotation is under way.
                    s.mark_bad(Operation::Desktop, b[1]);
                    Ok(Outcome::BadHash)
                } else {
                    Ok(Outcome::Answer(json!({})))
                }
            },
            |_| Some(7),
        );
        assert_eq!(got, Ok(7));
        assert_eq!(sent, vec![b[0], b[2]]);
    }

    #[test]
    fn a_retired_operation_sends_nothing() {
        let (s, _) = store([], []);
        let _: Result<u8, _> = rotate(&s, Operation::Albums, |_| Ok(Outcome::BadHash), |_| Some(1));
        let mut calls = 0;
        let got: Result<u8, _> = rotate(
            &s,
            Operation::Albums,
            |_| {
                calls += 1;
                Ok(Outcome::BadHash)
            },
            |_| Some(1),
        );
        assert_eq!((got, calls), (Err(RotateError::Retired), 0));
    }

    #[test]
    fn working_hash_is_persisted_once() {
        let (s, saved) = store([], []);
        let b = Operation::Tracks.builtin()[0];
        s.mark_working(Operation::Tracks, b);
        s.mark_working(Operation::Tracks, b);
        assert_eq!(saved.lock().len(), 1);
    }

    #[test]
    fn override_file_rules() {
        let good = h(1);
        let text = format!(
            r#"{{"searchDesktop":["{good}","XYZ","{}"],"deleteAll":["{good}"],"searchTracks":"no"}}"#,
            good.to_uppercase()
        );
        let (o, warnings) = parse_overrides(&text);
        assert_eq!(o.get(&Operation::Desktop), Some(&vec![good.clone()]));
        assert!(!o.contains_key(&Operation::Tracks));
        assert_eq!(o.len(), 1);
        assert!(warnings.iter().any(|w| w.contains("deleteAll")));
        assert!(warnings.iter().all(|w| !w.contains("XYZ")));

        let many: Vec<String> = (0..12).map(|n| format!("\"{n:064x}\"")).collect();
        let (o, warnings) = parse_overrides(&format!(r#"{{"searchAlbums":[{}]}}"#, many.join(",")));
        assert_eq!(o[&Operation::Albums].len(), MAX_OVERRIDES);
        assert!(!warnings.is_empty());

        let (o, warnings) = parse_overrides("{ not json");
        assert!(o.is_empty());
        assert_eq!(warnings.len(), 1);
        let (o, _) = parse_overrides("[]");
        assert!(o.is_empty());
    }

    #[test]
    fn a_missing_override_file_is_silent_and_empty() {
        assert!(load_overrides(Path::new("/nonexistent/pathfinder_hashes.json")).is_empty());
    }
}
