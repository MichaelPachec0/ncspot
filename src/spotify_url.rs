use std::fmt;

use crate::spotify::UriType;

use url::{Host, Url};

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SpotifyUrl {
    pub id: String,
    pub uri_type: UriType,
}

impl fmt::Display for SpotifyUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let type_seg = match self.uri_type {
            UriType::Album => "album",
            UriType::Artist => "artist",
            UriType::Episode => "episode",
            UriType::Playlist => "playlist",
            UriType::Show => "show",
            UriType::Track => "track",
        };
        write!(f, "https://open.spotify.com/{}/{}", type_seg, self.id)
    }
}

impl SpotifyUrl {
    pub fn new(id: &str, uri_type: UriType) -> Self {
        Self {
            id: id.to_string(),
            uri_type,
        }
    }

    /// Get media id and type from open.spotify.com url
    ///
    /// ```
    /// let result = spotify_url::SpotifyURL::from_url("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC").unwrap();
    /// assert_eq!(result.id, "4uLU6hMCjMI75M1A2tKUQC");
    /// assert_eq!(result.uri_type, URIType::Track);
    /// ```
    pub fn from_url<S: AsRef<str>>(s: S) -> Option<Self> {
        let url = Url::parse(s.as_ref()).ok()?;
        if url.host() != Some(Host::Domain("open.spotify.com")) {
            return None;
        }

        let mut path_segments = url.path_segments()?;

        let mut entity = path_segments.next()?;

        if entity.to_lowercase().as_str().starts_with("intl-") {
            entity = path_segments.next()?
        }
        let uri_type = match entity.to_lowercase().as_str() {
            "album" => Some(UriType::Album),
            "artist" => Some(UriType::Artist),
            "episode" => Some(UriType::Episode),
            "playlist" => Some(UriType::Playlist),
            "show" => Some(UriType::Show),
            "track" => Some(UriType::Track),
            "user" => {
                let _user_id = path_segments.next()?;
                let entity = path_segments.next()?;

                if entity != "playlist" {
                    return None;
                }

                Some(UriType::Playlist)
            }
            _ => None,
        }?;

        let id = path_segments.next()?;

        Some(Self::new(id, uri_type))
    }

    /// Parse a `spotify:` URI or an open.spotify.com URL.
    pub fn parse_any(s: &str) -> Option<Self> {
        if s.contains("open.spotify.com") {
            Self::from_url(s)
        } else {
            let uri_type: UriType = s.parse().ok()?;
            let id = &s[s.rfind(':').map_or(0, |i| i + 1)..];
            (!id.is_empty()).then(|| Self::new(id, uri_type))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::SpotifyUrl;
    use crate::spotify::UriType;

    #[test]
    fn test_urls() {
        let mut test_cases = HashMap::new();
        test_cases.insert(
            "https://open.spotify.com/playlist/1XFxe8bkTryTODn0lk4CNa?si=FfSpZ6KPQdieClZbwHakOQ",
            SpotifyUrl::new("1XFxe8bkTryTODn0lk4CNa", UriType::Playlist),
        );
        test_cases.insert(
            "https://open.spotify.com/track/6fRJg3R90w0juYoCJXxj2d",
            SpotifyUrl::new("6fRJg3R90w0juYoCJXxj2d", UriType::Track),
        );
        test_cases.insert(
            "https://open.spotify.com/intl-pt/track/3Kj2M9gRU1Lwf5eiNjBtBp",
            SpotifyUrl::new("3Kj2M9gRU1Lwf5eiNjBtBp", UriType::Track),
        );
        test_cases.insert(
            "https://open.spotify.com/user/~villainy~/playlist/0OgoSs65CLDPn6AF6tsZVg",
            SpotifyUrl::new("0OgoSs65CLDPn6AF6tsZVg", UriType::Playlist),
        );
        test_cases.insert(
            "https://open.spotify.com/show/4MZfJbM2MXzZdPbv6gi5lJ",
            SpotifyUrl::new("4MZfJbM2MXzZdPbv6gi5lJ", UriType::Show),
        );
        test_cases.insert(
            "https://open.spotify.com/episode/3QE6rfmjRaeqXSqeWcIWF6",
            SpotifyUrl::new("3QE6rfmjRaeqXSqeWcIWF6", UriType::Episode),
        );
        test_cases.insert(
            "https://open.spotify.com/artist/6LEeAFiJF8OuPx747e1wxR",
            SpotifyUrl::new("6LEeAFiJF8OuPx747e1wxR", UriType::Artist),
        );

        for case in test_cases {
            let result = SpotifyUrl::from_url(case.0).unwrap();
            assert_eq!(result.id, case.1.id);
            assert_eq!(result.uri_type, case.1.uri_type);
        }
    }

    #[test]
    fn parse_any_accepts_uris_and_urls() {
        let album = SpotifyUrl::parse_any("spotify:album:4LH4d3cOWNNsVw41Gqt2kv").unwrap();
        assert_eq!(album.id, "4LH4d3cOWNNsVw41Gqt2kv");
        assert_eq!(album.uri_type, UriType::Album);

        let playlist =
            SpotifyUrl::parse_any("spotify:user:someone:playlist:37i9dQZF1DXcBWIGoYBM5M").unwrap();
        assert_eq!(playlist.id, "37i9dQZF1DXcBWIGoYBM5M");
        assert_eq!(playlist.uri_type, UriType::Playlist);

        let track =
            SpotifyUrl::parse_any("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC").unwrap();
        assert_eq!(track.id, "4uLU6hMCjMI75M1A2tKUQC");
        assert_eq!(track.uri_type, UriType::Track);
    }

    #[test]
    fn parse_any_rejects_other_input() {
        assert!(SpotifyUrl::parse_any("").is_none());
        assert!(SpotifyUrl::parse_any("spotify:nothing:abc").is_none());
        assert!(SpotifyUrl::parse_any("https://example.com/track/abc").is_none());
    }
}
