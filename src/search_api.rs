//! Search pages served from Spotify's internal API: the first page of every tab from one
//! `searchDesktop` query, later pages of one tab from its own operation, and the rules that
//! decide when the internal path is not used, has failed, or is over for the session.

use std::collections::HashMap;
use std::thread;

use librespot_core::SpotifyUri;
use log::{debug, warn};
use rspotify::model::{FullArtist, FullTrack, SimplifiedAlbum, SimplifiedEpisode, SimplifiedShow};

use crate::internal_api::Area;
use crate::internal_api::error::InternalError;
use crate::internal_api::hashes::{HashStore, Operation, RotateError, rotate};
use crate::internal_api::metadata::{self, Missing};
use crate::internal_api::pathfinder::{self, Outcome, search_variables};
use crate::internal_api::search::{Kind, PAGE, Section, next_offset, read_all, read_section};
use crate::model::playlist::Playlist;
use crate::spotify_api::WebApi;

/// Transient retries of one search query. Fewer than other internal calls get: someone is
/// waiting for the answer, and on a dead link every attempt runs into its timeout.
const QUERY_RETRIES: u32 = 1;

/// The rows of one tab, in the search answer's order.
#[derive(Debug, Clone)]
pub enum Rows {
    Tracks(Vec<FullTrack>),
    Albums(Vec<SimplifiedAlbum>),
    Artists(Vec<FullArtist>),
    Playlists(Vec<Playlist>),
    Shows(Vec<SimplifiedShow>),
    Episodes(Vec<SimplifiedEpisode>),
}

impl Rows {
    pub fn empty(kind: Kind) -> Self {
        match kind {
            Kind::Track => Self::Tracks(Vec::new()),
            Kind::Album => Self::Albums(Vec::new()),
            Kind::Artist => Self::Artists(Vec::new()),
            Kind::Playlist => Self::Playlists(Vec::new()),
            Kind::Show => Self::Shows(Vec::new()),
            Kind::Episode => Self::Episodes(Vec::new()),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Tracks(rows) => rows.len(),
            Self::Albums(rows) => rows.len(),
            Self::Artists(rows) => rows.len(),
            Self::Playlists(rows) => rows.len(),
            Self::Shows(rows) => rows.len(),
            Self::Episodes(rows) => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone)]
pub struct TabPage {
    pub rows: Rows,
    /// Offset of the next page; `None` = no more.
    pub next_offset: Option<usize>,
    pub total: usize,
}

pub enum FirstPage {
    Internal(HashMap<Kind, TabPage>),
    /// Run today's six Web API searches instead.
    NotInUse,
    /// Leave the tabs empty.
    Failed,
}

pub enum NextPage {
    Page(TabPage),
    /// Nothing added; the same offset can be asked again.
    Failed,
    /// Paging of this view is over (an area latched, or the query was refused).
    Ended,
}

#[derive(Debug, PartialEq)]
pub(crate) enum FirstPageFailure {
    NotInUse,
    Failed,
}

/// Pure: what a failed first-page query means. `true` = latch `Area::Search`.
pub(crate) fn first_page_failure(err: &RotateError) -> (FirstPageFailure, bool) {
    match err {
        RotateError::Retired | RotateError::Failed(InternalError::ProtocolChanged(_)) => {
            (FirstPageFailure::NotInUse, true)
        }
        RotateError::Failed(InternalError::NoSession | InternalError::Denied) => {
            (FirstPageFailure::NotInUse, false)
        }
        RotateError::NoResult
        | RotateError::Failed(
            InternalError::RateLimited | InternalError::Transient(_) | InternalError::NotFound,
        ) => (FirstPageFailure::Failed, false),
    }
}

/// What the page logic needs from the outside, so tests can stand in for the
/// session. `WebApi` implements it with the real calls.
pub(crate) trait SearchBackend {
    /// Whether `area` is on and not latched right now.
    fn usable(&self, area: Area) -> bool;
    /// Send one query (one internal call in `Area::Search`, not counted as `internal_ok`:
    /// the page logic counts with [Self::count_ok] once an answer was read).
    fn send(
        &self,
        op: Operation,
        hash: &str,
        variables: &serde_json::Value,
    ) -> Result<Outcome, InternalError>;
    /// The rows of `uris` for `kind` (one `run_internal(Area::Metadata, ..)` call).
    /// Never called for `Kind::Playlist` or with no uris.
    fn fetch(&self, kind: Kind, uris: &[String]) -> Result<Rows, InternalError>;
    fn latch(&self, area: Area, err: &InternalError);
    fn count_ok(&self, area: Area);
    fn count_fallback(&self, area: Area);
}

impl SearchBackend for WebApi {
    fn usable(&self, area: Area) -> bool {
        self.internal_usable(area)
    }

    fn send(
        &self,
        op: Operation,
        hash: &str,
        variables: &serde_json::Value,
    ) -> Result<Outcome, InternalError> {
        let name = op.name();
        self.run_internal_uncounted(Area::Search, QUERY_RETRIES, |session| {
            pathfinder::query(session, name, hash.to_string(), variables.clone())
        })
    }

    fn fetch(&self, kind: Kind, uris: &[String]) -> Result<Rows, InternalError> {
        let uris: Vec<SpotifyUri> = uris
            .iter()
            .filter_map(|uri| SpotifyUri::from_uri(uri).ok())
            .collect();
        if uris.is_empty() {
            return Ok(Rows::empty(kind));
        }
        let area = Area::Metadata;
        match kind {
            Kind::Track => self
                .run_internal(area, |s| metadata::tracks_by_uri(s, uris.clone()))
                .map(Rows::Tracks),
            Kind::Album => self
                .run_internal(area, |s| {
                    metadata::albums_by_uri(s, uris.clone(), Missing::Skip)
                })
                .map(Rows::Albums),
            Kind::Artist => self
                .run_internal(area, |s| metadata::artists_by_uri(s, uris.clone()))
                .map(Rows::Artists),
            Kind::Show => self
                .run_internal(area, |s| metadata::shows_by_uri(s, uris.clone()))
                .map(Rows::Shows),
            Kind::Episode => self
                .run_internal(area, |s| {
                    metadata::episodes_by_uri(s, uris.clone(), Missing::Skip)
                })
                .map(Rows::Episodes),
            // Playlist rows come from the answer itself.
            Kind::Playlist => Ok(Rows::empty(kind)),
        }
    }

    fn latch(&self, area: Area, err: &InternalError) {
        self.latch_area(area, err);
    }

    fn count_ok(&self, area: Area) {
        self.count_internal_ok(area);
    }

    fn count_fallback(&self, area: Area) {
        Self::count_fallback(self, area);
    }
}

/// Whether the tab of `kind` needs a metadata fetch for its rows.
fn needs_fetch(kind: Kind, section: &Section) -> bool {
    kind != Kind::Playlist && !section.uris.is_empty()
}

/// `fetch(kind, uris)` returns the rows for those uris or the error of the batch call.
pub(crate) fn build_tab(
    kind: Kind,
    section: &Section,
    offset: usize,
    fetch: impl FnOnce(Kind, &[String]) -> Result<Rows, InternalError>,
) -> Result<TabPage, InternalError> {
    let rows = if kind == Kind::Playlist {
        Rows::Playlists(section.playlists.clone())
    } else if section.uris.is_empty() {
        Rows::empty(kind)
    } else {
        fetch(kind, &section.uris)?
    };
    Ok(TabPage {
        rows,
        next_offset: next_offset(offset, PAGE, section),
        total: section.total,
    })
}

/// Log a failed row fetch for the tab of `kind`; a protocol change latches `Area::Metadata`.
fn rows_failed(b: &impl SearchBackend, kind: Kind, err: &InternalError) {
    if matches!(err, InternalError::ProtocolChanged(_)) {
        b.latch(Area::Metadata, err);
    }
    warn!(
        "internal search: {kind:?} rows could not be fetched ({})",
        err.class()
    );
}

fn retired() -> InternalError {
    InternalError::ProtocolChanged("search hashes retired".into())
}

pub(crate) fn first_page_with(
    b: &(impl SearchBackend + Sync),
    store: &HashStore,
    term: &str,
) -> FirstPage {
    if term.trim().is_empty() || !b.usable(Area::Search) || !b.usable(Area::Metadata) {
        return FirstPage::NotInUse;
    }
    debug!("internal search: first page for {term:?}");

    let op = Operation::Desktop;
    let variables = search_variables(term, 0, PAGE);
    let sections = match rotate(store, op, |hash| b.send(op, hash, &variables), read_all) {
        Ok(sections) => {
            b.count_ok(Area::Search);
            sections
        }
        Err(err) => {
            let (failure, latch) = first_page_failure(&err);
            if latch {
                let cause = match &err {
                    RotateError::Failed(e) => e.clone(),
                    _ => retired(),
                };
                b.latch(Area::Search, &cause);
            }
            if failure == FirstPageFailure::NotInUse {
                b.count_fallback(Area::Search);
            }
            return match failure {
                FirstPageFailure::NotInUse => FirstPage::NotInUse,
                FirstPageFailure::Failed => {
                    warn!("internal search: first page failed ({})", error_class(&err));
                    FirstPage::Failed
                }
            };
        }
    };

    // One thread per tab that needs a metadata fetch; the others are built here.
    let mut built: Vec<(Kind, Result<TabPage, InternalError>)> = Vec::new();
    thread::scope(|scope| {
        let mut running = Vec::new();
        for kind in Kind::ALL {
            let section = &sections[&kind];
            let tab = move || build_tab(kind, section, 0, |k, uris| b.fetch(k, uris));
            if needs_fetch(kind, section) {
                running.push((kind, scope.spawn(tab)));
            } else {
                built.push((kind, tab()));
            }
        }
        for (kind, handle) in running {
            let result = handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            built.push((kind, result));
        }
    });

    let mut tabs = HashMap::new();
    for (kind, result) in built {
        let tab = result.unwrap_or_else(|err| {
            rows_failed(b, kind, &err);
            TabPage {
                rows: Rows::empty(kind),
                next_offset: Some(0),
                total: sections[&kind].total,
            }
        });
        tabs.insert(kind, tab);
    }

    if b.usable(Area::Metadata) {
        FirstPage::Internal(tabs)
    } else {
        b.count_fallback(Area::Search);
        FirstPage::NotInUse
    }
}

/// The section of `kind` from one rotated query of `op`. A `searchDesktop` answer must have
/// all six sections even when one is wanted.
fn query_section(
    b: &impl SearchBackend,
    store: &HashStore,
    op: Operation,
    kind: Kind,
    variables: &serde_json::Value,
) -> Result<Section, RotateError> {
    let send = |hash: &str| b.send(op, hash, variables);
    if op == Operation::Desktop {
        rotate(store, op, send, |v| {
            read_all(v).and_then(|mut all| all.remove(&kind))
        })
    } else {
        rotate(store, op, send, |v| read_section(v, kind))
    }
}

pub(crate) fn next_page_with(
    b: &impl SearchBackend,
    store: &HashStore,
    kind: Kind,
    term: &str,
    offset: usize,
) -> NextPage {
    if !b.usable(Area::Search) || !b.usable(Area::Metadata) {
        return NextPage::Ended;
    }
    debug!("internal search: {kind:?} page at {offset} for {term:?}");

    let variables = search_variables(term, offset, PAGE);
    let op = kind.operation();
    let mut result = query_section(b, store, op, kind, &variables);
    if op != Operation::Desktop && result.as_ref().err() == Some(&RotateError::Retired) {
        result = query_section(b, store, Operation::Desktop, kind, &variables);
    }
    let section = match result {
        Ok(section) => {
            b.count_ok(Area::Search);
            section
        }
        // Sending the same refused query again would be refused again.
        Err(RotateError::NoResult) => {
            warn!("internal search: {kind:?} page refused, paging ends");
            return NextPage::Ended;
        }
        Err(RotateError::Retired) => {
            b.latch(Area::Search, &retired());
            return NextPage::Ended;
        }
        Err(RotateError::Failed(err @ InternalError::ProtocolChanged(_))) => {
            b.latch(Area::Search, &err);
            return NextPage::Ended;
        }
        Err(err) => {
            warn!(
                "internal search: {kind:?} page failed ({})",
                error_class(&err)
            );
            return NextPage::Failed;
        }
    };

    match build_tab(kind, &section, offset, |k, uris| b.fetch(k, uris)) {
        Ok(page) => NextPage::Page(page),
        Err(err) => {
            rows_failed(b, kind, &err);
            if b.usable(Area::Metadata) {
                NextPage::Failed
            } else {
                NextPage::Ended
            }
        }
    }
}

/// A fixed name for a failed query, for the log.
fn error_class(err: &RotateError) -> &'static str {
    match err {
        RotateError::Retired => "retired",
        RotateError::NoResult => "no result",
        RotateError::Failed(err) => err.class(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ConfigValues};
    use crate::internal_api::search;
    use parking_lot::Mutex;
    use serde_json::json;

    type SendFn =
        dyn Fn(Operation, &serde_json::Value) -> Result<Outcome, InternalError> + Send + Sync;
    type FetchFn = dyn Fn(Kind) -> Result<Rows, InternalError> + Send + Sync;

    /// A stand-in for the session: records sends (operation, offset, term), latches, counted
    /// answers and fallbacks.
    struct Fake {
        send: Box<SendFn>,
        fetch: Box<FetchFn>,
        sends: Mutex<Vec<(Operation, u64, String)>>,
        latched: Mutex<Vec<(Area, InternalError)>>,
        oks: Mutex<usize>,
        fallbacks: Mutex<usize>,
    }

    impl Fake {
        fn new(
            send: impl Fn(Operation, &serde_json::Value) -> Result<Outcome, InternalError>
            + Send
            + Sync
            + 'static,
            fetch: impl Fn(Kind) -> Result<Rows, InternalError> + Send + Sync + 'static,
        ) -> Self {
            Self {
                send: Box::new(send),
                fetch: Box::new(fetch),
                sends: Mutex::new(Vec::new()),
                latched: Mutex::new(Vec::new()),
                oks: Mutex::new(0),
                fallbacks: Mutex::new(0),
            }
        }

        fn latched_areas(&self) -> Vec<Area> {
            self.latched.lock().iter().map(|(area, _)| *area).collect()
        }

        /// (counted answers, fallbacks)
        fn counts(&self) -> (usize, usize) {
            (*self.oks.lock(), *self.fallbacks.lock())
        }
    }

    impl SearchBackend for Fake {
        fn usable(&self, area: Area) -> bool {
            !self.latched.lock().iter().any(|(a, _)| *a == area)
        }

        fn send(
            &self,
            op: Operation,
            _hash: &str,
            variables: &serde_json::Value,
        ) -> Result<Outcome, InternalError> {
            let offset = variables["offset"].as_u64().unwrap();
            let term = variables["searchTerm"].as_str().unwrap().to_string();
            self.sends.lock().push((op, offset, term));
            (self.send)(op, variables)
        }

        fn fetch(&self, kind: Kind, uris: &[String]) -> Result<Rows, InternalError> {
            assert!(kind != Kind::Playlist && !uris.is_empty());
            (self.fetch)(kind)
        }

        fn latch(&self, area: Area, err: &InternalError) {
            self.latched.lock().push((area, err.clone()));
        }

        fn count_ok(&self, area: Area) {
            assert_eq!(area, Area::Search);
            *self.oks.lock() += 1;
        }

        fn count_fallback(&self, area: Area) {
            assert_eq!(area, Area::Search);
            *self.fallbacks.lock() += 1;
        }
    }

    fn desktop_answer() -> Outcome {
        Outcome::Answer(search::test_answer())
    }

    fn store() -> HashStore {
        HashStore::default()
    }

    fn playlist(id: &str) -> Playlist {
        Playlist {
            id: id.to_string(),
            name: String::new(),
            owner_id: "owner".to_string(),
            owner_name: None,
            snapshot_id: String::new(),
            num_tracks: 0,
            tracks: None,
            collaborative: false,
            cover_url: None,
            tracks_unknown: true,
            web_loaded: false,
        }
    }

    #[test]
    fn first_page_failure_classes() {
        use FirstPageFailure::*;
        let f = |e| first_page_failure(&RotateError::Failed(e));
        assert_eq!(first_page_failure(&RotateError::Retired), (NotInUse, true));
        assert_eq!(
            f(InternalError::ProtocolChanged("x".into())),
            (NotInUse, true)
        );
        assert_eq!(f(InternalError::NoSession), (NotInUse, false));
        assert_eq!(f(InternalError::Denied), (NotInUse, false));
        assert_eq!(f(InternalError::RateLimited), (Failed, false));
        assert_eq!(f(InternalError::Transient("x".into())), (Failed, false));
        assert_eq!(f(InternalError::NotFound), (Failed, false));
        assert_eq!(first_page_failure(&RotateError::NoResult), (Failed, false));
    }

    #[test]
    fn no_session_is_not_in_use_without_a_latch() {
        let api = WebApi::new(&Config::new_for_test());
        assert!(matches!(
            api.search_first_page("anything"),
            FirstPage::NotInUse
        ));
        assert!(api.internal_usable(Area::Search));
        let c = api.area_counts(Area::Search);
        assert_eq!((c.internal_ok, c.fallback, c.latched), (0, 1, false));
        assert!(matches!(
            api.search_next_page(Kind::Track, "anything", 50),
            NextPage::Failed
        ));
    }

    #[test]
    fn switches_decide_before_anything_is_sent() {
        let off = |toml: &str| {
            let values: ConfigValues = toml::from_str(toml).unwrap();
            WebApi::new(&Config::new_for_test_with_values(values))
        };
        for toml in [
            "internal_api = false",
            "[internal_api_areas]\nsearch = false\n",
            "[internal_api_areas]\nmetadata = false\n",
        ] {
            let api = off(toml);
            assert!(matches!(api.search_first_page("x"), FirstPage::NotInUse));
            assert!(matches!(
                api.search_next_page(Kind::Album, "x", 50),
                NextPage::Ended
            ));
            let c = api.area_counts(Area::Search);
            assert_eq!(
                (c.fallback, c.latched),
                (0, false),
                "decided before run_internal"
            );
        }
        let api = WebApi::new(&Config::new_for_test());
        for blank in ["", "   ", "\t"] {
            assert!(matches!(api.search_first_page(blank), FirstPage::NotInUse));
        }
        assert_eq!(api.area_counts(Area::Search).fallback, 0);
    }

    #[test]
    fn a_latched_area_ends_paging_and_skips_the_first_page() {
        let api = WebApi::new(&Config::new_for_test());
        api.latch_area(Area::Metadata, &InternalError::ProtocolChanged("x".into()));
        assert!(matches!(api.search_first_page("x"), FirstPage::NotInUse));
        assert!(matches!(
            api.search_next_page(Kind::Show, "x", 50),
            NextPage::Ended
        ));
    }

    #[test]
    fn no_results_is_an_internal_answer() {
        let empty = Section {
            uris: vec![],
            playlists: vec![],
            item_count: 0,
            total: 0,
            last_page: None,
        };
        let page = build_tab(Kind::Track, &empty, 0, |_, _| Ok(Rows::Tracks(vec![]))).unwrap();
        assert!(page.rows.is_empty());
        assert_eq!((page.next_offset, page.total), (None, 0));
    }

    #[test]
    fn playlists_need_no_fetch_and_a_failed_fetch_is_reported() {
        let s = Section {
            uris: vec!["spotify:playlist:p1".into()],
            playlists: vec![playlist("p1")],
            item_count: 50,
            total: 180,
            last_page: None,
        };
        let page = build_tab(Kind::Playlist, &s, 50, |_, _| {
            panic!("no fetch for playlists")
        })
        .unwrap();
        assert_eq!((page.rows.len(), page.next_offset), (1, Some(100)));
        let err = build_tab(Kind::Album, &s, 0, |_, _| Err(InternalError::RateLimited));
        assert_eq!(err.unwrap_err(), InternalError::RateLimited);
    }

    #[test]
    fn a_retired_type_operation_pages_through_search_desktop() {
        let fake = Fake::new(
            |op, _| {
                Ok(if op == Operation::Tracks {
                    Outcome::BadHash
                } else {
                    desktop_answer()
                })
            },
            |kind| Ok(Rows::empty(kind)),
        );
        let page = next_page_with(&fake, &store(), Kind::Track, "x", 50);
        assert!(matches!(page, NextPage::Page(_)));
        let last = fake
            .sends
            .lock()
            .last()
            .map(|(op, offset, _)| (*op, *offset));
        assert_eq!(last, Some((Operation::Desktop, 50)));
        assert!(fake.latched_areas().is_empty());
        // One counted answer for the page, not one per hash sent.
        assert_eq!(fake.counts(), (1, 0));
    }

    #[test]
    fn retired_search_desktop_latches_search() {
        let fake = Fake::new(|_, _| Ok(Outcome::BadHash), |kind| Ok(Rows::empty(kind)));
        assert!(matches!(
            first_page_with(&fake, &store(), "x"),
            FirstPage::NotInUse
        ));
        assert_eq!(fake.latched_areas(), [Area::Search]);
        assert_eq!(fake.counts(), (0, 1), "bad hashes are not answers");
        // Per-type retired, then desktop retired: paging ends.
        let fake = Fake::new(|_, _| Ok(Outcome::BadHash), |kind| Ok(Rows::empty(kind)));
        assert!(matches!(
            next_page_with(&fake, &store(), Kind::Album, "x", 50),
            NextPage::Ended
        ));
        assert_eq!(fake.latched_areas(), [Area::Search]);
    }

    #[test]
    fn a_desktop_answer_missing_a_section_is_a_bad_hash_even_for_one_kind() {
        let fake = Fake::new(
            |_, _| Ok(Outcome::Answer(json!({ "episodes": { "items": [] } }))),
            |kind| Ok(Rows::empty(kind)),
        );
        assert!(matches!(
            next_page_with(&fake, &store(), Kind::Episode, "x", 50),
            NextPage::Ended
        ));
        assert_eq!(fake.latched_areas(), [Area::Search]);
    }

    #[test]
    fn a_refused_or_rate_limited_query_fails_without_web_or_latch() {
        for outcome in [
            Ok(Outcome::NoResult),
            Ok(Outcome::Partial(json!({}))),
            Err(InternalError::RateLimited),
            Err(InternalError::Transient("x".into())),
        ] {
            let o = outcome.clone();
            let fake = Fake::new(move |_, _| o.clone(), |kind| Ok(Rows::empty(kind)));
            assert!(matches!(
                first_page_with(&fake, &store(), "x"),
                FirstPage::Failed
            ));
            assert!(fake.latched_areas().is_empty());
            assert_eq!(fake.counts(), (0, 0));
            assert_eq!(fake.sends.lock().len(), 1, "one request, no rotation");
        }
    }

    #[test]
    fn a_refused_next_page_ends_paging_without_web_or_latch() {
        for outcome in [Outcome::NoResult, Outcome::Partial(json!({}))] {
            let o = outcome.clone();
            let fake = Fake::new(move |_, _| Ok(o.clone()), |kind| Ok(Rows::empty(kind)));
            assert!(matches!(
                next_page_with(&fake, &store(), Kind::Track, "x", 50),
                NextPage::Ended
            ));
            assert!(fake.latched_areas().is_empty());
            assert_eq!(fake.counts(), (0, 0));
            assert_eq!(fake.sends.lock().len(), 1, "one request, no rotation");
        }
    }

    #[test]
    fn a_failed_next_page_keeps_its_offset_without_web_or_latch() {
        for outcome in [
            Err(InternalError::RateLimited),
            Err(InternalError::Transient("x".into())),
        ] {
            let o = outcome.clone();
            let fake = Fake::new(move |_, _| o.clone(), |kind| Ok(Rows::empty(kind)));
            assert!(matches!(
                next_page_with(&fake, &store(), Kind::Track, "x", 50),
                NextPage::Failed
            ));
            assert!(fake.latched_areas().is_empty());
            assert_eq!(fake.counts(), (0, 0));
            assert_eq!(fake.sends.lock().len(), 1, "one request, no rotation");
        }
    }

    #[test]
    fn a_protocol_change_on_the_first_page_latches_search() {
        let fake = Fake::new(
            |_, _| Err(InternalError::ProtocolChanged("x".into())),
            |kind| Ok(Rows::empty(kind)),
        );
        assert!(matches!(
            first_page_with(&fake, &store(), "x"),
            FirstPage::NotInUse
        ));
        assert_eq!(
            *fake.latched.lock(),
            [(Area::Search, InternalError::ProtocolChanged("x".into()))]
        );
        assert_eq!(fake.counts(), (0, 1));
    }

    #[test]
    fn an_unreadable_partial_first_page_fails_without_marking_a_hash() {
        let s = store();
        let fake = Fake::new(
            |_, _| Ok(Outcome::Partial(json!({ "tracksV2": null }))),
            |kind| Ok(Rows::empty(kind)),
        );
        assert!(matches!(first_page_with(&fake, &s, "x"), FirstPage::Failed));
        assert!(fake.latched_areas().is_empty());
        assert_eq!(
            s.candidates(Operation::Desktop).len(),
            Operation::Desktop.builtin().len()
        );
    }

    #[test]
    fn the_term_is_sent_verbatim() {
        let term = "artist:x year:2017  y";
        let fake = Fake::new(|_, _| Ok(desktop_answer()), |kind| Ok(Rows::empty(kind)));
        assert!(matches!(
            first_page_with(&fake, &store(), term),
            FirstPage::Internal(_)
        ));
        assert!(matches!(
            next_page_with(&fake, &store(), Kind::Album, term, 50),
            NextPage::Page(_)
        ));
        let sends = fake.sends.lock();
        assert_eq!(sends.len(), 2);
        assert!(sends.iter().all(|(_, _, sent)| sent == term));
        assert_eq!(
            (sends[0].0, sends[0].1, sends[1].0, sends[1].1),
            (Operation::Desktop, 0, Operation::Albums, 50)
        );
    }

    #[test]
    fn a_failed_batch_keeps_the_other_tabs_and_retries_from_zero() {
        let fake = Fake::new(
            |_, _| Ok(desktop_answer()),
            |kind| {
                if kind == Kind::Album {
                    Err(InternalError::RateLimited)
                } else {
                    Ok(Rows::empty(kind))
                }
            },
        );
        let FirstPage::Internal(tabs) = first_page_with(&fake, &store(), "x") else {
            panic!()
        };
        assert_eq!(tabs.len(), 6);
        assert_eq!(tabs[&Kind::Album].next_offset, Some(0));
        assert_eq!(tabs[&Kind::Track].next_offset, Some(50));
        assert_eq!(tabs[&Kind::Playlist].rows.len(), 1);
        assert!(fake.latched_areas().is_empty());
        assert_eq!(fake.counts(), (1, 0));
    }

    #[test]
    fn a_protocol_change_in_a_batch_latches_metadata() {
        let fake = Fake::new(
            |_, _| Ok(desktop_answer()),
            |kind| {
                if kind == Kind::Track {
                    Err(InternalError::ProtocolChanged("x".into()))
                } else {
                    Ok(Rows::empty(kind))
                }
            },
        );
        assert!(matches!(
            first_page_with(&fake, &store(), "x"),
            FirstPage::NotInUse
        ));
        assert_eq!(fake.latched_areas(), [Area::Metadata]);
        // The search answer was read, then the page went to the Web API.
        assert_eq!(fake.counts(), (1, 1));
        let fake = Fake::new(
            |_, _| Ok(desktop_answer()),
            |_| Err(InternalError::ProtocolChanged("x".into())),
        );
        assert!(matches!(
            next_page_with(&fake, &store(), Kind::Track, "x", 50),
            NextPage::Ended
        ));
        assert_eq!(fake.latched_areas(), [Area::Metadata]);
    }

    #[test]
    fn a_session_problem_sends_one_search_to_the_web_api() {
        for err in [InternalError::NoSession, InternalError::Denied] {
            let e = err.clone();
            let fake = Fake::new(move |_, _| Err(e.clone()), |kind| Ok(Rows::empty(kind)));
            assert!(matches!(
                first_page_with(&fake, &store(), "x"),
                FirstPage::NotInUse
            ));
            assert_eq!((*fake.fallbacks.lock(), fake.latched_areas().len()), (1, 0));
        }
    }
}
