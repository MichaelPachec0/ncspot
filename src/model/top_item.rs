use std::sync::Arc;

use crate::internal_api::search::Kind;
use crate::library::Library;
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::episode::Episode;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::track::Track;
use crate::queue::Queue;
use crate::search_api::TopRow;
use crate::traits::{ListItem, ViewExt};

/// A row of the search view's Top tab.
#[derive(Clone)]
pub enum TopItem {
    Track(Track),
    Album(Album),
    Artist(Artist),
    Playlist(Playlist),
    Show(Show),
    Episode(Episode),
    /// A featured playlist: drawn without a kind word.
    Featured(Playlist),
    /// A row of a kind's section: drawn without a kind word, otherwise the wrapped row.
    /// Only ever wraps Track, Album, Artist, Playlist, Show or Episode.
    Section(Box<Self>),
    /// A rule line with this label.
    Rule(String),
}

/// The label of the rule line above the featured playlists.
pub const FEATURED_LABEL: &str = "Featured playlists";

impl From<TopRow> for TopItem {
    fn from(row: TopRow) -> Self {
        match row {
            TopRow::Track(row) => Self::Track(Track::from(&row)),
            TopRow::Album(row) => Self::Album(Album::from(&row)),
            TopRow::Artist(row) => Self::Artist(Artist::from(&row)),
            TopRow::Playlist(row) => Self::Playlist(row),
            TopRow::Show(row) => Self::Show(Show::from(&row)),
            TopRow::Episode(row) => Self::Episode(Episode::from(&row)),
        }
    }
}

/// The label of a kind's section: its tab name.
pub fn section_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Track => "Tracks",
        Kind::Album => "Albums",
        Kind::Artist => "Artists",
        Kind::Playlist => "Playlists",
        Kind::Show => "Shows",
        Kind::Episode => "Episodes",
    }
}

/// The rows of the Top tab: `rows`; then for each section, in the order given,
/// `Rule(section_label(kind))` and its rows as `Section`; then, only if `featured` is not
/// empty, `Rule(FEATURED_LABEL)` and the featured playlists. A section without rows is
/// skipped.
pub fn top_items(
    rows: Vec<TopRow>,
    sections: Vec<(Kind, Vec<TopRow>)>,
    featured: &[Playlist],
) -> Vec<TopItem> {
    let mut items: Vec<TopItem> = rows.into_iter().map(TopItem::from).collect();
    for (kind, rows) in sections {
        if rows.is_empty() {
            continue;
        }
        items.push(TopItem::Rule(section_label(kind).to_string()));
        for row in rows {
            items.push(TopItem::Section(Box::new(TopItem::from(row))));
        }
    }
    if !featured.is_empty() {
        items.push(TopItem::Rule(FEATURED_LABEL.to_string()));
        items.extend(featured.iter().cloned().map(TopItem::Featured));
    }
    items
}

impl TopItem {
    /// Whether this is an item row (anything but `Rule`, also when wrapped in `Section`).
    pub fn is_item(&self) -> bool {
        match self {
            Self::Rule(_) => false,
            Self::Section(i) => i.is_item(),
            _ => true,
        }
    }

    /// The model inside, `None` for the rule.
    fn inner(&self) -> Option<&dyn ListItem> {
        match self {
            Self::Track(i) => Some(i),
            Self::Album(i) => Some(i),
            Self::Artist(i) => Some(i),
            Self::Playlist(i) | Self::Featured(i) => Some(i),
            Self::Show(i) => Some(i),
            Self::Episode(i) => Some(i),
            Self::Section(i) => i.inner(),
            Self::Rule(_) => None,
        }
    }

    /// The model inside, `None` for the rule.
    fn inner_mut(&mut self) -> Option<&mut dyn ListItem> {
        match self {
            Self::Track(i) => Some(i),
            Self::Album(i) => Some(i),
            Self::Artist(i) => Some(i),
            Self::Playlist(i) | Self::Featured(i) => Some(i),
            Self::Show(i) => Some(i),
            Self::Episode(i) => Some(i),
            Self::Section(i) => i.inner_mut(),
            Self::Rule(_) => None,
        }
    }
}

impl ListItem for TopItem {
    fn is_playing(&self, queue: &Queue) -> bool {
        self.inner().is_some_and(|i| i.is_playing(queue))
    }

    fn display_left(&self, library: &Library) -> String {
        self.inner()
            .map(|i| i.display_left(library))
            .unwrap_or_default()
    }

    fn display_right(&self, library: &Library) -> String {
        self.inner()
            .map(|i| i.display_right(library))
            .unwrap_or_default()
    }

    fn play(&mut self, queue: &Queue) {
        if let Some(i) = self.inner_mut() {
            i.play(queue);
        }
    }

    fn play_next(&mut self, queue: &Queue) {
        if let Some(i) = self.inner_mut() {
            i.play_next(queue);
        }
    }

    fn queue(&mut self, queue: &Queue) {
        if let Some(i) = self.inner_mut() {
            i.queue(queue);
        }
    }

    fn toggle_saved(&mut self, library: &Library) {
        if let Some(i) = self.inner_mut() {
            i.toggle_saved(library);
        }
    }

    fn save(&mut self, library: &Library) {
        if let Some(i) = self.inner_mut() {
            i.save(library);
        }
    }

    fn unsave(&mut self, library: &Library) {
        if let Some(i) = self.inner_mut() {
            i.unsave(library);
        }
    }

    fn open(&self, queue: Arc<Queue>, library: Arc<Library>) -> Option<Box<dyn ViewExt>> {
        self.inner()?.open(queue, library)
    }

    fn open_recommendations(
        &mut self,
        queue: Arc<Queue>,
        library: Arc<Library>,
    ) -> Option<Box<dyn ViewExt>> {
        self.inner_mut()?.open_recommendations(queue, library)
    }

    fn share_url(&self) -> Option<String> {
        self.inner()?.share_url()
    }

    fn album(&self, queue: &Queue) -> Option<Album> {
        self.inner()?.album(queue)
    }

    fn artists(&self) -> Option<Vec<Artist>> {
        self.inner()?.artists()
    }

    fn track(&self) -> Option<Track> {
        self.inner()?.track()
    }

    fn playlist_id(&self) -> Option<String> {
        self.inner()?.playlist_id()
    }

    fn rule_label(&self) -> Option<String> {
        match self {
            Self::Rule(label) => Some(label.clone()),
            Self::Section(i) => i.rule_label(),
            _ => None,
        }
    }

    fn display_center(&self, library: &Library) -> String {
        match self {
            Self::Section(i) => i
                .inner()
                .map(|m| m.display_center(library))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn display_tag(&self, _library: &Library) -> Option<String> {
        let word = match self {
            Self::Artist(_) => "Artist",
            Self::Track(_) => "Track",
            Self::Album(_) => "Album",
            Self::Playlist(_) => "Playlist",
            Self::Show(_) => "Show",
            Self::Episode(_) => "Episode",
            Self::Featured(_) | Self::Section(_) | Self::Rule(_) => return None,
        };
        Some(word.to_string())
    }

    fn is_saved(&self, library: &Library) -> Option<bool> {
        self.inner()?.is_saved(library)
    }

    fn is_playable(&self) -> bool {
        self.inner().is_some_and(|i| i.is_playable())
    }

    fn id(&self) -> Option<String> {
        self.inner()?.id()
    }

    fn merge_loaded(&mut self, loaded: Self) {
        match (self, loaded) {
            (Self::Track(a), Self::Track(b)) => a.merge_loaded(b),
            (Self::Album(a), Self::Album(b)) => a.merge_loaded(b),
            (Self::Artist(a), Self::Artist(b)) => a.merge_loaded(b),
            (Self::Playlist(a), Self::Playlist(b)) => a.merge_loaded(b),
            (Self::Show(a), Self::Show(b)) => a.merge_loaded(b),
            (Self::Episode(a), Self::Episode(b)) => a.merge_loaded(b),
            (Self::Featured(a), Self::Featured(b)) => a.merge_loaded(b),
            (Self::Section(a), Self::Section(b)) => a.merge_loaded(*b),
            _ => {}
        }
    }

    fn prepare_open(&mut self, queue: &Queue) {
        if let Some(i) = self.inner_mut() {
            i.prepare_open(queue);
        }
    }

    fn as_listitem(&self) -> Box<dyn ListItem> {
        match self.inner() {
            Some(i) => i.as_listitem(),
            None => Box::new(self.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ConfigValues, TrackFormat};
    use crate::events::EventManager;
    use crate::model::playable::Playable;
    use crate::save_log::test_items::{
        album, artist, episode, playable_track, playlist, show, track,
    };
    use crate::spotify::Spotify;
    use crate::ui::contextmenu::ContextMenu;
    use rspotify::model::{
        AlbumId, ArtistId, FullArtist, FullTrack, SimplifiedAlbum, TrackId, Type,
    };
    use std::collections::HashMap;

    // `full_track` and `full_artist` set fields that rspotify marks as deprecated because Spotify
    // removed them; the structs have no other way to be built.
    #[allow(deprecated)]
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
            is_playable: Some(true),
            linked_from: None,
            restrictions: None,
            name: id.to_string(),
            popularity: 0,
            preview_url: None,
            track_number: 0,
            r#type: Type::Track,
        }
    }

    #[allow(deprecated)]
    fn full_artist(id: &str) -> FullArtist {
        FullArtist {
            external_urls: HashMap::new(),
            followers: Default::default(),
            genres: Vec::new(),
            href: String::new(),
            id: ArtistId::from_id(id.to_string()).unwrap(),
            images: Vec::new(),
            name: id.to_string(),
            popularity: 0,
        }
    }

    fn album_row(id: &str) -> SimplifiedAlbum {
        SimplifiedAlbum {
            id: Some(AlbumId::from_id(id.to_string()).unwrap()),
            ..Default::default()
        }
    }

    fn section_track(id: &str) -> TopItem {
        TopItem::Section(Box::new(TopItem::from(TopRow::Track(full_track(id)))))
    }

    fn setup_with(cfg: Arc<Config>) -> (Arc<Queue>, Arc<Library>) {
        let ev = EventManager::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        let library = Library::new_for_test(ev, spotify.clone(), cfg.clone());
        let queue = Arc::new(Queue::new(spotify, cfg, library.clone()));
        (queue, library)
    }

    fn setup() -> (Arc<Queue>, Arc<Library>) {
        setup_with(Config::new_for_test())
    }

    fn six() -> Vec<TopItem> {
        vec![
            TopItem::Track(track("t1", &[("a1", "A")])),
            TopItem::Album(album("b1")),
            TopItem::Artist(artist("a1", "A", false)),
            TopItem::Playlist(playlist("p1")),
            TopItem::Show(show("s1")),
            TopItem::Episode(episode("e1")),
        ]
    }

    fn kinds(items: &[TopItem]) -> Vec<&'static str> {
        items
            .iter()
            .map(|i| match i {
                TopItem::Track(_) => "track",
                TopItem::Album(_) => "album",
                TopItem::Artist(_) => "artist",
                TopItem::Playlist(_) => "playlist",
                TopItem::Show(_) => "show",
                TopItem::Episode(_) => "episode",
                TopItem::Featured(_) => "featured",
                TopItem::Section(inner) => kinds(std::slice::from_ref(inner))[0],
                TopItem::Rule(_) => "rule",
            })
            .collect()
    }

    fn rows() -> Vec<TopRow> {
        vec![
            TopRow::Playlist(playlist("p1")),
            TopRow::Playlist(playlist("p2")),
        ]
    }

    #[test]
    fn top_items_put_the_rule_before_featured_and_omit_it_without_featured() {
        let featured = [playlist("f1"), playlist("f2")];
        assert_eq!(
            kinds(&top_items(rows(), Vec::new(), &featured)),
            ["playlist", "playlist", "rule", "featured", "featured"]
        );
        assert_eq!(
            kinds(&top_items(rows(), Vec::new(), &[])),
            ["playlist", "playlist"]
        );
        assert_eq!(
            kinds(&top_items(Vec::new(), Vec::new(), &featured)),
            ["rule", "featured", "featured"]
        );
    }

    #[test]
    fn each_kind_shows_its_word_in_the_tag_column_and_no_center() {
        let (_queue, library) = setup();
        let mut with_album = track("t1", &[]);
        with_album.album = Some("Some Album".into());
        let mut items = six();
        items[0] = TopItem::Track(with_album);
        let words: Vec<Option<String>> = items.iter().map(|i| i.display_tag(&library)).collect();
        let expected = ["Track", "Album", "Artist", "Playlist", "Show", "Episode"];
        assert_eq!(
            words,
            expected
                .iter()
                .map(|w| Some(w.to_string()))
                .collect::<Vec<_>>()
        );
        for item in &items {
            assert_eq!(item.display_center(&library), "");
        }
        assert_eq!(
            TopItem::Featured(playlist("f1")).display_tag(&library),
            None
        );
        assert_eq!(
            TopItem::Rule(FEATURED_LABEL.into()).display_tag(&library),
            None
        );
    }

    #[test]
    fn a_custom_track_format_center_is_not_shown() {
        let values = ConfigValues {
            track_format: Some(TrackFormat {
                left: None,
                center: Some("%title".into()),
                right: None,
            }),
            ..Default::default()
        };
        let (_queue, library) = setup_with(Config::new_for_test_with_values(values));
        let t = track("t1", &[("a1", "A")]);
        assert_ne!(t.display_center(&library), "", "the format sets the center");
        let item = TopItem::Track(t.clone());
        assert_eq!(item.display_center(&library), "");
        assert_eq!(item.display_left(&library), t.display_left(&library));
    }

    #[test]
    fn left_and_right_are_those_of_the_inner_item() {
        let (_queue, library) = setup();
        let mut items = six();
        items.push(TopItem::Featured(playlist("f1")));
        for item in &items {
            let inner = item.as_listitem();
            assert_eq!(item.display_left(&library), inner.display_left(&library));
            assert_eq!(item.display_right(&library), inner.display_right(&library));
        }
    }

    #[test]
    fn the_rule_row_is_inert() {
        let (queue, library) = setup();
        let mut rule = TopItem::Rule(FEATURED_LABEL.into());
        assert_eq!(rule.rule_label().as_deref(), Some(FEATURED_LABEL));
        assert!(!rule.is_item());
        assert!(rule.id().is_none());
        assert!(!rule.is_playable());
        assert!(!rule.is_playing(&queue));
        assert_eq!(rule.display_left(&library), "");
        assert_eq!(rule.display_right(&library), "");
        assert_eq!(rule.display_center(&library), "");
        assert!(rule.share_url().is_none());
        assert!(rule.track().is_none());
        assert!(rule.artists().is_none());
        assert!(rule.album(&queue).is_none());
        assert!(rule.playlist_id().is_none());
        assert!(rule.is_saved(&library).is_none());
        assert!(rule.open(queue.clone(), library.clone()).is_none());
        assert!(
            rule.open_recommendations(queue.clone(), library.clone())
                .is_none()
        );
        rule.play(&queue);
        rule.play_next(&queue);
        rule.queue(&queue);
        rule.toggle_saved(&library);
        rule.save(&library);
        rule.unsave(&library);
        rule.prepare_open(&queue);
        assert!(queue.list().read().is_empty());
        assert!(!rule.as_listitem().is_playable());
        assert_eq!(
            rule.as_listitem().rule_label().as_deref(),
            Some(FEATURED_LABEL)
        );
        assert!(TopItem::Featured(playlist("f1")).rule_label().is_none());
    }

    #[test]
    fn play_on_a_track_row_queues_exactly_that_track() {
        let (queue, _library) = setup();
        let mut item = TopItem::Track(playable_track("t1"));
        item.play(&queue);
        let list = queue.list();
        let list = list.read();
        assert_eq!(list.len(), 1);
        assert!(matches!(&list[0], Playable::Track(t) if t.id.as_deref() == Some("t1")));
    }

    #[test]
    fn the_context_menu_of_a_track_row_offers_play_next_and_queue() {
        let (queue, library) = setup();
        let item = TopItem::Track(playable_track("t1")).as_listitem();
        let mut menu = ContextMenu::new(item.as_ref(), queue, library);
        let labels = menu.get_mut().labels_for_test();
        assert!(labels.iter().any(|l| l == "Play next"), "{labels:?}");
        assert!(labels.iter().any(|l| l == "Queue"), "{labels:?}");
    }

    #[test]
    fn ids_prepare_open_and_merge_pass_through() {
        let (queue, _library) = setup();
        let items = six();
        let ids: Vec<Option<String>> = items.iter().map(|i| i.id()).collect();
        let expected = ["t1", "b1", "a1", "p1", "s1", "e1"];
        assert_eq!(
            ids,
            expected
                .iter()
                .map(|i| Some(i.to_string()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            TopItem::Featured(playlist("f1")).id().as_deref(),
            Some("f1")
        );

        let mut row = TopItem::Playlist(playlist("p1"));
        let mut loaded_playlist = playlist("p1");
        loaded_playlist.tracks = Some(vec![Playable::Track(playable_track("t1"))]);
        row.merge_loaded(TopItem::Playlist(loaded_playlist));
        match &row {
            TopItem::Playlist(p) => assert_eq!(p.tracks.as_ref().map(Vec::len), Some(1)),
            _ => panic!("the variant changed"),
        }

        let mut other = TopItem::Playlist(playlist("p1"));
        let mut loaded = playlist("p1");
        loaded.tracks = Some(vec![Playable::Track(playable_track("t1"))]);
        other.merge_loaded(TopItem::Featured(loaded));
        match &other {
            TopItem::Playlist(p) => assert!(p.tracks.is_none(), "a different variant is ignored"),
            _ => panic!("the variant changed"),
        }

        // prepare_open on a row that loads nothing leaves it alone.
        let mut track_row = TopItem::Track(track("t1", &[]));
        track_row.prepare_open(&queue);
        assert_eq!(track_row.id().as_deref(), Some("t1"));
    }

    #[test]
    fn open_save_state_and_share_pass_through_per_kind() {
        let (queue, library) = setup();
        for item in six() {
            let inner = item.as_listitem();
            assert_eq!(item.share_url(), inner.share_url());
            assert_eq!(item.is_saved(&library), inner.is_saved(&library));
            assert_eq!(
                item.open(queue.clone(), library.clone()).is_some(),
                inner.open(queue.clone(), library.clone()).is_some()
            );
        }
    }

    #[test]
    fn a_section_row_opens_and_shares_like_its_plain_row() {
        let (queue, library) = setup();
        for (plain, inner) in six().into_iter().zip(six()) {
            let section = TopItem::Section(Box::new(inner));
            assert_eq!(section.share_url(), plain.share_url());
            assert_eq!(section.is_saved(&library), plain.is_saved(&library));
            assert_eq!(
                section.open(queue.clone(), library.clone()).is_some(),
                plain.open(queue.clone(), library.clone()).is_some()
            );
        }
    }

    /// What `op` does to `item` on a fresh test library, as far as it shows without the network:
    /// the status notes it posted, and whether it left a playlist write that the writer then ran.
    /// The test library has no loaded store (a save posts the loading note and stops) and is not
    /// done loading (a playlist follow or unfollow posts the loading note and stops), so no
    /// request leaves the process.
    fn effect_of(item: TopItem, op: impl FnOnce(&mut TopItem, &Library)) -> (Vec<String>, bool) {
        let ev = EventManager::new_for_test();
        let cfg = Config::new_for_test();
        let spotify = Spotify::new_for_test(cfg.clone(), ev.clone());
        let library = Library::new_for_test(ev.clone(), spotify, cfg);
        let mut item = item;
        op(&mut item, &library);
        let wrote = library.run_next_write();
        let notes = ev
            .msg_iter()
            .filter_map(|event| match event {
                crate::events::Event::StatusNote(note) => Some(note),
                _ => None,
            })
            .collect();
        (notes, wrote)
    }

    /// Check `op` on a section row of each kind in `kinds` (an index into `six`) against the
    /// plain row, and that the plain row did something visible, so a pair of empty effects
    /// cannot pass.
    fn assert_section_matches_plain(kinds: &[usize], op: fn(&mut TopItem, &Library)) {
        for &k in kinds {
            let plain = effect_of(six().swap_remove(k), op);
            assert_ne!(plain, (Vec::new(), false), "kind {k} shows no effect");
            let section = effect_of(TopItem::Section(Box::new(six().swap_remove(k))), op);
            assert_eq!(section, plain, "kind {k}");
        }
    }

    // Kinds by index in `six`: 0 track, 1 album, 2 artist, 3 playlist, 4 show, 5 episode.
    // The episode saves nothing while Your Episodes is off (the test library), and
    // `toggle_saved` of a playlist does nothing for a playlist the user does not follow (the
    // test library has no user), so those have no visible effect offline and are not checked.

    #[test]
    fn a_section_row_forwards_save_like_its_plain_row() {
        assert_section_matches_plain(&[0, 1, 2, 3, 4], |i, l| i.save(l));
    }

    #[test]
    fn a_section_row_forwards_unsave_like_its_plain_row() {
        assert_section_matches_plain(&[0, 1, 2, 3, 4], |i, l| i.unsave(l));
    }

    #[test]
    fn a_section_row_forwards_toggle_saved_like_its_plain_row() {
        assert_section_matches_plain(&[0, 1, 2, 4], |i, l| i.toggle_saved(l));
    }

    // `album(queue)` has no offline case: a track reads its album from Spotify when it has an
    // album id, and every other kind answers None, so no kind shows a difference without the
    // network and it is not checked.
    #[test]
    fn a_section_row_gives_the_artists_of_its_plain_row() {
        let artist_ids = |item: &TopItem| {
            item.artists()
                .map(|v| v.into_iter().map(|a| a.id).collect::<Vec<_>>())
        };
        for (plain, inner) in six().into_iter().zip(six()) {
            let section = TopItem::Section(Box::new(inner));
            assert_eq!(artist_ids(&section), artist_ids(&plain));
        }
        // Only a track and an album name artists; the plain rows must not both be empty.
        let two = [("a1", "A"), ("a2", "B")];
        let section = TopItem::Section(Box::new(TopItem::Track(track("t2", &two))));
        assert_eq!(
            artist_ids(&section),
            Some(vec![Some("a1".to_string()), Some("a2".to_string())])
        );
        let mut by = album("b2");
        by.artists = vec!["A".into()];
        by.artist_ids = vec!["a1".into()];
        let section = TopItem::Section(Box::new(TopItem::Album(by)));
        assert_eq!(artist_ids(&section), Some(vec![Some("a1".to_string())]));
    }

    #[test]
    fn top_items_put_sections_between_the_rows_and_featured() {
        let (_queue, library) = setup();
        let mixed = vec![
            TopRow::Artist(full_artist("a1")),
            TopRow::Track(full_track("t1")),
        ];
        let sections = vec![
            (
                Kind::Track,
                vec![
                    TopRow::Track(full_track("t2")),
                    TopRow::Track(full_track("t3")),
                ],
            ),
            (Kind::Album, vec![TopRow::Album(album_row("b1"))]),
        ];
        let featured = [playlist("f1")];
        let items = top_items(mixed, sections, &featured);
        assert_eq!(
            kinds(&items),
            [
                "artist", "track", "rule", "track", "track", "rule", "album", "rule", "featured"
            ]
        );
        let labels: Vec<String> = items.iter().filter_map(|i| i.rule_label()).collect();
        assert_eq!(labels, ["Tracks", "Albums", FEATURED_LABEL]);
        let tagged: Vec<bool> = items
            .iter()
            .filter(|i| i.is_item())
            .map(|i| i.display_tag(&library).is_some())
            .collect();
        assert_eq!(tagged, [true, true, false, false, false, false]);
    }

    #[test]
    fn top_items_skip_an_empty_section() {
        let sections = vec![
            (Kind::Track, Vec::new()),
            (Kind::Album, vec![TopRow::Album(album_row("b1"))]),
        ];
        let items = top_items(Vec::new(), sections, &[]);
        assert_eq!(kinds(&items), ["rule", "album"]);
        assert_eq!(items[0].rule_label().as_deref(), Some("Albums"));
    }

    #[test]
    fn a_section_row_has_no_tag_and_looks_like_its_tab_row() {
        let (_queue, library) = setup();
        let mut row = full_track("t1");
        row.album.name = "Some Album".into();
        let model = Track::from(&row);
        let plain = TopItem::Track(model.clone());
        let section = TopItem::Section(Box::new(TopItem::from(TopRow::Track(row))));
        assert_eq!(section.display_tag(&library), None);
        assert_eq!(section.display_left(&library), model.display_left(&library));
        assert_eq!(
            section.display_center(&library),
            model.display_center(&library)
        );
        assert_eq!(
            section.display_right(&library),
            model.display_right(&library)
        );
        assert_ne!(section.display_center(&library), "");
        assert_eq!(plain.display_center(&library), "");
        assert_eq!(section.id(), plain.id());
        assert_eq!(section.is_playable(), plain.is_playable());
        assert!(section.track().is_some());
        let artist = TopItem::Section(Box::new(TopItem::from(TopRow::Artist(full_artist("a1")))));
        assert!(artist.track().is_none());
    }

    #[test]
    fn a_section_track_shows_a_custom_center_format() {
        let values = ConfigValues {
            track_format: Some(TrackFormat {
                left: None,
                center: Some("%title".into()),
                right: None,
            }),
            ..Default::default()
        };
        let (_queue, library) = setup_with(Config::new_for_test_with_values(values));
        let section = section_track("t1");
        assert_eq!(section.display_center(&library), "t1");
    }

    #[test]
    fn enter_on_a_section_track_plays_that_one_track() {
        let (queue, _library) = setup();
        let mut item = section_track("t1");
        item.play(&queue);
        let list = queue.list();
        let list = list.read();
        assert_eq!(list.len(), 1);
        assert!(matches!(&list[0], Playable::Track(t) if t.id.as_deref() == Some("t1")));
    }

    #[test]
    fn a_section_track_offers_play_next_and_queue() {
        let (queue, library) = setup();
        let item = section_track("t1").as_listitem();
        let mut menu = ContextMenu::new(item.as_ref(), queue, library);
        let labels = menu.get_mut().labels_for_test();
        assert!(labels.iter().any(|l| l == "Play next"), "{labels:?}");
        assert!(labels.iter().any(|l| l == "Queue"), "{labels:?}");
    }

    #[test]
    fn a_section_that_wraps_a_rule_is_that_rule() {
        let section = TopItem::Section(Box::new(TopItem::Rule("Tracks".into())));
        assert_eq!(section.rule_label().as_deref(), Some("Tracks"));
        assert!(!section.is_item());
        assert!(section_track("t1").rule_label().is_none());
        assert!(section_track("t1").is_item());
    }

    #[test]
    fn a_loaded_section_row_merges_into_its_stale_twin() {
        let mut row = TopItem::Section(Box::new(TopItem::Playlist(playlist("p1"))));
        let mut loaded = playlist("p1");
        loaded.tracks = Some(vec![Playable::Track(playable_track("t1"))]);
        row.merge_loaded(TopItem::Section(Box::new(TopItem::Playlist(loaded))));
        match &row {
            TopItem::Section(inner) => match inner.as_ref() {
                TopItem::Playlist(p) => assert_eq!(p.tracks.as_ref().map(Vec::len), Some(1)),
                _ => panic!("the wrapped variant changed"),
            },
            _ => panic!("the variant changed"),
        }
    }

    #[test]
    fn section_labels_are_the_tab_names() {
        let labels: Vec<&str> = [
            Kind::Track,
            Kind::Album,
            Kind::Artist,
            Kind::Playlist,
            Kind::Show,
            Kind::Episode,
        ]
        .into_iter()
        .map(section_label)
        .collect();
        assert_eq!(
            labels,
            [
                "Tracks",
                "Albums",
                "Artists",
                "Playlists",
                "Shows",
                "Episodes"
            ]
        );
    }
}
