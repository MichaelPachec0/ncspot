use std::sync::Arc;

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
#[cfg_attr(not(test), expect(dead_code, reason = "used by the search Top tab"))]
pub enum TopItem {
    Track(Track),
    Album(Album),
    Artist(Artist),
    Playlist(Playlist),
    Show(Show),
    Episode(Episode),
    /// A featured playlist: drawn without a kind word.
    Featured(Playlist),
    /// The rule line above the featured playlists.
    Rule,
}

/// The label of the rule line above the featured playlists.
#[cfg_attr(not(test), expect(dead_code, reason = "used by the search Top tab"))]
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

/// The rows of the Top tab: `rows`, then, only if `featured` is not empty, `Rule` and
/// the featured playlists.
#[cfg_attr(not(test), expect(dead_code, reason = "used by the search Top tab"))]
pub fn top_items(rows: Vec<TopRow>, featured: &[Playlist]) -> Vec<TopItem> {
    let mut items: Vec<TopItem> = rows.into_iter().map(TopItem::from).collect();
    if !featured.is_empty() {
        items.push(TopItem::Rule);
        items.extend(featured.iter().cloned().map(TopItem::Featured));
    }
    items
}

impl TopItem {
    /// Whether this is an item row (anything but `Rule`).
    #[cfg_attr(not(test), expect(dead_code, reason = "used by the search Top tab"))]
    pub fn is_item(&self) -> bool {
        !matches!(self, Self::Rule)
    }

    /// The model inside, `None` for the rule.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by the search Top tab"))]
    fn inner(&self) -> Option<&dyn ListItem> {
        match self {
            Self::Track(i) => Some(i),
            Self::Album(i) => Some(i),
            Self::Artist(i) => Some(i),
            Self::Playlist(i) | Self::Featured(i) => Some(i),
            Self::Show(i) => Some(i),
            Self::Episode(i) => Some(i),
            Self::Rule => None,
        }
    }

    /// The model inside, `None` for the rule.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by the search Top tab"))]
    fn inner_mut(&mut self) -> Option<&mut dyn ListItem> {
        match self {
            Self::Track(i) => Some(i),
            Self::Album(i) => Some(i),
            Self::Artist(i) => Some(i),
            Self::Playlist(i) | Self::Featured(i) => Some(i),
            Self::Show(i) => Some(i),
            Self::Episode(i) => Some(i),
            Self::Rule => None,
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
        matches!(self, Self::Rule).then(|| FEATURED_LABEL.to_string())
    }

    fn display_tag(&self, _library: &Library) -> Option<String> {
        let word = match self {
            Self::Artist(_) => "Artist",
            Self::Track(_) => "Track",
            Self::Album(_) => "Album",
            Self::Playlist(_) => "Playlist",
            Self::Show(_) => "Show",
            Self::Episode(_) => "Episode",
            Self::Featured(_) | Self::Rule => return None,
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
            None => Box::new(Self::Rule),
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
                TopItem::Rule => "rule",
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
            kinds(&top_items(rows(), &featured)),
            ["playlist", "playlist", "rule", "featured", "featured"]
        );
        assert_eq!(kinds(&top_items(rows(), &[])), ["playlist", "playlist"]);
        assert_eq!(
            kinds(&top_items(Vec::new(), &featured)),
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
        assert_eq!(TopItem::Rule.display_tag(&library), None);
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
        let mut rule = TopItem::Rule;
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
}
