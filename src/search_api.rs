//! Search pages served from Spotify's internal API: the first page of every tab from one
//! `searchDesktop` query, later pages of one tab from its own operation, and the rules that
//! decide when the internal path is not used, has failed, or is over for the session.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::thread;

use librespot_core::{Session, SpotifyUri};
use log::{debug, warn};
use rspotify::model::{FullArtist, FullTrack, SimplifiedAlbum, SimplifiedEpisode, SimplifiedShow};
use rspotify::prelude::Id;

use crate::internal_api::Area;
use crate::internal_api::error::InternalError;
use crate::internal_api::hashes::{HashStore, Operation, RotateError, rotate, witness};
use crate::internal_api::metadata::{self, Missing};
use crate::internal_api::pathfinder::{self, Outcome, search_variables};
use crate::internal_api::search::{
    Kind, PAGE, Section, TopResults, next_offset, read_all, read_section, read_top,
};
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

/// One row of the Top tab, as fetched.
#[derive(Debug, Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "a Top tab holds a handful of rows; boxing would save nothing worth an allocation"
)]
pub enum TopRow {
    Track(FullTrack),
    Album(SimplifiedAlbum),
    Artist(FullArtist),
    Playlist(Playlist),
    Show(SimplifiedShow),
    Episode(SimplifiedEpisode),
}

impl TopRow {
    /// `spotify:<kind>:<id>` of the row; `None` when the model carries no id.
    pub fn uri(&self) -> Option<String> {
        match self {
            Self::Track(t) => t.id.as_ref().map(|id| id.uri()),
            Self::Album(a) => a.id.as_ref().map(|id| id.uri()),
            Self::Artist(a) => Some(a.id.uri()),
            Self::Playlist(p) => Some(format!("spotify:playlist:{}", p.id)),
            Self::Show(s) => Some(s.id.uri()),
            Self::Episode(e) => Some(e.id.uri()),
        }
    }
}

/// The rows of `rows` one by one, in order.
fn into_top_rows(rows: Rows) -> Vec<TopRow> {
    match rows {
        Rows::Tracks(rows) => rows.into_iter().map(TopRow::Track).collect(),
        Rows::Albums(rows) => rows.into_iter().map(TopRow::Album).collect(),
        Rows::Artists(rows) => rows.into_iter().map(TopRow::Artist).collect(),
        Rows::Playlists(rows) => rows.into_iter().map(TopRow::Playlist).collect(),
        Rows::Shows(rows) => rows.into_iter().map(TopRow::Show).collect(),
        Rows::Episodes(rows) => rows.into_iter().map(TopRow::Episode).collect(),
    }
}

/// Copies of the first `n` rows of `rows`, in order; the rest is not copied.
fn first_top_rows(rows: &Rows, n: usize) -> Vec<TopRow> {
    match rows {
        Rows::Tracks(r) => r.iter().take(n).cloned().map(TopRow::Track).collect(),
        Rows::Albums(r) => r.iter().take(n).cloned().map(TopRow::Album).collect(),
        Rows::Artists(r) => r.iter().take(n).cloned().map(TopRow::Artist).collect(),
        Rows::Playlists(r) => r.iter().take(n).cloned().map(TopRow::Playlist).collect(),
        Rows::Shows(r) => r.iter().take(n).cloned().map(TopRow::Show).collect(),
        Rows::Episodes(r) => r.iter().take(n).cloned().map(TopRow::Episode).collect(),
    }
}

/// The first `per_kind` rows of each kind's first page, in tab order
/// (Track, Album, Artist, Playlist, Show, Episode). A kind with no rows, or missing from
/// `tabs`, is left out. `per_kind == 0` gives an empty list.
pub fn section_rows(tabs: &HashMap<Kind, TabPage>, per_kind: usize) -> Vec<(Kind, Vec<TopRow>)> {
    if per_kind == 0 {
        return Vec::new();
    }
    Kind::ALL
        .into_iter()
        .filter_map(|kind| {
            let rows = first_top_rows(&tabs.get(&kind)?.rows, per_kind);
            (!rows.is_empty()).then_some((kind, rows))
        })
        .collect()
}

/// One place of the Top tab, in the answer's order.
#[derive(Debug, Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "a Top tab holds a handful of slots; boxing would save nothing worth an allocation"
)]
enum Slot {
    Row(TopRow),
    /// Not among its kind's first page: waits for its own fetch.
    Stray(Kind, String),
    /// Left out for good.
    Dropped,
}

/// The Top tab while it is being put together.
#[derive(Debug, Clone, Default)]
pub struct TopDraft {
    slots: Vec<Slot>,
    featured: Vec<Playlist>,
}

impl TopDraft {
    /// The rows known so far, in the answer's order.
    pub fn rows(&self) -> Vec<TopRow> {
        self.slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Row(row) => Some(row.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn featured(&self) -> &[Playlist] {
        &self.featured
    }

    /// Whether a stray is still waiting for its fetch.
    pub fn has_strays(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| matches!(slot, Slot::Stray(..)))
    }

    /// For tests in other modules: `slots` in order, `Ok(row)` a row at hand, `Err((kind,
    /// uri))` a stray.
    #[cfg(test)]
    pub(crate) fn for_test(
        slots: Vec<Result<TopRow, (Kind, String)>>,
        featured: Vec<Playlist>,
    ) -> Self {
        let slots = slots
            .into_iter()
            .map(|slot| match slot {
                Ok(row) => Slot::Row(row),
                Err((kind, uri)) => Slot::Stray(kind, uri),
            })
            .collect();
        Self { slots, featured }
    }

    /// The uris of the strays of `kind`, in order.
    fn strays_of(&self, kind: Kind) -> Vec<String> {
        self.slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Stray(k, uri) if *k == kind => Some(uri.clone()),
                _ => None,
            })
            .collect()
    }

    /// Fill the strays of `kind` from `rows` (matched by uri) and drop those not in it.
    pub(crate) fn settle(&mut self, kind: Kind, rows: Option<Rows>) {
        let answered = rows.is_some();
        let mut fetched: HashMap<String, TopRow> = rows
            .map(into_top_rows)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| Some((row.uri()?, row)))
            .collect();
        let mut missing = 0;
        for slot in &mut self.slots {
            let Slot::Stray(k, uri) = slot else {
                continue;
            };
            if *k != kind {
                continue;
            }
            *slot = match fetched.remove(uri.as_str()) {
                Some(row) => Slot::Row(row),
                None => {
                    missing += 1;
                    Slot::Dropped
                }
            };
        }
        // A failed fetch is logged by the caller.
        if answered && missing > 0 {
            debug!("internal search: {missing} {kind:?} top results not returned, dropped");
        }
    }
}

/// The Top tab of a first page: `top`'s entries in order, rows taken from the built `tabs`
/// where the entry is among its kind's section, strays otherwise. Entries of a kind in
/// `failed` (its batch failed) are dropped.
fn top_draft(
    top: TopResults,
    sections: &HashMap<Kind, Section>,
    tabs: &HashMap<Kind, TabPage>,
    failed: &HashSet<Kind>,
) -> TopDraft {
    let slots = top
        .items
        .into_iter()
        .map(|entry| {
            if entry.kind == Kind::Playlist {
                return entry
                    .playlist
                    .map_or(Slot::Dropped, |p| Slot::Row(TopRow::Playlist(p)));
            }
            if failed.contains(&entry.kind) {
                return Slot::Dropped;
            }
            if sections[&entry.kind].uris.contains(&entry.uri) {
                return into_top_rows(tabs[&entry.kind].rows.clone())
                    .into_iter()
                    .find(|row| row.uri().as_deref() == Some(entry.uri.as_str()))
                    .map_or(Slot::Dropped, Slot::Row);
            }
            Slot::Stray(entry.kind, entry.uri)
        })
        .collect();
    TopDraft {
        slots,
        featured: top.featured,
    }
}

pub enum FirstPage {
    Internal {
        tabs: HashMap<Kind, TabPage>,
        top: TopDraft,
    },
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
        // Every `searchDesktop` hash drew an HTTP 400: hashes or request, nothing here can
        // tell. This one search goes to the Web API and the next one asks again.
        RotateError::Rejected(_) => (FirstPageFailure::NotInUse, false),
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
    /// the page logic counts with [Self::count_ok] once an answer was read) A transient
    /// failure is sent again at most `retries` times.
    fn send(
        &self,
        op: Operation,
        hash: &str,
        variables: &serde_json::Value,
        retries: u32,
    ) -> Result<Outcome, InternalError>;
    /// The rows of `uris` for `kind` (one `run_internal(Area::Metadata, ..)` call).
    /// Never called for `Kind::Playlist` or with no uris.
    fn fetch(&self, kind: Kind, uris: &[String]) -> Result<Rows, InternalError>;
    /// Like [Self::fetch], for rows only the Top tab needs: at most one retry on a transient
    /// failure, and no waiting for a rate-limit window (a rate-limit answer or an open window
    /// fails at once with `RateLimited`). Never called for `Kind::Playlist` or with no uris.
    fn fetch_stray(&self, kind: Kind, uris: &[String]) -> Result<Rows, InternalError>;
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
        retries: u32,
    ) -> Result<Outcome, InternalError> {
        let name = op.name();
        self.run_internal_uncounted(Area::Search, retries, |session| {
            pathfinder::query(session, name, hash.to_string(), variables.clone())
        })
    }

    fn fetch(&self, kind: Kind, uris: &[String]) -> Result<Rows, InternalError> {
        metadata_rows(self, kind, uris, false)
    }

    fn fetch_stray(&self, kind: Kind, uris: &[String]) -> Result<Rows, InternalError> {
        metadata_rows(self, kind, uris, true)
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

/// One call in `Area::Metadata`: through [WebApi::run_internal_stray] when `stray`, else
/// through [WebApi::run_internal].
fn run_metadata<T, F, Fut>(api: &WebApi, stray: bool, internal: F) -> Result<T, InternalError>
where
    F: Fn(Session) -> Fut,
    Fut: Future<Output = Result<T, InternalError>> + Send + 'static,
    T: Send + 'static,
{
    if stray {
        api.run_internal_stray(Area::Metadata, internal)
    } else {
        api.run_internal(Area::Metadata, internal)
    }
}

/// The rows of `uris` for `kind` from one metadata call (see [run_metadata] for `stray`).
fn metadata_rows(
    api: &WebApi,
    kind: Kind,
    uris: &[String],
    stray: bool,
) -> Result<Rows, InternalError> {
    let uris: Vec<SpotifyUri> = uris
        .iter()
        .filter_map(|uri| SpotifyUri::from_uri(uri).ok())
        .collect();
    if uris.is_empty() {
        return Ok(Rows::empty(kind));
    }
    match kind {
        Kind::Track => {
            run_metadata(api, stray, |s| metadata::tracks_by_uri(s, uris.clone())).map(Rows::Tracks)
        }
        Kind::Album => run_metadata(api, stray, |s| {
            metadata::albums_by_uri(s, uris.clone(), Missing::Skip)
        })
        .map(Rows::Albums),
        Kind::Artist => run_metadata(api, stray, |s| metadata::artists_by_uri(s, uris.clone()))
            .map(Rows::Artists),
        Kind::Show => {
            run_metadata(api, stray, |s| metadata::shows_by_uri(s, uris.clone())).map(Rows::Shows)
        }
        Kind::Episode => run_metadata(api, stray, |s| {
            metadata::episodes_by_uri(s, uris.clone(), Missing::Skip)
        })
        .map(Rows::Episodes),
        // Playlist rows come from the answer itself.
        Kind::Playlist => Ok(Rows::empty(kind)),
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
    let (sections, top) = match rotate(
        store,
        op,
        |hash| b.send(op, hash, &variables, QUERY_RETRIES),
        |answer| read_all(answer).map(|sections| (sections, read_top(answer))),
    ) {
        Ok(read) => {
            b.count_ok(Area::Search);
            read
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
    let mut failed = HashSet::new();
    for (kind, result) in built {
        let tab = result.unwrap_or_else(|err| {
            rows_failed(b, kind, &err);
            failed.insert(kind);
            TabPage {
                rows: Rows::empty(kind),
                next_offset: Some(0),
                total: sections[&kind].total,
            }
        });
        tabs.insert(kind, tab);
    }

    if b.usable(Area::Metadata) {
        let top = top_draft(top, &sections, &tabs, &failed);
        FirstPage::Internal { tabs, top }
    } else {
        b.count_fallback(Area::Search);
        FirstPage::NotInUse
    }
}

/// Fetch the strays of `draft`: one `fetch_stray` per kind that has any, all kinds at the
/// same time. A failed fetch drops that kind's strays and is logged at `warn` with kind
/// and error class; it never latches and never counts a fallback. Afterwards
/// `has_strays()` is false.
pub(crate) fn fetch_strays_with(b: &(impl SearchBackend + Sync), draft: &mut TopDraft) {
    let wanted: Vec<(Kind, Vec<String>)> = Kind::ALL
        .into_iter()
        .filter(|kind| *kind != Kind::Playlist)
        .map(|kind| (kind, draft.strays_of(kind)))
        .filter(|(_, uris)| !uris.is_empty())
        .collect();
    let fetched: Vec<(Kind, Result<Rows, InternalError>)> = thread::scope(|scope| {
        let running: Vec<_> = wanted
            .iter()
            .map(|(kind, uris)| (*kind, scope.spawn(move || b.fetch_stray(*kind, uris))))
            .collect();
        running
            .into_iter()
            .map(|(kind, handle)| {
                let result = handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                (kind, result)
            })
            .collect()
    });
    for (kind, result) in fetched {
        match result {
            Ok(rows) => draft.settle(kind, Some(rows)),
            Err(err) => {
                warn!(
                    "internal search: {kind:?} top results could not be fetched ({})",
                    err.class()
                );
                draft.settle(kind, None);
            }
        }
    }
}

/// The section of `kind` from one rotated query of `op`, and whether it came from a plain
/// answer (one without errors). A `searchDesktop` answer must have all six sections even
/// when one is wanted.
fn query_section(
    b: &impl SearchBackend,
    store: &HashStore,
    op: Operation,
    kind: Kind,
    variables: &serde_json::Value,
) -> Result<(Section, bool), RotateError> {
    // The rotation returns on the send that answered, so the last send decides.
    let plain = Cell::new(false);
    let send = |hash: &str| {
        let outcome = b.send(op, hash, variables, QUERY_RETRIES);
        plain.set(matches!(outcome, Ok(Outcome::Answer(_))));
        outcome
    };
    let section = if op == Operation::Desktop {
        rotate(store, op, send, |v| {
            read_all(v).and_then(|mut all| all.remove(&kind))
        })
    } else {
        rotate(store, op, send, |v| read_section(v, kind))
    }?;
    Ok((section, plain.get()))
}

/// A number that stands for one request, so the hash store can tell requests apart without
/// keeping the search term.
fn request_key(variables: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    variables.to_string().hash(&mut hasher);
    hasher.finish()
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
    let section_of = |op| query_section(b, store, op, kind, &variables).map(|(section, _)| section);
    let mut result = section_of(op);
    if op != Operation::Desktop {
        match result {
            Err(RotateError::Retired) => result = section_of(Operation::Desktop),
            // The hashes of the operation drew an HTTP 400 and none of its own answered.
            // `searchDesktop` gets the same page. A plain answer counts against those
            // hashes (the store marks them bad after two such requests); an answer with
            // errors only supplies the page; a refusal leaves the request as the suspect.
            Err(RotateError::Rejected(suspects)) => {
                let desktop = query_section(b, store, Operation::Desktop, kind, &variables);
                if matches!(desktop, Ok((_, true))) {
                    witness(store, op, &suspects, request_key(&variables));
                }
                result = desktop.map(|(section, _)| section);
            }
            _ => {}
        }
    }
    let section = match result {
        Ok(section) => {
            b.count_ok(Area::Search);
            section
        }
        // Sending the same refused query again would be refused again.
        Err(RotateError::NoResult | RotateError::Rejected(_)) => {
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
        RotateError::Rejected(_) => "rejected",
        RotateError::Failed(err) => err.class(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ConfigValues};
    use crate::internal_api::search;
    use parking_lot::Mutex;
    use rspotify::model::{ShowId, TrackId, Type};
    use serde_json::json;
    use std::sync::Arc;

    type SendFn =
        dyn Fn(Operation, &serde_json::Value) -> Result<Outcome, InternalError> + Send + Sync;
    type FetchFn = dyn Fn(Kind, &[String]) -> Result<Rows, InternalError> + Send + Sync;

    /// A stand-in for the session: records sends (operation, offset, term), fetches, stray
    /// fetches, latches, counted answers and fallbacks.
    struct Fake {
        send: Box<SendFn>,
        fetch: Arc<FetchFn>,
        /// Answers `fetch_stray`; the same closure as `fetch` unless set apart.
        stray: Arc<FetchFn>,
        sends: Mutex<Vec<(Operation, u64, String)>>,
        fetches: Mutex<Vec<(Kind, Vec<String>)>>,
        stray_fetches: Mutex<Vec<(Kind, Vec<String>)>>,
        retries: Mutex<Vec<u32>>,
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
            fetch: impl Fn(Kind, &[String]) -> Result<Rows, InternalError> + Send + Sync + 'static,
        ) -> Self {
            let fetch: Arc<FetchFn> = Arc::new(fetch);
            Self {
                send: Box::new(send),
                stray: Arc::clone(&fetch),
                fetch,
                sends: Mutex::new(Vec::new()),
                fetches: Mutex::new(Vec::new()),
                stray_fetches: Mutex::new(Vec::new()),
                retries: Mutex::new(Vec::new()),
                latched: Mutex::new(Vec::new()),
                oks: Mutex::new(0),
                fallbacks: Mutex::new(0),
            }
        }

        /// Answer `fetch_stray` with `stray` instead of the `fetch` closure.
        fn with_stray(
            mut self,
            stray: impl Fn(Kind, &[String]) -> Result<Rows, InternalError> + Send + Sync + 'static,
        ) -> Self {
            self.stray = Arc::new(stray);
            self
        }

        fn fetched_uris(&self) -> Vec<String> {
            let fetches = self.fetches.lock();
            fetches.iter().flat_map(|(_, uris)| uris.clone()).collect()
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
            retries: u32,
        ) -> Result<Outcome, InternalError> {
            self.retries.lock().push(retries);
            let offset = variables["offset"].as_u64().unwrap();
            let term = variables["searchTerm"].as_str().unwrap().to_string();
            self.sends.lock().push((op, offset, term));
            (self.send)(op, variables)
        }

        fn fetch(&self, kind: Kind, uris: &[String]) -> Result<Rows, InternalError> {
            assert!(kind != Kind::Playlist && !uris.is_empty());
            self.fetches.lock().push((kind, uris.to_vec()));
            (self.fetch)(kind, uris)
        }

        fn fetch_stray(&self, kind: Kind, uris: &[String]) -> Result<Rows, InternalError> {
            assert!(kind != Kind::Playlist && !uris.is_empty());
            self.stray_fetches.lock().push((kind, uris.to_vec()));
            (self.stray)(kind, uris)
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
        assert_eq!(
            first_page_failure(&RotateError::Rejected(Vec::new())),
            (NotInUse, false)
        );
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
            |kind, _| Ok(Rows::empty(kind)),
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

    /// A backend where `searchTracks` draws an HTTP 400 and `searchDesktop` gives `desktop()`.
    fn tracks_refused(
        desktop: impl Fn() -> Result<Outcome, InternalError> + Send + Sync + 'static,
    ) -> Fake {
        Fake::new(
            move |op, _| {
                if op == Operation::Tracks {
                    Ok(Outcome::BadRequest)
                } else {
                    desktop()
                }
            },
            |kind, _| Ok(Rows::empty(kind)),
        )
    }

    fn sent_ops(fake: &Fake) -> Vec<Operation> {
        fake.sends.lock().iter().map(|(op, _, _)| *op).collect()
    }

    #[test]
    fn a_400_on_a_type_operation_is_a_bad_hash_once_search_desktop_answers_two_pages() {
        use Operation::{Desktop, Tracks};
        let fake = tracks_refused(|| Ok(desktop_answer()));
        let store = store();
        let page = next_page_with(&fake, &store, Kind::Track, "x", 50);
        assert!(matches!(page, NextPage::Page(_)));
        assert_eq!(sent_ops(&fake), vec![Tracks, Desktop]);
        // One request may be something only `searchTracks` refuses.
        assert!(!store.is_retired(Tracks));
        // The same page again is not a second request.
        next_page_with(&fake, &store, Kind::Track, "x", 50);
        assert!(!store.is_retired(Tracks));

        let page = next_page_with(&fake, &store, Kind::Track, "x", 100);
        assert!(matches!(page, NextPage::Page(_)));
        assert!(store.is_retired(Tracks));
        assert!(fake.latched_areas().is_empty());
        assert_eq!(fake.counts(), (3, 0));

        // Later pages go straight to `searchDesktop`.
        let page = next_page_with(&fake, &store, Kind::Track, "x", 150);
        assert!(matches!(page, NextPage::Page(_)));
        assert_eq!(
            sent_ops(&fake),
            vec![Tracks, Desktop, Tracks, Desktop, Tracks, Desktop, Desktop]
        );
    }

    #[test]
    fn a_search_desktop_answer_with_errors_supplies_the_page_but_is_no_witness() {
        let fake = tracks_refused(|| Ok(Outcome::Partial(search::test_answer())));
        let store = store();
        for offset in [50, 100, 150] {
            let page = next_page_with(&fake, &store, Kind::Track, "x", offset);
            assert!(matches!(page, NextPage::Page(_)));
        }
        assert!(!store.is_retired(Operation::Tracks));
        assert_eq!(fake.counts(), (3, 0));
    }

    #[test]
    fn a_400_from_the_type_operation_and_search_desktop_ends_paging_and_marks_nothing() {
        let fake = Fake::new(
            |_, _| Ok(Outcome::BadRequest),
            |kind, _| Ok(Rows::empty(kind)),
        );
        let store = store();
        for offset in [50, 100] {
            assert!(matches!(
                next_page_with(&fake, &store, Kind::Track, "x", offset),
                NextPage::Ended
            ));
        }
        assert!(!store.is_retired(Operation::Tracks) && !store.is_retired(Operation::Desktop));
        assert_eq!(
            store.candidates(Operation::Desktop).len(),
            Operation::Desktop.builtin().len()
        );
        // Per page: the one `searchTracks` hash, then every `searchDesktop` hash.
        let mut per_page = vec![Operation::Tracks];
        per_page.extend(
            Operation::Desktop
                .builtin()
                .iter()
                .map(|_| Operation::Desktop),
        );
        assert_eq!(sent_ops(&fake), [per_page.clone(), per_page].concat());
        assert!(fake.latched_areas().is_empty());
        assert_eq!(fake.counts(), (0, 0));
    }

    #[test]
    fn a_refusing_search_desktop_after_a_type_400_marks_no_type_hash() {
        // (what `searchDesktop` gives, the page outcome is Ended, Search latches)
        type Desktop = fn() -> Result<Outcome, InternalError>;
        let cases: [(Desktop, bool, bool); 4] = [
            (|| Ok(Outcome::NoResult), true, false),
            // An answer with errors that cannot be read.
            (|| Ok(Outcome::Partial(json!({}))), true, false),
            (|| Err(InternalError::Transient("x".into())), false, false),
            // Every `searchDesktop` hash unknown: search is over for the session.
            (|| Ok(Outcome::BadHash), true, true),
        ];
        for (at, (desktop, ended, latched)) in cases.into_iter().enumerate() {
            let fake = tracks_refused(desktop);
            let store = store();
            for offset in [50, 100] {
                let page = next_page_with(&fake, &store, Kind::Track, "x", offset);
                assert_eq!(matches!(page, NextPage::Ended), ended, "case {at}");
                assert_eq!(matches!(page, NextPage::Failed), !ended, "case {at}");
            }
            assert!(!store.is_retired(Operation::Tracks), "case {at}");
            assert_eq!(!fake.latched_areas().is_empty(), latched, "case {at}");
            assert_eq!(fake.counts(), (0, 0), "case {at}");
        }
    }

    #[test]
    fn a_400_from_every_search_desktop_hash_sends_that_search_to_the_web_api_without_a_latch() {
        let fake = Fake::new(
            |_, _| Ok(Outcome::BadRequest),
            |kind, _| Ok(Rows::empty(kind)),
        );
        let store = store();
        assert!(matches!(
            first_page_with(&fake, &store, "x"),
            FirstPage::NotInUse
        ));
        assert!(fake.latched_areas().is_empty());
        assert_eq!(fake.counts(), (0, 1));
        // Nothing was marked: the next search asks every hash again.
        let hashes = Operation::Desktop.builtin().len();
        assert_eq!(store.candidates(Operation::Desktop).len(), hashes);
        assert_eq!(fake.sends.lock().len(), hashes);
        assert!(matches!(
            first_page_with(&fake, &store, "y"),
            FirstPage::NotInUse
        ));
        assert_eq!(fake.sends.lock().len(), 2 * hashes);
        assert_eq!(fake.counts(), (0, 2));
    }

    #[test]
    fn only_the_search_desktop_hash_that_answered_decides_whether_the_answer_was_plain() {
        // Per page `searchDesktop` gives a plain answer that cannot be read (that hash is
        // marked bad), then a readable answer with errors from the next hash.
        let desktop_sends = Mutex::new(0);
        let fake = Fake::new(
            move |op, _| {
                if op == Operation::Tracks {
                    return Ok(Outcome::BadRequest);
                }
                let mut sends = desktop_sends.lock();
                *sends += 1;
                Ok(if *sends == 1 {
                    Outcome::Answer(json!({}))
                } else {
                    Outcome::Partial(search::test_answer())
                })
            },
            |kind, _| Ok(Rows::empty(kind)),
        );
        let store = store();
        for offset in [50, 100, 150] {
            let page = next_page_with(&fake, &store, Kind::Track, "x", offset);
            assert!(matches!(page, NextPage::Page(_)));
        }
        // An answer with errors is no witness, whatever an earlier hash sent.
        assert!(!store.is_retired(Operation::Tracks));
    }

    #[test]
    fn retired_search_desktop_latches_search() {
        let fake = Fake::new(|_, _| Ok(Outcome::BadHash), |kind, _| Ok(Rows::empty(kind)));
        assert!(matches!(
            first_page_with(&fake, &store(), "x"),
            FirstPage::NotInUse
        ));
        assert_eq!(fake.latched_areas(), [Area::Search]);
        assert_eq!(fake.counts(), (0, 1), "bad hashes are not answers");
        // Per-type retired, then desktop retired: paging ends.
        let fake = Fake::new(|_, _| Ok(Outcome::BadHash), |kind, _| Ok(Rows::empty(kind)));
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
            |kind, _| Ok(Rows::empty(kind)),
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
            let fake = Fake::new(move |_, _| o.clone(), |kind, _| Ok(Rows::empty(kind)));
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
            let fake = Fake::new(move |_, _| Ok(o.clone()), |kind, _| Ok(Rows::empty(kind)));
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
            let fake = Fake::new(move |_, _| o.clone(), |kind, _| Ok(Rows::empty(kind)));
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
    fn every_search_query_asks_for_one_transient_retry() {
        let fake = Fake::new(
            |_, _| Err(InternalError::Transient("x".into())),
            |kind, _| Ok(Rows::empty(kind)),
        );
        assert!(matches!(
            first_page_with(&fake, &store(), "x"),
            FirstPage::Failed
        ));
        assert!(matches!(
            next_page_with(&fake, &store(), Kind::Track, "x", 50),
            NextPage::Failed
        ));
        assert!(matches!(
            next_page_with(&fake, &store(), Kind::Episode, "x", 50),
            NextPage::Failed
        ));
        assert_eq!(*fake.retries.lock(), vec![1, 1, 1]);
    }

    #[test]
    fn a_protocol_change_on_the_first_page_latches_search() {
        let fake = Fake::new(
            |_, _| Err(InternalError::ProtocolChanged("x".into())),
            |kind, _| Ok(Rows::empty(kind)),
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
            |kind, _| Ok(Rows::empty(kind)),
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
        let fake = Fake::new(|_, _| Ok(desktop_answer()), |kind, _| Ok(Rows::empty(kind)));
        assert!(matches!(
            first_page_with(&fake, &store(), term),
            FirstPage::Internal { .. }
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
            |kind, _| {
                if kind == Kind::Album {
                    Err(InternalError::RateLimited)
                } else {
                    Ok(Rows::empty(kind))
                }
            },
        );
        let FirstPage::Internal { tabs, .. } = first_page_with(&fake, &store(), "x") else {
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
            |kind, _| {
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
            |_, _| Err(InternalError::ProtocolChanged("x".into())),
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
            let fake = Fake::new(move |_, _| Err(e.clone()), |kind, _| Ok(Rows::empty(kind)));
            assert!(matches!(
                first_page_with(&fake, &store(), "x"),
                FirstPage::NotInUse
            ));
            assert_eq!((*fake.fallbacks.lock(), fake.latched_areas().len()), (1, 0));
        }
    }

    /// A track row with nothing set but the id and name, as `internal_api::convert` builds one.
    #[allow(deprecated)] // rspotify marks fields Spotify removed as deprecated; they must be set.
    fn full_track(id: &str) -> FullTrack {
        FullTrack {
            album: SimplifiedAlbum::default(),
            artists: Vec::new(),
            available_markets: Vec::new(),
            disc_number: 0,
            duration: chrono::Duration::zero(),
            explicit: false,
            external_ids: HashMap::new(),
            external_urls: HashMap::new(),
            href: None,
            id: Some(TrackId::from_id(id.to_string()).unwrap()),
            is_local: false,
            is_playable: None,
            linked_from: None,
            restrictions: None,
            name: id.to_string(),
            popularity: 0,
            preview_url: None,
            track_number: 0,
            r#type: Type::Track,
        }
    }

    /// A show row with nothing set but the id and name, as `internal_api::convert` builds one.
    #[allow(deprecated)] // rspotify marks fields Spotify removed as deprecated; they must be set.
    fn show_row(id: &str) -> SimplifiedShow {
        SimplifiedShow {
            available_markets: Vec::new(),
            copyrights: Vec::new(),
            description: String::new(),
            explicit: false,
            external_urls: HashMap::new(),
            href: String::new(),
            id: ShowId::from_id(id.to_string()).unwrap(),
            images: Vec::new(),
            is_externally_hosted: None,
            languages: Vec::new(),
            media_type: String::new(),
            name: id.to_string(),
            publisher: String::new(),
        }
    }

    /// An album row with nothing set but its id.
    fn album_fixture(id: &str) -> SimplifiedAlbum {
        SimplifiedAlbum {
            id: Some(rspotify::model::AlbumId::from_id(id.to_string()).unwrap()),
            ..Default::default()
        }
    }

    /// An episode row with nothing set but the id and name.
    #[allow(deprecated)] // rspotify marks fields Spotify removed as deprecated; they must be set.
    fn episode_fixture(id: &str) -> SimplifiedEpisode {
        SimplifiedEpisode {
            audio_preview_url: None,
            description: String::new(),
            duration: chrono::Duration::zero(),
            explicit: false,
            external_urls: HashMap::new(),
            href: String::new(),
            id: rspotify::model::EpisodeId::from_id(id.to_string()).unwrap(),
            images: Vec::new(),
            is_externally_hosted: false,
            is_playable: true,
            language: String::new(),
            languages: Vec::new(),
            name: id.to_string(),
            release_date: String::new(),
            release_date_precision: rspotify::model::DatePrecision::Day,
            resume_point: None,
        }
    }

    fn page_of(rows: Rows) -> TabPage {
        TabPage {
            rows,
            next_offset: None,
            total: 0,
        }
    }

    fn section_pages() -> HashMap<Kind, TabPage> {
        let ids = |prefix: &str, n: usize| -> Vec<String> {
            (0..n).map(|i| format!("{prefix}{i}")).collect()
        };
        HashMap::from([
            (
                Kind::Track,
                page_of(Rows::Tracks(
                    ids("t", 5).iter().map(|i| full_track(i)).collect(),
                )),
            ),
            (
                Kind::Album,
                page_of(Rows::Albums(
                    ids("b", 2).iter().map(|i| album_fixture(i)).collect(),
                )),
            ),
            (Kind::Artist, page_of(Rows::Artists(Vec::new()))),
            (
                Kind::Playlist,
                page_of(Rows::Playlists(
                    ids("p", 4).iter().map(|i| playlist(i)).collect(),
                )),
            ),
            (
                Kind::Episode,
                page_of(Rows::Episodes(vec![episode_fixture("e0")])),
            ),
        ])
    }

    #[test]
    fn section_rows_take_the_first_rows_of_each_kind_in_tab_order() {
        let pages = section_pages();
        let sections = section_rows(&pages, 3);
        let kinds: Vec<Kind> = sections.iter().map(|(k, _)| *k).collect();
        let lens: Vec<usize> = sections.iter().map(|(_, rows)| rows.len()).collect();
        assert_eq!(
            kinds,
            [Kind::Track, Kind::Album, Kind::Playlist, Kind::Episode]
        );
        assert_eq!(lens, [3, 2, 3, 1]);
        for (kind, rows) in &sections {
            let all = into_top_rows(pages[kind].rows.clone());
            for (at, row) in rows.iter().enumerate() {
                assert_eq!(row.uri(), all[at].uri(), "{kind:?} row {at}");
            }
        }
    }

    #[test]
    fn section_rows_with_zero_or_a_huge_count() {
        let pages = section_pages();
        assert!(section_rows(&pages, 0).is_empty());
        let lens: Vec<usize> = section_rows(&pages, 1000)
            .iter()
            .map(|(_, rows)| rows.len())
            .collect();
        assert_eq!(lens, [5, 2, 4, 1]);
    }

    /// A fetch that answers with exactly the track and show rows of the uris it was asked;
    /// the fixtures need no rows of the other kinds.
    fn rows_for(kind: Kind, uris: &[String]) -> Result<Rows, InternalError> {
        let ids = uris.iter().map(|uri| uri.rsplit(':').next().unwrap());
        Ok(match kind {
            Kind::Track => Rows::Tracks(ids.map(full_track).collect()),
            Kind::Show => Rows::Shows(ids.map(show_row).collect()),
            _ => Rows::empty(kind),
        })
    }

    /// `desktop_answer()` with no shows, and a top section of `t1` (in the tracks section),
    /// `t9` (not in it), `p1`, `s9`, and one featured playlist.
    fn top_fixture() -> serde_json::Value {
        let mut v = search::test_answer();
        v["podcasts"] = json!({ "totalCount": 0, "items": [] });
        let playlist = |id: &str| {
            json!({ "uri": format!("spotify:playlist:{id}"), "name": id,
                "ownerV2": { "data": { "name": "Owner", "username": "owner1" } } })
        };
        let entry = |data: serde_json::Value| json!({ "item": { "data": data } });
        let uri = |uri: &str| entry(json!({ "uri": uri }));
        v["topResultsV2"] = json!({
            "itemsV2": [
                uri("spotify:track:t1"),
                uri("spotify:track:t9"),
                entry(playlist("p1")),
                uri("spotify:show:s9"),
            ],
            "featured": [ { "data": playlist("pf1") } ],
        });
        v
    }

    fn top_fake() -> Fake {
        Fake::new(|_, _| Ok(Outcome::Answer(top_fixture())), rows_for)
    }

    fn uris(rows: &[TopRow]) -> Vec<String> {
        rows.iter().map(|row| row.uri().unwrap()).collect()
    }

    fn first_top(fake: &Fake) -> (HashMap<Kind, TabPage>, TopDraft) {
        match first_page_with(fake, &store(), "x") {
            FirstPage::Internal { tabs, top } => (tabs, top),
            _ => panic!("not an internal first page"),
        }
    }

    const T1: &str = "spotify:track:t1";
    const T8: &str = "spotify:track:t8";
    const T9: &str = "spotify:track:t9";
    const P1: &str = "spotify:playlist:p1";
    const S9: &str = "spotify:show:s9";

    #[test]
    fn the_first_page_carries_top_rows_taken_from_the_tabs() {
        let fake = top_fake();
        let (_, top) = first_top(&fake);
        assert_eq!(uris(&top.rows()), [T1, P1]);
        assert!(top.has_strays());
        assert_eq!(top.featured().len(), 1);
        let fetched = fake.fetched_uris();
        assert!(!fetched.iter().any(|uri| uri == T9 || uri == S9));
        assert!(fake.stray_fetches.lock().is_empty());
    }

    #[test]
    fn kind_tabs_hold_only_their_sections_uris_in_order() {
        let fake = top_fake();
        let (tabs, _) = first_top(&fake);
        let section = &read_all(&top_fixture()).unwrap()[&Kind::Track];
        let tracks = uris(&into_top_rows(tabs[&Kind::Track].rows.clone()));
        assert_eq!(tracks, section.uris);
        assert!(!tracks.iter().any(|uri| uri == T9));
    }

    #[test]
    fn strays_are_fetched_per_kind_and_land_in_the_answers_order() {
        let fake = top_fake();
        let (_, mut top) = first_top(&fake);
        let fetches = fake.fetches.lock().len();
        fetch_strays_with(&fake, &mut top);
        assert_eq!(uris(&top.rows()), [T1, T9, P1, S9]);
        let strays = fake.stray_fetches.lock().clone();
        assert_eq!(strays.len(), 2);
        assert!(strays.contains(&(Kind::Track, vec![T9.to_string()])));
        assert!(strays.contains(&(Kind::Show, vec![S9.to_string()])));
        assert_eq!(fake.fetches.lock().len(), fetches);
        assert!(!top.has_strays());
    }

    #[test]
    fn a_failed_stray_fetch_drops_only_strays_and_latches_nothing() {
        let fake = top_fake().with_stray(|kind, _| {
            Err(if kind == Kind::Track {
                InternalError::ProtocolChanged("x".into())
            } else {
                InternalError::RateLimited
            })
        });
        let (_, mut top) = first_top(&fake);
        let counts = fake.counts();
        fetch_strays_with(&fake, &mut top);
        assert_eq!(uris(&top.rows()), [T1, P1]);
        assert!(!top.has_strays());
        assert!(fake.latched_areas().is_empty());
        assert_eq!(fake.counts(), counts);
    }

    #[test]
    fn a_stray_fetch_that_returns_fewer_or_reordered_rows_is_matched_by_uri() {
        let draft = || {
            TopDraft::for_test(
                vec![
                    Err((Kind::Track, T8.to_string())),
                    Ok(TopRow::Track(full_track("t1"))),
                    Err((Kind::Track, T9.to_string())),
                ],
                Vec::new(),
            )
        };
        let unused = |_: Operation, _: &serde_json::Value| Err(InternalError::NoSession);
        let cases: [(&[&str], &[&str]); 2] = [(&["t9"], &[T1, T9]), (&["t9", "t8"], &[T8, T1, T9])];
        for (returned, expected) in cases {
            let ids: Vec<String> = returned.iter().map(|id| id.to_string()).collect();
            let fake = Fake::new(unused, rows_for).with_stray(move |_, _| {
                Ok(Rows::Tracks(ids.iter().map(|id| full_track(id)).collect()))
            });
            let mut top = draft();
            fetch_strays_with(&fake, &mut top);
            assert_eq!(uris(&top.rows()), expected);
            assert_eq!(
                *fake.stray_fetches.lock(),
                [(Kind::Track, vec![T8.to_string(), T9.to_string()])]
            );
            assert!(!top.has_strays());
        }
    }

    #[test]
    fn a_stray_row_under_another_id_is_dropped() {
        let unused = |_: Operation, _: &serde_json::Value| Err(InternalError::NoSession);
        let fake =
            Fake::new(unused, rows_for).with_stray(|_, _| Ok(Rows::Tracks(vec![full_track("tx")])));
        let mut top = TopDraft::for_test(vec![Err((Kind::Track, T9.to_string()))], Vec::new());
        fetch_strays_with(&fake, &mut top);
        assert!(top.rows().is_empty());
        assert!(!top.has_strays());
    }

    #[test]
    fn an_in_section_top_result_the_fetched_rows_leave_out_is_dropped() {
        // The tracks section holds t1 and t2; the fetch answers t2 only.
        let fake = Fake::new(
            |_, _| Ok(Outcome::Answer(top_fixture())),
            |kind, uris| {
                let kept: Vec<String> = uris.iter().filter(|uri| *uri != T1).cloned().collect();
                rows_for(kind, &kept)
            },
        );
        let (tabs, mut top) = first_top(&fake);
        assert_eq!(tabs[&Kind::Track].rows.len(), 1);
        assert_eq!(uris(&top.rows()), [P1]);
        fetch_strays_with(&fake, &mut top);
        assert_eq!(uris(&top.rows()), [T9, P1, S9]);
        let strays = fake.stray_fetches.lock().clone();
        assert!(
            !strays
                .iter()
                .any(|(_, uris)| uris.iter().any(|uri| uri == T1))
        );
    }

    #[test]
    fn a_failed_kind_batch_leaves_that_kinds_top_results_out_and_fetches_no_stray_of_it() {
        let fake = Fake::new(
            |_, _| Ok(Outcome::Answer(top_fixture())),
            |kind, uris| {
                if kind == Kind::Track {
                    Err(InternalError::RateLimited)
                } else {
                    rows_for(kind, uris)
                }
            },
        );
        let (_, mut top) = first_top(&fake);
        assert_eq!(uris(&top.rows()), [P1]);
        assert!(top.has_strays());
        fetch_strays_with(&fake, &mut top);
        assert_eq!(
            *fake.stray_fetches.lock(),
            [(Kind::Show, vec![S9.to_string()])]
        );
        assert_eq!(uris(&top.rows()), [P1, S9]);
    }

    #[test]
    fn a_missing_or_broken_top_section_changes_nothing_else() {
        let run = |answer: serde_json::Value| {
            let fake = Fake::new(move |_, _| Ok(Outcome::Answer(answer.clone())), rows_for);
            let s = store();
            let FirstPage::Internal { tabs, top } = first_page_with(&fake, &s, "x") else {
                panic!("not an internal first page")
            };
            let bad_hashes = (
                s.is_retired(Operation::Desktop),
                s.candidates(Operation::Desktop).len(),
            );
            (tabs, top, bad_hashes, fake.counts())
        };
        let (plain_tabs, plain_top, plain_hashes, _) = run(search::test_answer());
        assert!(plain_top.rows().is_empty() && !plain_top.has_strays());
        let mut broken = search::test_answer();
        broken["topResultsV2"] = json!("broken");
        let (tabs, top, hashes, counts) = run(broken);
        assert_eq!(tabs.len(), 6);
        for kind in Kind::ALL {
            let (a, b) = (&tabs[&kind], &plain_tabs[&kind]);
            assert_eq!(
                (a.rows.len(), a.next_offset, a.total),
                (b.rows.len(), b.next_offset, b.total),
                "{kind:?}"
            );
        }
        assert!(top.rows().is_empty() && !top.has_strays() && top.featured().is_empty());
        assert_eq!(hashes, plain_hashes);
        assert_eq!(counts, (1, 0));
    }

    #[test]
    fn no_draft_is_built_when_the_first_page_falls_back_or_fails() {
        let not_in_use = Fake::new(|_, _| Ok(Outcome::BadRequest), rows_for);
        assert!(matches!(
            first_page_with(&not_in_use, &store(), "x"),
            FirstPage::NotInUse
        ));
        let failed = Fake::new(|_, _| Err(InternalError::RateLimited), rows_for);
        assert!(matches!(
            first_page_with(&failed, &store(), "x"),
            FirstPage::Failed
        ));
        let latched = Fake::new(
            |_, _| Ok(Outcome::Answer(top_fixture())),
            |kind, uris| {
                if kind == Kind::Track {
                    Err(InternalError::ProtocolChanged("x".into()))
                } else {
                    rows_for(kind, uris)
                }
            },
        );
        assert!(matches!(
            first_page_with(&latched, &store(), "x"),
            FirstPage::NotInUse
        ));
        for fake in [not_in_use, failed, latched] {
            assert!(fake.stray_fetches.lock().is_empty());
        }
    }
}
