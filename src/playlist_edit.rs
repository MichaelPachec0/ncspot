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

/// A write applied when the head length after it is the expected one.
pub fn verified(expected_len: usize, after: &Head) -> bool {
    after.length == expected_len
}

#[derive(Debug)]
pub enum EditError {
    Internal(InternalError),
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

/// A moved index or a mismatch fails the action: `Transient` never falls back to the Web API.
impl From<EditError> for InternalError {
    fn from(err: EditError) -> Self {
        match err {
            EditError::Internal(err) => err,
            EditError::Moved => Self::Transient("the selected item moved".into()),
            EditError::Mismatch => Self::Transient("the playlist changed during the write".into()),
        }
    }
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
/// `expected_len`, else reload the local copy and fail with [EditError::Mismatch].
fn send_verified(
    api: &WebApi,
    playlist: &mut Playlist,
    known: &HashMap<String, Playable>,
    base: Vec<u8>,
    ops: Vec<Op>,
    expected_len: usize,
) -> Result<Head, EditError> {
    api.playlist_changes(&playlist.id, list_changes(base, ops))?;
    let after = api.playlist_head(&playlist.id)?;
    if verified(expected_len, &after) {
        return Ok(after);
    }
    warn!(
        "Playlist write did not verify: length {} after it, {expected_len} expected",
        after.length
    );
    reload(api, playlist, known, &after)?;
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
    for batch in new.chunks(ADD_BATCH) {
        spaced(spacer);
        let expected = head.length + batch.len();
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
    let expected = head.length.checked_sub(1).ok_or(EditError::Moved)?;
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
    let (first, rest) = tracks.split_at(tracks.len().min(OVERWRITE_ATOMIC_MAX));
    let ops = overwrite_ops(head.length, &uris(first));
    let mut spacer = Spacer::new(PLAYLIST_FETCH_SPACING);
    let after = if ops.is_empty() {
        head
    } else {
        spaced(&mut spacer);
        send_verified(api, playlist, known, head.revision, ops, first.len())?
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
