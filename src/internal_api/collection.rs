//! Spotify's collection v2 service (liked songs, saved albums, followed artists, saved shows,
//! Your Episodes) over the librespot session.

/// Messages generated from the vendored `collection2v2.proto` (librespot ships the file but
/// does not compile it).
#[allow(clippy::all, clippy::use_self, dead_code)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/collection_proto/mod.rs"));
    #[cfg_attr(not(test), expect(unused_imports))]
    pub use collection2v2::*;
}

#[cfg(test)]
mod tests {
    use super::proto::{CollectionItem, DeltaRequest, DeltaResponse, PageRequest, PageResponse};
    use protobuf::Message;

    #[test]
    fn page_request_round_trips() {
        let req = PageRequest {
            username: "u".into(),
            set: "collection".into(),
            pagination_token: "t".into(),
            limit: 300,
            ..Default::default()
        };
        let bytes = req.write_to_bytes().unwrap();
        assert_eq!(PageRequest::parse_from_bytes(&bytes).unwrap(), req);
    }

    #[test]
    fn delta_request_round_trips() {
        let req = DeltaRequest {
            username: "u".into(),
            set: "artist".into(),
            last_sync_token: "s".into(),
            ..Default::default()
        };
        let bytes = req.write_to_bytes().unwrap();
        assert_eq!(DeltaRequest::parse_from_bytes(&bytes).unwrap(), req);
    }

    #[test]
    fn page_response_decodes_fixed_bytes() {
        // items { uri: "spotify:track:a" added_at: 5 } next_page_token: "n" sync_token: ""
        let bytes = [
            0x0a, 0x13, 0x0a, 0x0f, b's', b'p', b'o', b't', b'i', b'f', b'y', b':', b't', b'r',
            b'a', b'c', b'k', b':', b'a', 0x10, 0x05, 0x12, 0x01, b'n',
        ];
        let resp = PageResponse::parse_from_bytes(&bytes).unwrap();
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].uri, "spotify:track:a");
        assert_eq!(resp.items[0].added_at, 5);
        assert_eq!(resp.next_page_token, "n");
        assert!(resp.sync_token.is_empty());
    }

    #[test]
    fn delta_response_decodes_fixed_bytes() {
        // delta_update_possible: true items { uri: "spotify:artist:b" is_removed: true } sync_token: "s"
        let bytes = [
            0x08, 0x01, 0x12, 0x14, 0x0a, 0x10, b's', b'p', b'o', b't', b'i', b'f', b'y', b':',
            b'a', b'r', b't', b'i', b's', b't', b':', b'b', 0x18, 0x01, 0x1a, 0x01, b's',
        ];
        let resp = DeltaResponse::parse_from_bytes(&bytes).unwrap();
        assert!(resp.delta_update_possible);
        assert_eq!(
            resp.items
                .first()
                .map(|i: &CollectionItem| (i.uri.as_str(), i.is_removed)),
            Some(("spotify:artist:b", true))
        );
        assert_eq!(resp.sync_token, "s");
    }
}
