//! Playlist v2 writes (create, add, remove, rootlist edits) over the librespot session.
//!
//! The request builders are pure; the senders POST protobuf through the spclient. A reply never
//! proves a write applied, so callers verify with a head read.

use http::{HeaderMap, HeaderValue, Method, header};
use librespot_core::Session;
use librespot_protocol::playlist4_external::{
    Add, ChangeInfo, CreateListReply, Delta, Item, ListAttributes, ListAttributesPartialState,
    ListChanges, Op, OpList, Rem, SourceInfo, UpdateListAttributes, op::Kind, source_info::Client,
};
use log::debug;
use protobuf::{EnumOrUnknown, Message, MessageField};

use super::error::{InternalError, classify};

/// Tracks per ADD op.
pub const ADD_BATCH: usize = 100;
/// Most tracks an overwrite sends in one POST.
pub const OVERWRITE_ATOMIC_MAX: usize = 1000;

const PLAYLIST_URI_PREFIX: &str = "spotify:playlist:";

/// A playlist just created: its base62 id and the revision the service answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub id: String,
    pub revision: Vec<u8>,
}

fn op(kind: Kind) -> Op {
    Op {
        kind: Some(EnumOrUnknown::new(kind)),
        ..Default::default()
    }
}

fn items(uris: &[String]) -> Vec<Item> {
    uris.iter()
        .map(|uri| Item {
            uri: Some(uri.clone()),
            ..Default::default()
        })
        .collect()
}

/// The body that creates a playlist with `name` (and nothing else).
pub fn create_body(name: &str) -> OpList {
    let mut update = op(Kind::UPDATE_LIST_ATTRIBUTES);
    update.update_list_attributes = MessageField::some(UpdateListAttributes {
        new_attributes: MessageField::some(ListAttributesPartialState {
            values: MessageField::some(ListAttributes {
                name: Some(name.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    });
    OpList {
        ops: vec![update],
        ..Default::default()
    }
}

/// ADD at the end (`at == None`) or at `from_index`.
pub fn add_op(uris: &[String], at: Option<usize>) -> Op {
    let mut add = Add {
        items: items(uris),
        ..Default::default()
    };
    match at {
        None => add.add_last = Some(true),
        Some(index) => add.from_index = Some(to_i32(index)),
    }
    let mut out = op(Kind::ADD);
    out.add = MessageField::some(add);
    out
}

/// REM of one item by position. Never by key: a key removes every copy of a duplicate.
pub fn remove_at_op(index: usize) -> Op {
    remove_range_op(index, 1)
}

fn remove_range_op(index: usize, length: usize) -> Op {
    let mut out = op(Kind::REM);
    out.rem = MessageField::some(Rem {
        from_index: Some(to_i32(index)),
        length: Some(to_i32(length)),
        ..Default::default()
    });
    out
}

/// REM of the current contents (when `current_len > 0`) then ADD ops of up to [ADD_BATCH]
/// items at from_index 0, 100, 200, ...; at most [OVERWRITE_ATOMIC_MAX] uris.
pub fn overwrite_ops(current_len: usize, uris: &[String]) -> Vec<Op> {
    let uris = &uris[..uris.len().min(OVERWRITE_ATOMIC_MAX)];
    let mut ops = Vec::new();
    if current_len > 0 {
        ops.push(remove_range_op(0, current_len));
    }
    for (n, chunk) in uris.chunks(ADD_BATCH).enumerate() {
        ops.push(add_op(chunk, Some(n * ADD_BATCH)));
    }
    ops
}

/// ADD a playlist at the top of the rootlist.
pub fn rootlist_add_op(uri: &str) -> Op {
    let mut add = Add {
        from_index: Some(0),
        add_first: Some(true),
        ..Default::default()
    };
    add.items = items(&[uri.to_string()]);
    let mut out = op(Kind::ADD);
    out.add = MessageField::some(add);
    out
}

/// REM of one rootlist entry by position.
pub fn rootlist_remove_op(index: usize) -> Op {
    remove_at_op(index)
}

/// A change set against `base_revision`, as the desktop client sends it.
pub fn list_changes(base_revision: Vec<u8>, ops: Vec<Op>) -> ListChanges {
    let delta = Delta {
        ops,
        info: MessageField::some(ChangeInfo {
            source: MessageField::some(SourceInfo {
                client: Some(EnumOrUnknown::new(Client::CLIENT)),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    ListChanges {
        base_revision: Some(base_revision),
        deltas: vec![delta],
        want_resulting_revisions: Some(true),
        ..Default::default()
    }
}

fn to_i32(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

fn encode(message: &impl Message) -> Result<Vec<u8>, InternalError> {
    message
        .write_to_bytes()
        .map_err(|e| InternalError::ProtocolChanged(format!("playlist write encode: {e}")))
}

/// The id and revision of a create reply; a reply without a playlist URI is a protocol change.
fn parse_created(bytes: &[u8]) -> Result<Created, InternalError> {
    let reply = CreateListReply::parse_from_bytes(bytes)
        .map_err(|e| InternalError::ProtocolChanged(format!("playlist create reply: {e}")))?;
    let id = reply
        .uri
        .as_deref()
        .and_then(|uri| uri.strip_prefix(PLAYLIST_URI_PREFIX))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            InternalError::ProtocolChanged("playlist create reply without a playlist URI".into())
        })?;
    Ok(Created {
        id: id.to_string(),
        revision: reply.revision.unwrap_or_default(),
    })
}

/// POST a protobuf body. Only `Content-Type` is set: the service answers 400 when asked for
/// another `Accept`.
async fn post_protobuf(
    session: &Session,
    endpoint: &str,
    body: Vec<u8>,
) -> Result<Vec<u8>, InternalError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-protobuf"),
    );
    session
        .spclient()
        .request(
            &Method::POST,
            endpoint,
            Some(headers),
            Some(body.as_slice()),
        )
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|e| classify(&e))
}

/// Create a playlist named `name`. The caller still adds it to the rootlist.
pub async fn create(session: Session, name: String) -> Result<Created, InternalError> {
    debug!("playlist create");
    let bytes = post_protobuf(
        &session,
        "/playlist/v2/playlist",
        encode(&create_body(&name))?,
    )
    .await?;
    parse_created(&bytes)
}

/// Apply `body` to playlist `id` (base62). The reply is ignored: verify with a head read.
pub async fn changes(session: Session, id: String, body: ListChanges) -> Result<(), InternalError> {
    debug!("playlist changes");
    let id = librespot_core::SpotifyId::from_base62(&id).map_err(|_| InternalError::NotFound)?;
    let endpoint = format!("/playlist/v2/playlist/{}/changes", id.to_base62());
    post_protobuf(&session, &endpoint, encode(&body)?)
        .await
        .map(|_| ())
}

/// Apply `body` to the user's rootlist. The reply is ignored: verify with a head read.
pub async fn rootlist_changes(session: Session, body: ListChanges) -> Result<(), InternalError> {
    debug!("rootlist changes");
    let endpoint = format!("/playlist/v2/user/{}/rootlist/changes", session.username());
    post_protobuf(&session, &endpoint, encode(&body)?)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_protocol::playlist4_external::op::Kind;

    fn kind(op: &Op) -> Kind {
        op.kind.unwrap().enum_value().unwrap()
    }

    #[test]
    fn create_body_sets_only_the_name() {
        let body = create_body("ncspot-test-1");
        assert_eq!(body.ops.len(), 1);
        assert_eq!(kind(&body.ops[0]), Kind::UPDATE_LIST_ATTRIBUTES);
        let values = &body.ops[0].update_list_attributes.new_attributes.values;
        assert_eq!(values.name.as_deref(), Some("ncspot-test-1"));
    }

    #[test]
    fn add_goes_last_or_at_an_index() {
        let uris = vec!["spotify:track:a".to_string(), "spotify:track:b".to_string()];
        let last = add_op(&uris, None);
        assert_eq!(kind(&last), Kind::ADD);
        assert_eq!(last.add.add_last, Some(true));
        assert_eq!(last.add.from_index, None);
        assert_eq!(
            last.add.items.iter().map(|i| i.uri()).collect::<Vec<_>>(),
            ["spotify:track:a", "spotify:track:b"]
        );
        let at = add_op(&uris, Some(3));
        assert_eq!(at.add.from_index, Some(3));
        assert_eq!(at.add.add_last, None);
    }

    #[test]
    fn remove_is_index_only() {
        let op = remove_at_op(7);
        assert_eq!(kind(&op), Kind::REM);
        assert_eq!((op.rem.from_index, op.rem.length), (Some(7), Some(1)));
        assert!(op.rem.items.is_empty());
        assert_eq!(op.rem.items_as_key, None);
    }

    #[test]
    fn overwrite_removes_then_adds_in_chunks() {
        let uris: Vec<String> = (0..250).map(|i| format!("spotify:track:{i}")).collect();
        let ops = overwrite_ops(12, &uris);
        assert_eq!(ops.len(), 4);
        assert_eq!(kind(&ops[0]), Kind::REM);
        assert_eq!(
            (ops[0].rem.from_index, ops[0].rem.length),
            (Some(0), Some(12))
        );
        let adds: Vec<(Option<i32>, usize)> = ops[1..]
            .iter()
            .map(|o| (o.add.from_index, o.add.items.len()))
            .collect();
        assert_eq!(adds, [(Some(0), 100), (Some(100), 100), (Some(200), 50)]);
        assert!(
            overwrite_ops(0, &uris[..1])
                .iter()
                .all(|o| kind(o) == Kind::ADD),
            "nothing to remove"
        );
    }

    #[test]
    fn rootlist_ops() {
        let add = rootlist_add_op("spotify:playlist:x");
        assert_eq!(
            (add.add.from_index, add.add.add_first),
            (Some(0), Some(true))
        );
        let rem = rootlist_remove_op(4);
        assert_eq!((rem.rem.from_index, rem.rem.length), (Some(4), Some(1)));
        assert!(rem.rem.items.is_empty());
    }

    #[test]
    fn list_changes_carries_base_source_and_resulting_revisions() {
        let body = list_changes(b"base".to_vec(), vec![remove_at_op(0)]);
        assert_eq!(body.base_revision.as_deref(), Some(&b"base"[..]));
        assert_eq!(body.want_resulting_revisions, Some(true));
        assert_eq!(body.deltas.len(), 1);
        let client = body.deltas[0]
            .info
            .source
            .client
            .unwrap()
            .enum_value()
            .unwrap();
        assert_eq!(
            client,
            librespot_protocol::playlist4_external::source_info::Client::CLIENT
        );
    }

    #[test]
    fn create_reply_gives_the_base62_id_and_revision() {
        let reply = CreateListReply {
            uri: Some("spotify:playlist:abc123".into()),
            revision: Some(b"rev".to_vec()),
            ..Default::default()
        };
        let bytes = reply.write_to_bytes().unwrap();
        assert_eq!(
            parse_created(&bytes),
            Ok(Created {
                id: "abc123".into(),
                revision: b"rev".to_vec()
            })
        );
        let other = CreateListReply {
            uri: Some("spotify:user:x".into()),
            ..Default::default()
        };
        assert!(matches!(
            parse_created(&other.write_to_bytes().unwrap()),
            Err(InternalError::ProtocolChanged(_))
        ));
        assert!(matches!(
            parse_created(&[0xff, 0xff]),
            Err(InternalError::ProtocolChanged(_))
        ));
    }
}
