use std::sync::Arc;

use cursive::Cursive;
use cursive::traits::{Nameable, Resizable};
use cursive::views::{Checkbox, Dialog, EditView, ListView};

use crate::jukebox::Jukebox;
use crate::jukebox::settings::JukeboxSettings;
use crate::ui::modal::Modal;

fn num_field(value: impl std::fmt::Display) -> impl cursive::View {
    EditView::new().content(value.to_string()).fixed_width(10)
}

/// Build and show the jukebox settings modal, pre-filled from the live settings.
pub fn open_settings_modal(s: &mut Cursive, jukebox: Arc<Jukebox>) {
    let cur = jukebox.settings();

    let mut list = ListView::new();
    list.add_child(
        "Similarity threshold",
        num_field(cur.max_branch_distance).with_name("jb_threshold"),
    );
    list.add_child(
        "Min branch probability",
        num_field(cur.min_branch_probability).with_name("jb_min"),
    );
    list.add_child(
        "Max branch probability",
        num_field(cur.max_branch_probability).with_name("jb_max"),
    );
    list.add_child(
        "Probability ramp",
        num_field(cur.branch_probability_ramp).with_name("jb_ramp"),
    );
    list.add_child(
        "Max play time (s, 0=inf)",
        num_field(cur.max_play_time_secs).with_name("jb_maxtime"),
    );
    list.add_child(
        "Dynamic threshold",
        Checkbox::new().with_checked(cur.dynamic_threshold).with_name("jb_dyn"),
    );
    list.add_child(
        "Only backward branches",
        Checkbox::new().with_checked(cur.only_backward_branches).with_name("jb_back"),
    );
    list.add_child(
        "Only long branches",
        Checkbox::new().with_checked(cur.only_long_branches).with_name("jb_long"),
    );
    list.add_child(
        "Remove sequential branches",
        Checkbox::new().with_checked(cur.remove_sequential_branches).with_name("jb_seq"),
    );
    list.add_child(
        "Add best last branch",
        Checkbox::new().with_checked(cur.add_last_branch).with_name("jb_last"),
    );
    list.add_child(
        "Always follow last branch",
        Checkbox::new().with_checked(cur.always_follow_last_branch).with_name("jb_follow"),
    );

    let apply_jukebox = jukebox.clone();
    let reset_jukebox = jukebox.clone();
    let dialog = Dialog::around(list)
        .title("Jukebox Settings")
        .button("Apply", move |s| {
            let new = collect_settings(s, &apply_jukebox.settings());
            apply_jukebox.apply_settings(new);
            s.pop_layer();
        })
        .button("Reset", move |s| {
            reset_jukebox.apply_settings(JukeboxSettings::default());
            s.pop_layer();
        })
        .dismiss_button("Cancel");

    s.add_layer(Modal::new(dialog));
}

fn read_num<T: std::str::FromStr>(s: &mut Cursive, name: &str, fallback: T) -> T {
    s.call_on_name(name, |v: &mut EditView| v.get_content().parse::<T>().ok())
        .flatten()
        .unwrap_or(fallback)
}

fn read_bool(s: &mut Cursive, name: &str, fallback: bool) -> bool {
    s.call_on_name(name, |v: &mut Checkbox| v.is_checked()).unwrap_or(fallback)
}

fn collect_settings(s: &mut Cursive, cur: &JukeboxSettings) -> JukeboxSettings {
    JukeboxSettings {
        max_branch_distance: read_num(s, "jb_threshold", cur.max_branch_distance),
        dynamic_threshold: read_bool(s, "jb_dyn", cur.dynamic_threshold),
        min_branch_probability: read_num(s, "jb_min", cur.min_branch_probability),
        max_branch_probability: read_num(s, "jb_max", cur.max_branch_probability),
        branch_probability_ramp: read_num(s, "jb_ramp", cur.branch_probability_ramp),
        only_backward_branches: read_bool(s, "jb_back", cur.only_backward_branches),
        only_long_branches: read_bool(s, "jb_long", cur.only_long_branches),
        remove_sequential_branches: read_bool(s, "jb_seq", cur.remove_sequential_branches),
        add_last_branch: read_bool(s, "jb_last", cur.add_last_branch),
        always_follow_last_branch: read_bool(s, "jb_follow", cur.always_follow_last_branch),
        max_play_time_secs: read_num(s, "jb_maxtime", cur.max_play_time_secs),
        anti_loop: cur.anti_loop,
    }
}
