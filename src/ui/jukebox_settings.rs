use std::sync::Arc;

use cursive::Cursive;
use cursive::traits::{Nameable, Resizable, Scrollable};
use cursive::views::{Checkbox, Dialog, EditView, ListView};

use crate::config::Config;
use crate::jukebox::Jukebox;
use crate::jukebox::settings::{
    AntiLoopSettings, JukeboxSettings, LoopCountMode, LoopCounter, LoopIdentity, LoopSkipAction,
};
use crate::ui::modal::Modal;

fn num_field(value: impl std::fmt::Display) -> impl cursive::View {
    EditView::new().content(value.to_string()).fixed_width(10)
}

fn text_field(value: &str) -> impl cursive::View {
    EditView::new().content(value).fixed_width(28)
}

/// Build and show the jukebox settings modal, pre-filled from the live settings. Apply
/// updates the live settings and persists them; Reset clears the persisted override and
/// restores the `config.toml` baseline.
pub fn open_settings_modal(s: &mut Cursive, jukebox: Arc<Jukebox>, cfg: Arc<Config>) {
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

    list.add_child("─ Anti-loop ─", cursive::views::DummyView);
    list.add_child(
        "Break loops",
        Checkbox::new().with_checked(cur.anti_loop.enabled).with_name("jb_break_loops"),
    );
    list.add_child(
        "Loop threshold",
        num_field(cur.anti_loop.threshold).with_name("jb_loop_threshold"),
    );
    list.add_child(
        "Identity (edge/destination/distance)",
        text_field(cur.anti_loop.identity.as_str()).with_name("jb_loop_identity"),
    );
    list.add_child(
        "Count (consecutive/cumulative)",
        text_field(cur.anti_loop.count_mode.as_str()).with_name("jb_loop_count"),
    );
    list.add_child(
        "Skip (different_else_continue/continue/different_only)",
        text_field(cur.anti_loop.skip_action.as_str()).with_name("jb_loop_skip"),
    );
    list.add_child(
        "Break last branch",
        Checkbox::new().with_checked(cur.anti_loop.break_last_branch).with_name("jb_break_last"),
    );
    list.add_child(
        "Counter (reset/retire)",
        text_field(cur.anti_loop.counter.as_str()).with_name("jb_loop_counter"),
    );

    let apply_jukebox = jukebox.clone();
    let apply_cfg = cfg.clone();
    let reset_jukebox = jukebox.clone();
    let reset_cfg = cfg.clone();
    let dialog = Dialog::around(list.scrollable())
        .title("Jukebox Settings")
        .button("Apply", move |s| {
            let new = collect_settings(s, &apply_jukebox.settings());
            apply_jukebox.apply_settings(new.clone());
            persist(&apply_cfg, new);
            s.pop_layer();
        })
        .button("Reset", move |s| {
            // Forget the persisted override and restore the config.toml baseline.
            let base = JukeboxSettings::from_config(
                &reset_cfg.values().jukebox.clone().unwrap_or_default(),
            );
            reset_cfg.with_state_mut(|st| st.jukebox = None);
            reset_cfg.save_state();
            reset_jukebox.apply_settings(base);
            s.pop_layer();
        })
        .dismiss_button("Cancel");

    s.add_layer(Modal::new(dialog));
}

fn persist(cfg: &Config, settings: JukeboxSettings) {
    cfg.with_state_mut(|st| st.jukebox = Some(settings.clone()));
    cfg.save_state();
}

fn read_num<T: std::str::FromStr>(s: &mut Cursive, name: &str, fallback: T) -> T {
    s.call_on_name(name, |v: &mut EditView| v.get_content().parse::<T>().ok())
        .flatten()
        .unwrap_or(fallback)
}

fn read_bool(s: &mut Cursive, name: &str, fallback: bool) -> bool {
    s.call_on_name(name, |v: &mut Checkbox| v.is_checked()).unwrap_or(fallback)
}

fn read_str(s: &mut Cursive, name: &str) -> Option<String> {
    s.call_on_name(name, |v: &mut EditView| v.get_content().to_string())
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
        anti_loop: AntiLoopSettings {
            enabled: read_bool(s, "jb_break_loops", cur.anti_loop.enabled),
            threshold: read_num::<usize>(s, "jb_loop_threshold", cur.anti_loop.threshold).max(1),
            identity: read_str(s, "jb_loop_identity")
                .map(|t| LoopIdentity::parse(&t))
                .unwrap_or(cur.anti_loop.identity),
            count_mode: read_str(s, "jb_loop_count")
                .map(|t| LoopCountMode::parse(&t))
                .unwrap_or(cur.anti_loop.count_mode),
            skip_action: read_str(s, "jb_loop_skip")
                .map(|t| LoopSkipAction::parse(&t))
                .unwrap_or(cur.anti_loop.skip_action),
            break_last_branch: read_bool(s, "jb_break_last", cur.anti_loop.break_last_branch),
            counter: read_str(s, "jb_loop_counter")
                .map(|t| LoopCounter::parse(&t))
                .unwrap_or(cur.anti_loop.counter),
        },
    }
}
