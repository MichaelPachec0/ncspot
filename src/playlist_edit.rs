//! One verified internal write on a playlist: bring the local copy up to date, POST the change
//! against the head revision, then confirm it with a second head read.
//!
//! A write reply never proves the write applied (librespot may also re-send a POST by itself),
//! so the head length after the write is the check, and the local copy takes the snapshot of
//! that head, never the reply's revision.

#![expect(
    dead_code,
    reason = "the playlist commands that call these arrive in a later change"
)]

use std::collections::HashMap;
use std::thread::sleep;
use std::time::Instant;

use librespot_protocol::playlist4_external::Op;
use log::warn;

use crate::internal_api::error::InternalError;
use crate::internal_api::playlist::Head;
use crate::internal_api::playlist_write::{
    ADD_BATCH, OVERWRITE_ATOMIC_MAX, add_op, list_changes, overwrite_ops, remove_at_op,
};
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::playlist_layout::{PLAYLIST_FETCH_SPACING, Spacer};
use crate::spotify_api::WebApi;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    Current,
    Stale,
}

/// Current when the local copy has loaded tracks, its snapshot is the head's and its track
/// count (placeholders included) is the head length.
pub fn freshness(local: &Playlist, head: &Head) -> Freshness {
    match &local.tracks {
        Some(tracks) if local.snapshot_id == head.snapshot_id && tracks.len() == head.length => {
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

/// A write applied when the head length after it is the expected one. Length only: a
/// concurrent edit that leaves the length unchanged is not detected.
pub fn verified(expected_len: usize, after: &Head) -> bool {
    after.length == expected_len
}

#[derive(Debug)]
pub enum EditError {
    /// Failed before anything was sent; the class decides the fallback as usual.
    Internal(InternalError),
    /// Failed after a POST was sent (the POST itself, the verify read or a later batch): the
    /// write may have applied, so it must never be repeated through the Web API.
    Unverified(InternalError),
    /// The selected item moved; nothing was sent.
    Moved,
    /// The head after the write does not match; the local copy was reloaded.
    Mismatch,
}

impl From<InternalError> for EditError {
    fn from(err: InternalError) -> Self {
        Self::Internal(err)
    }
}

/// A moved index, a mismatch or an error after a POST fails the action: `Transient` never
/// falls back to the Web API (which could apply the write twice) and never latches the area.
impl From<EditError> for InternalError {
    fn from(err: EditError) -> Self {
        match err {
            EditError::Internal(err) => err,
            EditError::Unverified(err) => {
                Self::Transient(format!("the write may have applied: {err}"))
            }
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

/// Bring `playlist` up to date with the server when stale; returns the base revision.
pub fn prepare(
    api: &WebApi,
    playlist: &mut Playlist,
    known: &HashMap<String, Playable>,
) -> Result<Vec<u8>, EditError> {
    Ok(prepare_head(api, playlist, known)?.revision)
}

/// Like [prepare], returning the whole head (its length is the base for the expected one).
fn prepare_head(
    api: &WebApi,
    playlist: &mut Playlist,
    known: &HashMap<String, Playable>,
) -> Result<Head, EditError> {
    let head = api.playlist_head(&playlist.id)?;
    if freshness(playlist, &head) == Freshness::Stale {
        reload(api, playlist, known, &head)?;
    }
    Ok(head)
}

/// Replace the local tracks with a full read and take the snapshot of `head`.
fn reload(
    api: &WebApi,
    playlist: &mut Playlist,
    known: &HashMap<String, Playable>,
    head: &Head,
) -> Result<(), InternalError> {
    let tracks = api.playlist_playables(&playlist.id, known)?;
    set_tracks(playlist, tracks, head);
    Ok(())
}

/// Store `tracks` (renumbered by position) with the snapshot of `head`.
fn set_tracks(playlist: &mut Playlist, mut tracks: Vec<Playable>, head: &Head) {
    for (index, playable) in tracks.iter_mut().enumerate() {
        playable.set_list_index(index);
    }
    playlist.num_tracks = tracks.len();
    playlist.tracks = Some(tracks);
    playlist.snapshot_id = head.snapshot_id.clone();
}

/// POST `ops` against `base`, then read the head: the new head when its length is
/// `expected_len`, else reload the local copy and fail with [EditError::Mismatch]. Every other
/// error here comes after the POST was sent, so it is [EditError::Unverified].
fn send_verified(
    api: &WebApi,
    playlist: &mut Playlist,
    known: &HashMap<String, Playable>,
    base: Vec<u8>,
    ops: Vec<Op>,
    expected_len: usize,
) -> Result<Head, EditError> {
    api.playlist_changes(&playlist.id, list_changes(base, ops))
        .map_err(EditError::Unverified)?;
    let after = api
        .playlist_head(&playlist.id)
        .map_err(EditError::Unverified)?;
    if verified(expected_len, &after) {
        return Ok(after);
    }
    warn!(
        "Playlist write did not verify: length {} after it, {expected_len} expected",
        after.length
    );
    if let Err(err) = reload(api, playlist, known, &after) {
        warn!("Could not reload the playlist after a write that did not verify: {err}");
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
    known: &HashMap<String, Playable>,
) -> Result<(), EditError> {
    if new.is_empty() {
        return Ok(());
    }
    let head = prepare_head(api, playlist, known)?;
    add_batches(
        api,
        playlist,
        known,
        head,
        new,
        &mut Spacer::new(PLAYLIST_FETCH_SPACING),
    )
}

/// One verified ADD per [ADD_BATCH] tracks, [PLAYLIST_FETCH_SPACING] between POSTs, each based
/// on the head verified after the previous one. Batches before a failure stay applied.
fn add_batches(
    api: &WebApi,
    playlist: &mut Playlist,
    known: &HashMap<String, Playable>,
    mut head: Head,
    new: &[Playable],
    spacer: &mut Spacer,
) -> Result<(), EditError> {
    for batch in add_chunks(new) {
        spaced(spacer);
        let expected = expected_len(head.length, Change::Add(batch.len())).unwrap_or_default();
        let ops = vec![add_op(&uris(batch), None)];
        let after = send_verified(api, playlist, known, head.revision, ops, expected)?;
        let mut tracks = playlist.tracks.take().unwrap_or_default();
        tracks.extend_from_slice(batch);
        set_tracks(playlist, tracks, &after);
        head = after;
    }
    Ok(())
}

/// Remove the item at `index`, which must still be `uri` once the local copy is current.
pub fn remove_track(
    api: &WebApi,
    playlist: &mut Playlist,
    index: usize,
    uri: &str,
    known: &HashMap<String, Playable>,
) -> Result<(), EditError> {
    let head = prepare_head(api, playlist, known)?;
    let tracks = playlist.tracks.as_deref().unwrap_or_default();
    if !index_still(tracks, index, uri) {
        return Err(EditError::Moved);
    }
    let expected = expected_len(head.length, Change::Remove).ok_or(EditError::Moved)?;
    let ops = vec![remove_at_op(index)];
    let after = send_verified(api, playlist, known, head.revision, ops, expected)?;
    let mut tracks = playlist.tracks.take().unwrap_or_default();
    tracks.remove(index);
    set_tracks(playlist, tracks, &after);
    Ok(())
}

/// Replace the contents with `tracks`: one atomic POST up to [OVERWRITE_ATOMIC_MAX] tracks,
/// above that the first POST carries the removal plus the first [OVERWRITE_ATOMIC_MAX] tracks
/// and the rest go as add batches.
pub fn overwrite(
    api: &WebApi,
    playlist: &mut Playlist,
    tracks: &[Playable],
    known: &HashMap<String, Playable>,
) -> Result<(), EditError> {
    let head = prepare_head(api, playlist, known)?;
    let (first, rest) = overwrite_split(tracks);
    let ops = overwrite_ops(head.length, &uris(first));
    let mut spacer = Spacer::new(PLAYLIST_FETCH_SPACING);
    let after = if ops.is_empty() {
        head
    } else {
        spaced(&mut spacer);
        let expected =
            expected_len(head.length, Change::Overwrite(first.len())).unwrap_or_default();
        send_verified(api, playlist, known, head.revision, ops, expected)?
    };
    set_tracks(playlist, first.to_vec(), &after);
    add_batches(api, playlist, known, after, rest, &mut spacer)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            length,
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
    fn index_must_still_point_at_the_selected_item() {
        let tracks = local("s", 3).tracks.unwrap();
        assert!(index_still(&tracks, 1, "spotify:track:1"));
        assert!(!index_still(&tracks, 1, "spotify:track:2"));
        assert!(!index_still(&tracks, 9, "spotify:track:1"));
    }

    #[test]
    fn verified_compares_the_head_length() {
        assert!(verified(3, &head("x", 3)));
        assert!(!verified(3, &head("x", 2)));
    }

    #[test]
    fn edit_errors_fail_without_fallback() {
        use crate::internal_api::fallback::{Action, decide_after};
        for err in [EditError::Moved, EditError::Mismatch] {
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
    fn set_tracks_renumbers_and_takes_the_head_snapshot() {
        let mut playlist = local("s", 0);
        let tracks = vec![
            track_playable("spotify:track:a"),
            track_playable("spotify:track:b"),
        ];
        set_tracks(
            &mut playlist,
            tracks.into_iter().rev().collect(),
            &head("t", 2),
        );
        assert_eq!(playlist.snapshot_id, "t");
        assert_eq!(playlist.num_tracks, 2);
        let tracks = playlist.tracks.unwrap();
        assert_eq!(tracks[0].uri(), "spotify:track:b");
        for (i, p) in tracks.iter().enumerate() {
            assert_eq!(p.list_index(), i);
        }
    }

    #[test]
    fn add_tracks_without_a_session_leaves_the_playlist_unchanged() {
        let api = WebApi::new(&crate::config::Config::new_for_test());
        let mut playlist = local("s", 2);
        let new = [track_playable("spotify:track:new")];
        let result = add_tracks(&api, &mut playlist, &new, &HashMap::new());
        assert!(matches!(
            result,
            Err(EditError::Internal(InternalError::NoSession))
        ));
        assert_eq!(playlist.snapshot_id, "s");
        assert_eq!(playlist.num_tracks, 2);
        let uris: Vec<String> = playlist.tracks.unwrap().iter().map(Playable::uri).collect();
        assert_eq!(uris, ["spotify:track:0", "spotify:track:1"]);
    }
}
