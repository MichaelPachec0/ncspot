//! One verified internal write on a playlist: bring the local copy up to date, POST the change
//! against the head revision, then confirm it with a second head read. Rootlist actions
//! (create, follow, unfollow) work the same way on the rootlist head.
//!
//! A write reply never proves the write applied (librespot may also re-send a POST by itself),
//! so the head length after the write is the check, and the local copy takes the snapshot of
//! that head, never the reply's revision.

use std::collections::HashMap;
use std::thread::sleep;
use std::time::Instant;

use librespot_protocol::playlist4_external::Op;
use log::{debug, warn};

use crate::internal_api::Area;
use crate::internal_api::error::InternalError;
use crate::internal_api::playlist::{Head, RootEntry, RootHead, Rootlist, base64_encode};
use crate::internal_api::playlist_write::{
    ADD_BATCH, Created, OVERWRITE_ATOMIC_MAX, add_op, list_changes, overwrite_ops, remove_at_op,
    rootlist_add_op, rootlist_remove_op,
};
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::playlist_layout::{PLAYLIST_FETCH_SPACING, Spacer};
use crate::spotify_api::WebApi;

/// Builds the track and episode metadata already known locally (`Library::known_playables`).
/// Called only when a stale local copy is reloaded, so a write on a current copy never
/// builds it.
pub type Known<'a> = &'a dyn Fn() -> HashMap<String, Playable>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    Current,
    Stale,
}

/// Current when the local copy has loaded tracks, its snapshot is the head's and its track
/// count (placeholders included) is the head length. A head without a length is never
/// current.
pub fn freshness(local: &Playlist, head: &Head) -> Freshness {
    match &local.tracks {
        Some(tracks)
            if local.snapshot_id == head.snapshot_id && head.length == Some(tracks.len()) =>
        {
            Freshness::Current
        }
        _ => Freshness::Stale,
    }
}

/// The selected item is still at `index` (same URI).
pub fn index_still(tracks: &[Playable], index: usize, uri: &str) -> bool {
    tracks
        .get(index)
        .is_some_and(|playable| playable.uri() == uri)
}

/// What a playlist write is based on: the revision the POST names, the snapshot the local
/// copy takes and the length the expected one is computed from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Base {
    revision: Vec<u8>,
    snapshot_id: String,
    length: usize,
}

/// A head read without a revision is a protocol change: a write never names an empty base.
fn require_revision(head: Head, what: &str) -> Result<Head, InternalError> {
    if head.revision.is_empty() {
        return Err(InternalError::ProtocolChanged(format!(
            "{what} without a revision"
        )));
    }
    Ok(head)
}

/// The base a full read gives: its revision, and the count of the items it read (never the
/// reply's `length` field, which may differ from them).
fn listing_base(head: Head, count: usize) -> Result<Base, InternalError> {
    let head = require_revision(head, "playlist read")?;
    Ok(Base {
        revision: head.revision,
        snapshot_id: head.snapshot_id,
        length: count,
    })
}

/// The length the reply of a full read reports when it is not the count of its items.
fn length_drift(head: &Head, count: usize) -> Option<usize> {
    head.length.filter(|&length| length != count)
}

/// The outcome of the head read after a write.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verify {
    /// The head length is the expected one; the head is the base of the next write.
    Applied(Base),
    /// The head length is another one.
    Differs(usize),
    /// The head lacks its revision or its length, so it proves nothing.
    Unreadable,
}

/// A write applied when the head length after it is the expected one. Length only: a
/// concurrent edit that leaves the length unchanged is not detected.
fn verify(expected_len: usize, after: Head) -> Verify {
    let Some(length) = after.length.filter(|_| !after.revision.is_empty()) else {
        return Verify::Unreadable;
    };
    if length != expected_len {
        return Verify::Differs(length);
    }
    Verify::Applied(Base {
        revision: after.revision,
        snapshot_id: after.snapshot_id,
        length,
    })
}

#[derive(Debug)]
pub enum EditError {
    /// Failed before anything was sent; the class decides the fallback as usual.
    Internal(InternalError),
    /// Failed after a POST may have reached the server (a transient or rate limited POST, the
    /// verify read or a later batch): the write may have applied, so it must never be repeated
    /// through the Web API.
    Unverified(InternalError),
    /// The POST was refused (not found, denied, a protocol change) or never sent (no
    /// session): nothing applied, but the action still fails without a Web fallback.
    Refused(InternalError),
    /// The selected item moved; nothing was sent.
    Moved,
    /// The head after the write does not match; the local copy was reloaded.
    Mismatch,
}

impl EditError {
    /// A fixed name of the failure class, for log lines (error texts may carry endpoint paths).
    pub fn class(&self) -> &'static str {
        match self {
            Self::Internal(err) => err.class(),
            Self::Unverified(_) => "unverified",
            Self::Refused(_) => "refused",
            Self::Moved => "moved",
            Self::Mismatch => "mismatch",
        }
    }

    /// The status note for a failure that is not a plain API error.
    pub fn note(&self) -> Option<&'static str> {
        match self {
            Self::Internal(_) => None,
            Self::Moved => Some("Playlist changed, try again"),
            Self::Mismatch => Some("Playlist changed elsewhere, reloaded"),
            Self::Unverified(_) => Some("Could not confirm the change; it may have applied"),
            Self::Refused(_) => Some("Spotify refused the change"),
        }
    }

    /// The edited local copy must be stored although the action failed: after an unconfirmed
    /// write it may hold earlier batches, and a failed verify read cleared its snapshot so the
    /// next open or write reloads it.
    pub fn stores_copy(&self) -> bool {
        matches!(self, Self::Unverified(_))
    }
}

impl From<InternalError> for EditError {
    fn from(err: InternalError) -> Self {
        Self::Internal(err)
    }
}

/// A moved index, a mismatch, a refused POST or an error after a POST fails the action:
/// `Transient` never falls back to the Web API (which could apply the write twice) and never
/// latches the area (a refused POST latches it itself on a protocol change).
impl From<EditError> for InternalError {
    fn from(err: EditError) -> Self {
        match err {
            EditError::Internal(err) => err,
            EditError::Unverified(err) => {
                Self::Transient(format!("the write may have applied: {err}"))
            }
            EditError::Refused(err) => Self::Transient(format!("the write was refused: {err}")),
            EditError::Moved => Self::Transient("the selected item moved".into()),
            EditError::Mismatch => Self::Transient("the playlist changed during the write".into()),
        }
    }
}

/// One change to a playlist, for its expected length afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// Append this many tracks.
    Add(usize),
    /// Remove one item.
    Remove,
    /// Replace the contents with this many tracks.
    Overwrite(usize),
}

/// The head length after `change` on a head of `head_len`; None when there is nothing to
/// remove.
fn expected_len(head_len: usize, change: Change) -> Option<usize> {
    match change {
        Change::Add(n) => Some(head_len + n),
        Change::Remove => head_len.checked_sub(1),
        Change::Overwrite(n) => Some(n),
    }
}

/// The part of an overwrite sent in its one atomic POST, and the tail sent as add batches.
fn overwrite_split<T>(tracks: &[T]) -> (&[T], &[T]) {
    tracks.split_at(tracks.len().min(OVERWRITE_ATOMIC_MAX))
}

/// The add batches, one POST each.
fn add_chunks<T>(new: &[T]) -> std::slice::Chunks<'_, T> {
    new.chunks(ADD_BATCH)
}

/// Bring `playlist` up to date with the server when stale (a head without a length counts as
/// stale); returns the base of the write. A head or full read without a revision fails with
/// a protocol change before anything is sent.
fn prepare_head(
    api: &WebApi,
    playlist: &mut Playlist,
    known: Known<'_>,
) -> Result<Base, EditError> {
    let head = require_revision(api.playlist_head(&playlist.id)?, "playlist head")?;
    match freshness(playlist, &head) {
        Freshness::Current => Ok(current_base(head, playlist)),
        Freshness::Stale => {
            let (base, drift) = reload(api, playlist, known)?;
            if let Some(reported) = drift {
                warn!(
                    "Playlist read lists {} items but reports {reported}; using the items",
                    base.length
                );
            }
            Ok(base)
        }
    }
}

/// The base of a write on a current local copy: the head read (its length is the copy's).
fn current_base(head: Head, playlist: &Playlist) -> Base {
    Base {
        revision: head.revision,
        snapshot_id: head.snapshot_id,
        length: playlist.tracks.as_ref().map_or(0, Vec::len),
    }
}

/// Replace the local tracks with a full read and take that read's snapshot; returns the base
/// it gives (see [listing_base]: the change is checked against these items, and the playlist
/// may have changed since the head read, so the POST must name the revision of this read) and
/// the reply's length when it differs from the item count. A read without a revision leaves
/// the local copy unchanged.
fn reload(
    api: &WebApi,
    playlist: &mut Playlist,
    known: Known<'_>,
) -> Result<(Base, Option<usize>), InternalError> {
    let (head, tracks) = api.playlist_playables_at(&playlist.id, &known())?;
    let drift = length_drift(&head, tracks.len());
    let base = listing_base(head, tracks.len())?;
    set_tracks(playlist, tracks, &base.snapshot_id);
    Ok((base, drift))
}

/// Store `tracks` (renumbered by position) with `snapshot_id`.
fn set_tracks(playlist: &mut Playlist, mut tracks: Vec<Playable>, snapshot_id: &str) {
    for (index, playable) in tracks.iter_mut().enumerate() {
        playable.set_list_index(index);
    }
    playlist.num_tracks = tracks.len();
    playlist.tracks = Some(tracks);
    playlist.snapshot_id = snapshot_id.to_string();
}

/// The class of a failed write POST: only a transient error or a rate limit may have reached
/// the server ([EditError::Unverified]); a refusal, a protocol change or a missing session
/// means nothing applied ([EditError::Refused]). Neither falls back to the Web API.
fn post_error(err: InternalError) -> EditError {
    match err {
        InternalError::Transient(_) | InternalError::RateLimited => EditError::Unverified(err),
        InternalError::NotFound
        | InternalError::Denied
        | InternalError::ProtocolChanged(_)
        | InternalError::NoSession => EditError::Refused(err),
    }
}

/// [post_error], latching the playlists area on a protocol change.
fn failed_post(api: &WebApi, err: InternalError) -> EditError {
    if let InternalError::ProtocolChanged(_) = err {
        api.latch_area(Area::Playlists, &err);
    }
    post_error(err)
}

/// The verify read after a POST failed or proved nothing: clear the local snapshot so the
/// next open or write reloads the copy (the caller stores it, see [EditError::stores_copy]).
fn unconfirmed(playlist: &mut Playlist, err: InternalError) -> EditError {
    playlist.snapshot_id.clear();
    EditError::Unverified(err)
}

/// POST `ops` against `base`, then read the head: its base when its length is `expected_len`,
/// else reload the local copy and fail with [EditError::Mismatch]. A failed POST is classed by
/// [post_error]; a failed or unreadable verify read is [unconfirmed].
fn send_verified(
    api: &WebApi,
    playlist: &mut Playlist,
    known: Known<'_>,
    base: Vec<u8>,
    ops: Vec<Op>,
    expected_len: usize,
) -> Result<Base, EditError> {
    api.playlist_changes(&playlist.id, list_changes(base, ops))
        .map_err(|err| failed_post(api, err))?;
    let after = match api.playlist_head(&playlist.id) {
        Ok(after) => after,
        Err(err) => return Err(unconfirmed(playlist, err)),
    };
    let length = match verify(expected_len, after) {
        Verify::Applied(base) => return Ok(base),
        Verify::Unreadable => {
            return Err(unconfirmed(
                playlist,
                InternalError::ProtocolChanged("playlist head without a revision or length".into()),
            ));
        }
        Verify::Differs(length) => length,
    };
    warn!("Playlist write did not verify: length {length} after it, {expected_len} expected");
    if let Err(err) = reload(api, playlist, known) {
        warn!(
            "Could not reload the playlist after a write that did not verify ({})",
            err.class()
        );
    }
    Err(EditError::Mismatch)
}

fn uris(playables: &[Playable]) -> Vec<String> {
    playables.iter().map(Playable::uri).collect()
}

fn spaced(spacer: &mut Spacer) {
    let wait = spacer.next_wait(Instant::now());
    if !wait.is_zero() {
        sleep(wait);
    }
}

/// Append `new` at the end of the playlist, one POST per [ADD_BATCH] tracks.
pub fn add_tracks(
    api: &WebApi,
    playlist: &mut Playlist,
    new: &[Playable],
    known: Known<'_>,
) -> Result<(), EditError> {
    if new.is_empty() {
        return Ok(());
    }
    let base = prepare_head(api, playlist, known)?;
    add_batches(
        api,
        playlist,
        known,
        base,
        new,
        &mut Spacer::new(PLAYLIST_FETCH_SPACING),
    )
}

/// One verified ADD per [ADD_BATCH] tracks, [PLAYLIST_FETCH_SPACING] between POSTs, each based
/// on the head verified after the previous one. Batches before a failure stay applied.
fn add_batches(
    api: &WebApi,
    playlist: &mut Playlist,
    known: Known<'_>,
    mut base: Base,
    new: &[Playable],
    spacer: &mut Spacer,
) -> Result<(), EditError> {
    for batch in add_chunks(new) {
        spaced(spacer);
        let expected = expected_len(base.length, Change::Add(batch.len())).unwrap_or_default();
        let ops = vec![add_op(&uris(batch), None)];
        let after = send_verified(api, playlist, known, base.revision, ops, expected)?;
        let mut tracks = playlist.tracks.take().unwrap_or_default();
        tracks.extend_from_slice(batch);
        set_tracks(playlist, tracks, &after.snapshot_id);
        base = after;
    }
    Ok(())
}

/// Remove the item at `index`, which must still be `uri` once the local copy is current.
pub fn remove_track(
    api: &WebApi,
    playlist: &mut Playlist,
    index: usize,
    uri: &str,
    known: Known<'_>,
) -> Result<(), EditError> {
    let base = prepare_head(api, playlist, known)?;
    let tracks = playlist.tracks.as_deref().unwrap_or_default();
    if !index_still(tracks, index, uri) {
        return Err(EditError::Moved);
    }
    let expected = expected_len(base.length, Change::Remove).ok_or(EditError::Moved)?;
    let ops = vec![remove_at_op(index)];
    let after = send_verified(api, playlist, known, base.revision, ops, expected)?;
    let mut tracks = playlist.tracks.take().unwrap_or_default();
    tracks.remove(index);
    set_tracks(playlist, tracks, &after.snapshot_id);
    Ok(())
}

/// Replace the contents with `tracks`: one atomic POST up to [OVERWRITE_ATOMIC_MAX] tracks,
/// above that the first POST carries the removal plus the first [OVERWRITE_ATOMIC_MAX] tracks
/// and the rest go as add batches.
pub fn overwrite(
    api: &WebApi,
    playlist: &mut Playlist,
    tracks: &[Playable],
    known: Known<'_>,
) -> Result<(), EditError> {
    let base = prepare_head(api, playlist, known)?;
    let (first, rest) = overwrite_split(tracks);
    let ops = overwrite_ops(base.length, &uris(first));
    let mut spacer = Spacer::new(PLAYLIST_FETCH_SPACING);
    let after = if ops.is_empty() {
        base
    } else {
        spaced(&mut spacer);
        let expected =
            expected_len(base.length, Change::Overwrite(first.len())).unwrap_or_default();
        send_verified(api, playlist, known, base.revision, ops, expected)?
    };
    set_tracks(playlist, first.to_vec(), &after.snapshot_id);
    add_batches(api, playlist, known, after, rest, &mut spacer)
}

const PLAYLIST_URI_PREFIX: &str = "spotify:playlist:";

fn playlist_uri(id: &str) -> String {
    format!("{PLAYLIST_URI_PREFIX}{id}")
}

/// The rootlist index of `spotify:playlist:<id>`, counting folder markers as entries.
/// Items the parse left out of `entries` count too (through `Rootlist::skipped`).
pub fn rootlist_index(rootlist: &Rootlist, id: &str) -> Option<usize> {
    let entry = rootlist
        .entries
        .iter()
        .position(|entry| matches!(entry, RootEntry::Playlist(p) if p.id == id))?;
    // The entry is the `entry`-th raw item that was not skipped.
    let mut index = entry;
    for &skipped in &rootlist.skipped {
        if skipped > index {
            break;
        }
        index += 1;
    }
    Some(index)
}

/// What a rootlist write is based on.
struct RootBase {
    revision: Vec<u8>,
    length: usize,
}

/// The base of a full rootlist read; a read without a revision is a protocol change.
fn root_base(rootlist: &Rootlist) -> Result<RootBase, InternalError> {
    if rootlist.revision.is_empty() {
        return Err(InternalError::ProtocolChanged(
            "rootlist read without a revision".into(),
        ));
    }
    Ok(RootBase {
        revision: rootlist.revision.clone(),
        length: rootlist.length,
    })
}

/// The base a rootlist head gives: only when it carries both a revision and a length.
fn head_base(head: RootHead) -> Option<RootBase> {
    Some(RootBase {
        revision: head.revision?,
        length: head.length?,
    })
}

/// The base from the rootlist head; when the head lacks its revision or its length, from a
/// full rootlist read instead.
fn rootlist_base(api: &WebApi) -> Result<RootBase, InternalError> {
    if let Some(base) = head_base(api.rootlist_head()?) {
        return Ok(base);
    }
    debug!("rootlist head without a revision or length, reading the whole rootlist for the base");
    root_base(&api.playlist_rootlist()?)
}

/// The verify rule for a rootlist write: the head after it carries a length and it is
/// `expected_len`. A head without a length proves nothing ([EditError::Unverified]).
fn rootlist_verify(after: &RootHead, expected_len: usize) -> Result<(), EditError> {
    match after.length {
        Some(length) if length == expected_len => Ok(()),
        Some(length) => {
            warn!(
                "Rootlist write did not verify: length {length} after it, {expected_len} expected"
            );
            Err(EditError::Mismatch)
        }
        None => Err(EditError::Unverified(InternalError::Transient(
            "rootlist head without a length".into(),
        ))),
    }
}

/// POST `op` against the rootlist `base`, then confirm it with [rootlist_verify]. A failed
/// POST is classed by [post_error]; a failed verify read is [EditError::Unverified].
fn send_rootlist_verified(
    api: &WebApi,
    base: RootBase,
    op: Op,
    expected_len: usize,
) -> Result<(), EditError> {
    api.rootlist_changes(list_changes(base.revision, vec![op]))
        .map_err(|err| failed_post(api, err))?;
    let after = api.rootlist_head().map_err(EditError::Unverified)?;
    rootlist_verify(&after, expected_len)
}

/// Add `spotify:playlist:<id>` on top of `base`, verified.
fn rootlist_add_on(api: &WebApi, base: RootBase, id: &str) -> Result<(), EditError> {
    let expected = expected_len(base.length, Change::Add(1)).unwrap_or_default();
    send_rootlist_verified(api, base, rootlist_add_op(&playlist_uri(id)), expected)
}

/// Add `spotify:playlist:<id>` at the top of the rootlist, verified.
fn rootlist_add(api: &WebApi, id: &str) -> Result<(), EditError> {
    rootlist_add_on(api, rootlist_base(api)?, id)
}

/// The second rootlist add of a create: a full rootlist read first, so an earlier add that
/// applied without being confirmed is not repeated; the read is also the base.
fn rootlist_add_again(api: &WebApi, id: &str) -> Result<(), EditError> {
    let rootlist = api.playlist_rootlist()?;
    if rootlist_index(&rootlist, id).is_some() {
        debug!("the new playlist is in the rootlist already");
        return Ok(());
    }
    rootlist_add_on(api, root_base(&rootlist)?, id)
}

/// The class of a failed create POST. A protocol change there may be a 2xx reply that does not
/// parse, so the playlist may exist: [EditError::Unverified], never a Web create (the caller
/// latches the area). Other classes keep their meaning: a refusal (not found, denied) may use
/// the Web API once, a transient error or rate limit fails.
fn create_error(err: InternalError) -> EditError {
    match err {
        InternalError::ProtocolChanged(_) => EditError::Unverified(err),
        err => EditError::Internal(err),
    }
}

/// Latch the playlists area on a protocol change found before a rootlist POST.
fn latch_on_protocol_change(api: &WebApi, err: &EditError) {
    if let EditError::Internal(err @ InternalError::ProtocolChanged(_)) = err {
        api.latch_area(Area::Playlists, err);
    }
}

/// The stored copy of a playlist just created: empty, owned by `owner`, at the create
/// reply's revision.
pub fn created_playlist(created: &Created, name: &str, owner: Option<&str>) -> Playlist {
    Playlist {
        id: created.id.clone(),
        name: name.to_string(),
        owner_id: owner.unwrap_or_default().to_string(),
        owner_name: None,
        snapshot_id: base64_encode(&created.revision),
        num_tracks: 0,
        tracks: Some(vec![]),
        collaborative: false,
        cover_url: None,
    }
}

/// A failure once the playlist exists: it must never lead to a second create through the Web
/// API, so even an error from before the rootlist POST counts as [EditError::Unverified].
fn after_create(err: EditError) -> EditError {
    match err {
        EditError::Internal(err) => EditError::Unverified(err),
        other => other,
    }
}

/// Create a playlist and add it to the top of the rootlist; returns the stored copy.
///
/// A failed rootlist add is retried once, then the Web API follow (a second add of the same
/// new playlist is harmless). If all fail the playlist exists but is not in the library.
pub fn create(api: &WebApi, name: &str) -> Result<Playlist, EditError> {
    let mut spacer = Spacer::new(PLAYLIST_FETCH_SPACING);
    spaced(&mut spacer);
    let created = api.playlist_create(name).map_err(|err| {
        if let InternalError::ProtocolChanged(_) = err {
            api.latch_area(Area::Playlists, &err);
        }
        create_error(err)
    })?;
    let playlist = created_playlist(&created, name, api.user_id());
    spaced(&mut spacer);
    let added = rootlist_add(api, &created.id)
        .or_else(|err| {
            latch_on_protocol_change(api, &err);
            warn!(
                "Could not add the new playlist to the rootlist ({}), trying again",
                err.class()
            );
            spaced(&mut spacer);
            rootlist_add_again(api, &created.id)
        })
        .or_else(|err| {
            latch_on_protocol_change(api, &err);
            warn!(
                "Could not add the new playlist to the rootlist ({}), following it over the Web API",
                err.class()
            );
            api.count_fallback(Area::Playlists);
            api.user_playlist_follow_playlist(&created.id)
                .map_err(|()| err)
        });
    match added {
        Ok(()) => Ok(playlist),
        Err(err) => {
            warn!("Playlist created but not added to your library");
            Err(after_create(err))
        }
    }
}

/// Add playlist `id` to the top of the rootlist.
pub fn follow(api: &WebApi, id: &str) -> Result<(), EditError> {
    rootlist_add(api, id)
}

/// Remove `id` from the rootlist, at the index found in a fresh full rootlist read.
pub fn unfollow(api: &WebApi, id: &str) -> Result<(), EditError> {
    let rootlist = api.playlist_rootlist()?;
    let index = rootlist_index(&rootlist, id).ok_or(EditError::Moved)?;
    // The index walks the raw items, so it is the playlist's own; it must also lie inside the
    // listing the service reported.
    if index >= rootlist.length {
        return Err(EditError::Moved);
    }
    let base = root_base(&rootlist)?;
    let expected = expected_len(base.length, Change::Remove).ok_or(EditError::Moved)?;
    send_rootlist_verified(api, base, rootlist_remove_op(index), expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal_api::playlist::{RootEntry, RootPlaylist};
    use crate::playlist_items::placeholder;

    fn track_playable(uri: &str) -> Playable {
        placeholder(uri)
    }

    fn test_playlist(id: &str) -> Playlist {
        Playlist {
            id: id.into(),
            name: String::new(),
            owner_id: String::new(),
            owner_name: None,
            snapshot_id: String::new(),
            num_tracks: 0,
            tracks: None,
            collaborative: false,
            cover_url: None,
        }
    }

    fn local(snapshot: &str, n: usize) -> Playlist {
        Playlist {
            snapshot_id: snapshot.into(),
            tracks: Some(
                (0..n)
                    .map(|i| track_playable(&format!("spotify:track:{i}")))
                    .collect(),
            ),
            num_tracks: n,
            ..test_playlist("p")
        }
    }

    fn head(snapshot: &str, length: usize) -> Head {
        Head {
            revision: vec![1],
            snapshot_id: snapshot.into(),
            length: Some(length),
            name: None,
        }
    }

    #[test]
    fn freshness_needs_loaded_tracks_same_snapshot_and_count() {
        assert_eq!(freshness(&local("s", 2), &head("s", 2)), Freshness::Current);
        assert_eq!(freshness(&local("s", 2), &head("t", 2)), Freshness::Stale);
        assert_eq!(freshness(&local("s", 2), &head("s", 3)), Freshness::Stale);
        let unloaded = Playlist {
            tracks: None,
            ..local("s", 0)
        };
        assert_eq!(freshness(&unloaded, &head("s", 0)), Freshness::Stale);
    }

    #[test]
    fn a_head_without_a_length_is_stale() {
        let no_length = Head {
            length: None,
            ..head("s", 0)
        };
        assert_eq!(freshness(&local("s", 0), &no_length), Freshness::Stale);
    }

    #[test]
    fn a_read_without_a_revision_is_a_protocol_change() {
        let no_revision = Head {
            revision: Vec::new(),
            ..head("", 2)
        };
        assert!(matches!(
            listing_base(no_revision.clone(), 2),
            Err(InternalError::ProtocolChanged(_))
        ));
        assert_eq!(
            require_revision(head("s", 2), "playlist head"),
            Ok(head("s", 2))
        );
        let err = require_revision(no_revision, "playlist head").unwrap_err();
        assert!(matches!(err, InternalError::ProtocolChanged(_)));
        // Before any POST, so it keeps its class: the area latches, then the Web API.
        use crate::internal_api::fallback::{Action, decide_after};
        let err = InternalError::from(EditError::from(err));
        assert_eq!(decide_after(&err), Action::LatchThenWeb);
    }

    #[test]
    fn a_reload_is_based_on_its_item_count_and_revision() {
        let read = Head {
            revision: b"reload".to_vec(),
            ..head("r", 5)
        };
        let base = listing_base(read, 3).expect("has a revision");
        assert_eq!(
            base,
            Base {
                revision: b"reload".to_vec(),
                snapshot_id: "r".into(),
                length: 3,
            }
        );
    }

    #[test]
    fn a_write_on_a_current_copy_is_based_on_the_head() {
        let read = Head {
            revision: b"head".to_vec(),
            ..head("h", 2)
        };
        assert_eq!(
            current_base(read, &local("h", 2)),
            Base {
                revision: b"head".to_vec(),
                snapshot_id: "h".into(),
                length: 2,
            }
        );
    }

    #[test]
    fn length_drift_compares_a_reported_length_with_the_items() {
        assert_eq!(length_drift(&head("s", 3), 3), None);
        assert_eq!(length_drift(&head("s", 4), 3), Some(4));
        let no_length = Head {
            length: None,
            ..head("s", 0)
        };
        assert_eq!(length_drift(&no_length, 3), None);
    }

    #[test]
    fn index_must_still_point_at_the_selected_item() {
        let tracks = local("s", 3).tracks.unwrap();
        assert!(index_still(&tracks, 1, "spotify:track:1"));
        assert!(!index_still(&tracks, 1, "spotify:track:2"));
        assert!(!index_still(&tracks, 9, "spotify:track:1"));
    }

    #[test]
    fn verify_compares_the_head_length() {
        assert_eq!(
            verify(3, head("x", 3)),
            Verify::Applied(Base {
                revision: vec![1],
                snapshot_id: "x".into(),
                length: 3,
            })
        );
        assert_eq!(verify(3, head("x", 2)), Verify::Differs(2));
    }

    #[test]
    fn a_verify_head_without_revision_or_length_proves_nothing() {
        let no_length = Head {
            length: None,
            ..head("x", 0)
        };
        let no_revision = Head {
            revision: Vec::new(),
            ..head("", 3)
        };
        assert_eq!(verify(0, no_length), Verify::Unreadable);
        assert_eq!(verify(3, no_revision), Verify::Unreadable);
    }

    #[test]
    fn an_unconfirmed_write_clears_the_snapshot_and_stores_the_copy() {
        let mut playlist = local("s", 2);
        let err = unconfirmed(&mut playlist, InternalError::Transient("x".into()));
        assert!(matches!(err, EditError::Unverified(_)));
        assert!(err.stores_copy());
        assert_eq!(
            err.note(),
            Some("Could not confirm the change; it may have applied")
        );
        assert_eq!(playlist.snapshot_id, "");
        assert_eq!(playlist.num_tracks, 2, "the tracks stay");
        // The next write sees the copy as stale and reloads.
        assert_eq!(freshness(&playlist, &head("s", 2)), Freshness::Stale);
    }

    #[test]
    fn post_errors_split_into_unverified_and_refused() {
        use crate::internal_api::fallback::{Action, decide_after};
        let cases = [
            (InternalError::Transient("timeout".into()), "unverified"),
            (InternalError::RateLimited, "unverified"),
            (InternalError::NotFound, "refused"),
            (InternalError::Denied, "refused"),
            (InternalError::ProtocolChanged("412".into()), "refused"),
            (InternalError::NoSession, "refused"),
        ];
        for (inner, class) in cases {
            let err = post_error(inner.clone());
            assert_eq!(err.class(), class, "{inner}");
            assert_eq!(err.stores_copy(), class == "unverified", "{inner}");
            let note = if class == "refused" {
                "Spotify refused the change"
            } else {
                "Could not confirm the change; it may have applied"
            };
            assert_eq!(err.note(), Some(note), "{inner}");
            assert_eq!(
                decide_after(&InternalError::from(err)),
                Action::Fail,
                "{inner}"
            );
        }
    }

    #[test]
    fn edit_errors_fail_without_fallback() {
        use crate::internal_api::fallback::{Action, decide_after};
        for err in [
            EditError::Moved,
            EditError::Mismatch,
            EditError::Refused(InternalError::Denied),
        ] {
            assert!(!err.stores_copy());
            assert_eq!(decide_after(&InternalError::from(err)), Action::Fail);
        }
    }

    #[test]
    fn unverified_errors_fail_for_every_class() {
        use crate::internal_api::fallback::{Action, decide_after};
        for inner in [
            InternalError::NoSession,
            InternalError::NotFound,
            InternalError::Denied,
            InternalError::ProtocolChanged("412".into()),
            InternalError::RateLimited,
            InternalError::Transient("x".into()),
        ] {
            let err = InternalError::from(EditError::Unverified(inner.clone()));
            assert!(matches!(err, InternalError::Transient(_)), "{inner}");
            assert_eq!(decide_after(&err), Action::Fail, "{inner}");
        }
    }

    #[test]
    fn errors_before_the_post_keep_their_class() {
        for inner in [
            InternalError::NoSession,
            InternalError::NotFound,
            InternalError::Denied,
            InternalError::ProtocolChanged("412".into()),
        ] {
            assert_eq!(
                InternalError::from(EditError::Internal(inner.clone())),
                inner
            );
        }
    }

    #[test]
    fn expected_length_per_change() {
        assert_eq!(expected_len(5, Change::Add(3)), Some(8));
        assert_eq!(expected_len(5, Change::Remove), Some(4));
        assert_eq!(expected_len(0, Change::Remove), None);
        assert_eq!(expected_len(5, Change::Overwrite(2)), Some(2));
        assert_eq!(expected_len(5, Change::Overwrite(0)), Some(0));
    }

    #[test]
    fn overwrite_sends_up_to_the_atomic_max_then_add_batches() {
        let small: Vec<usize> = (0..250).collect();
        let (first, rest) = overwrite_split(&small);
        assert_eq!((first.len(), rest.len()), (250, 0));

        let big: Vec<usize> = (0..OVERWRITE_ATOMIC_MAX + 250).collect();
        let (first, rest) = overwrite_split(&big);
        assert_eq!(first.len(), OVERWRITE_ATOMIC_MAX);
        assert_eq!(rest[0], OVERWRITE_ATOMIC_MAX, "the tail continues in order");
        let sizes: Vec<usize> = add_chunks(rest).map(<[usize]>::len).collect();
        assert_eq!(sizes, [ADD_BATCH, ADD_BATCH, 50]);
    }

    #[test]
    fn add_batches_are_add_batch_sized() {
        let new: Vec<usize> = (0..ADD_BATCH * 2 + 1).collect();
        let sizes: Vec<usize> = add_chunks(&new).map(<[usize]>::len).collect();
        assert_eq!(sizes, [ADD_BATCH, ADD_BATCH, 1]);
        assert_eq!(add_chunks::<usize>(&[]).count(), 0);
    }

    #[test]
    fn set_tracks_renumbers_and_takes_the_snapshot() {
        let mut playlist = local("s", 0);
        let tracks = vec![
            track_playable("spotify:track:a"),
            track_playable("spotify:track:b"),
        ];
        set_tracks(&mut playlist, tracks.into_iter().rev().collect(), "t");
        assert_eq!(playlist.snapshot_id, "t");
        assert_eq!(playlist.num_tracks, 2);
        let tracks = playlist.tracks.unwrap();
        assert_eq!(tracks[0].uri(), "spotify:track:b");
        for (i, p) in tracks.iter().enumerate() {
            assert_eq!(p.list_index(), i);
        }
    }

    fn root_pl(id: &str) -> RootEntry {
        RootEntry::Playlist(RootPlaylist {
            id: id.into(),
            name: String::new(),
            snapshot_id: String::new(),
            length: 0,
            owner_id: String::new(),
            collaborative: false,
            cover_url: None,
            status: 200,
        })
    }

    #[test]
    fn rootlist_index_counts_folder_markers() {
        let rootlist = Rootlist {
            entries: vec![
                RootEntry::FolderStart { name: "F".into() },
                root_pl("a"),
                RootEntry::FolderEnd,
                root_pl("b"),
            ],
            length: 4,
            read: 4,
            pages: 1,
            ..Default::default()
        };
        assert_eq!(rootlist_index(&rootlist, "b"), Some(3));
        assert_eq!(rootlist_index(&rootlist, "a"), Some(1));
        assert_eq!(rootlist_index(&rootlist, "missing"), None);
    }

    #[test]
    fn rootlist_index_counts_skipped_items() {
        // Raw items: a, <track>, <track>, b, <track>, c
        let rootlist = Rootlist {
            entries: vec![root_pl("a"), root_pl("b"), root_pl("c")],
            skipped: vec![1, 2, 4],
            length: 6,
            read: 6,
            pages: 1,
            ..Default::default()
        };
        assert_eq!(rootlist_index(&rootlist, "a"), Some(0));
        assert_eq!(rootlist_index(&rootlist, "b"), Some(3));
        assert_eq!(rootlist_index(&rootlist, "c"), Some(5));
    }

    #[test]
    fn create_without_a_session_sends_nothing() {
        let api = WebApi::new(&crate::config::Config::new_for_test());
        assert!(matches!(
            create(&api, "ncspot-test-1"),
            Err(EditError::Internal(InternalError::NoSession))
        ));
    }

    #[test]
    fn follow_and_unfollow_without_a_session_send_nothing() {
        let api = WebApi::new(&crate::config::Config::new_for_test());
        assert!(matches!(
            follow(&api, "p"),
            Err(EditError::Internal(InternalError::NoSession))
        ));
        assert!(matches!(
            unfollow(&api, "p"),
            Err(EditError::Internal(InternalError::NoSession))
        ));
    }

    #[test]
    fn errors_after_the_create_never_allow_a_second_create() {
        use crate::internal_api::fallback::{Action, decide_after};
        for err in [
            EditError::Internal(InternalError::NoSession),
            EditError::Internal(InternalError::ProtocolChanged("x".into())),
            EditError::Unverified(InternalError::Denied),
            EditError::Refused(InternalError::NotFound),
            EditError::Mismatch,
        ] {
            let after = after_create(err);
            assert!(!matches!(after, EditError::Internal(_)));
            assert_eq!(decide_after(&InternalError::from(after)), Action::Fail);
        }
    }

    #[test]
    fn created_playlist_is_empty_and_owned() {
        let created = Created {
            id: "abc".into(),
            revision: b"rev1".to_vec(),
        };
        let playlist = created_playlist(&created, "ncspot-test-1", Some("me"));
        assert_eq!(playlist.id, "abc");
        assert_eq!(playlist.snapshot_id, "cmV2MQ==");
        assert_eq!(playlist.owner_id, "me");
        assert_eq!(playlist.name, "ncspot-test-1");
        assert_eq!(playlist.num_tracks, 0);
        assert_eq!(playlist.tracks.as_deref().map(<[Playable]>::len), Some(0));
    }

    #[test]
    fn a_protocol_change_on_create_never_creates_again() {
        use crate::internal_api::fallback::{Action, decide_after};
        let class = |err| decide_after(&InternalError::from(create_error(err)));
        assert_eq!(
            class(InternalError::ProtocolChanged("reply".into())),
            Action::Fail
        );
        assert_eq!(class(InternalError::NotFound), Action::WebOnce);
        assert_eq!(class(InternalError::Denied), Action::WebOnce);
        assert_eq!(class(InternalError::NoSession), Action::WebOnce);
        assert_eq!(
            class(InternalError::Transient("timeout".into())),
            Action::Fail
        );
        assert_eq!(class(InternalError::RateLimited), Action::Fail);
    }

    #[test]
    fn a_rootlist_head_is_a_base_only_with_revision_and_length() {
        let head = |revision: Option<&[u8]>, length| RootHead {
            revision: revision.map(<[u8]>::to_vec),
            length,
        };
        let base = head_base(head(Some(b"r"), Some(4))).expect("both fields");
        assert_eq!((base.revision, base.length), (b"r".to_vec(), 4));
        assert!(head_base(head(None, Some(4))).is_none());
        assert!(head_base(head(Some(b"r"), None)).is_none());
    }

    #[test]
    fn a_rootlist_head_without_length_does_not_verify_or_mismatch() {
        let after = |length| RootHead {
            revision: Some(b"r".to_vec()),
            length,
        };
        assert!(rootlist_verify(&after(Some(5)), 5).is_ok());
        assert!(matches!(
            rootlist_verify(&after(Some(4)), 5),
            Err(EditError::Mismatch)
        ));
        assert!(matches!(
            rootlist_verify(&after(None), 0),
            Err(EditError::Unverified(_))
        ));
    }

    #[test]
    fn error_classes_log_fixed_names() {
        assert_eq!(
            EditError::Internal(InternalError::Transient("/user/x".into())).class(),
            "transient"
        );
        assert_eq!(
            EditError::Unverified(InternalError::Transient("/user/x".into())).class(),
            "unverified"
        );
        assert_eq!(
            EditError::Refused(InternalError::ProtocolChanged("/user/x".into())).class(),
            "refused"
        );
    }

    #[test]
    fn add_tracks_without_a_session_leaves_the_playlist_unchanged() {
        let api = WebApi::new(&crate::config::Config::new_for_test());
        let mut playlist = local("s", 2);
        let new = [track_playable("spotify:track:new")];
        let built = std::cell::Cell::new(0);
        let known = || {
            built.set(built.get() + 1);
            HashMap::new()
        };
        let result = add_tracks(&api, &mut playlist, &new, &known);
        assert!(matches!(
            result,
            Err(EditError::Internal(InternalError::NoSession))
        ));
        assert_eq!(
            built.get(),
            0,
            "no reload, so the known metadata is never built"
        );
        assert_eq!(playlist.snapshot_id, "s");
        assert_eq!(playlist.num_tracks, 2);
        let uris: Vec<String> = playlist.tracks.unwrap().iter().map(Playable::uri).collect();
        assert_eq!(uris, ["spotify:track:0", "spotify:track:1"]);
    }
}
