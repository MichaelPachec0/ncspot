use std::sync::Arc;

use cursive::Cursive;
use cursive::traits::{Nameable, Scrollable};
use cursive::views::{Checkbox, Dialog};

use crate::config::Config;
use crate::jukebox::Jukebox;
use crate::jukebox::settings::PartialJukeboxSettings;
use crate::ui::jukebox_settings::{build_dial_list, collect_settings};
use crate::ui::modal::Modal;

/// Open the per-song settings menu for `track_id`. Pre-filled from the saved override
/// resolved over the global baseline, or the live global settings when none is saved.
/// Saving writes a snapshot (or a diff when the checkbox is set) into the overrides map.
pub fn open(
    s: &mut Cursive,
    jukebox: Arc<Jukebox>,
    cfg: Arc<Config>,
    track_id: Option<String>,
    track_title: String,
) {
    let Some(id) = track_id else {
        let dialog = Dialog::text("Per-song settings require a Spotify track.")
            .title("Per-Song Settings")
            .dismiss_button("Close");
        s.add_layer(Modal::new(dialog));
        return;
    };

    let base = jukebox.settings();
    let prefilled = cfg
        .state()
        .jukebox_overrides
        .get(&id)
        .map(|p| p.resolve(&base))
        .unwrap_or_else(|| base.clone());

    let mut list = build_dial_list(&prefilled);
    list.add_child("─ Save mode ─", cursive::views::DummyView);
    list.add_child(
        "Save only changed dials (diff)",
        Checkbox::new().with_name("jb_ps_diff"),
    );

    let save_jukebox = jukebox.clone();
    let save_cfg = cfg.clone();
    let save_base = base.clone();
    let save_id = id.clone();
    let clear_jukebox = jukebox.clone();
    let clear_cfg = cfg.clone();
    let clear_id = id.clone();

    let dialog = Dialog::around(list.scrollable())
        .title(format!("Per-Song Settings — {track_title}"))
        .button("Save for this song", move |s| {
            let cur = collect_settings(s, &save_base);
            let diff_only = s
                .call_on_name("jb_ps_diff", |c: &mut Checkbox| c.is_checked())
                .unwrap_or(false);
            let partial = if diff_only {
                PartialJukeboxSettings::diff(&cur, &save_base)
            } else {
                PartialJukeboxSettings::snapshot(&cur)
            };
            let id = save_id.clone();
            save_cfg.with_state_mut(move |st| {
                st.jukebox_overrides.insert(id.clone(), partial.clone());
            });
            save_cfg.save_state();
            save_jukebox.request_rebuild();
            s.pop_layer();
        })
        .button("Clear for this song", move |s| {
            let id = clear_id.clone();
            clear_cfg.with_state_mut(move |st| {
                st.jukebox_overrides.remove(&id);
            });
            clear_cfg.save_state();
            clear_jukebox.request_rebuild();
            s.pop_layer();
        })
        .dismiss_button("Cancel");

    s.add_layer(Modal::new(dialog));
}
