use crate::config::JukeboxConfig;

#[derive(Debug, Clone, PartialEq)]
pub struct JukeboxSettings {
    pub max_branch_distance: u32,
    pub dynamic_threshold: bool,
    pub min_branch_probability: f64,
    pub max_branch_probability: f64,
    pub branch_probability_ramp: f64,
    pub only_backward_branches: bool,
    pub only_long_branches: bool,
    pub remove_sequential_branches: bool,
    pub add_last_branch: bool,
    pub always_follow_last_branch: bool,
    pub max_play_time_secs: u64,
}

impl JukeboxSettings {
    pub const MIN_BEATS_BEFORE_BRANCHING: usize = 5;
    pub const MAX_BRANCHES: usize = 4;
    pub const RANGE_MAX_BRANCH_DISTANCE: f64 = 80.0;

    pub fn from_config(c: &JukeboxConfig) -> Self {
        let d = Self::default();
        Self {
            max_branch_distance: c.branch_similarity_threshold.unwrap_or(d.max_branch_distance),
            dynamic_threshold: c.dynamic_threshold.unwrap_or(d.dynamic_threshold),
            min_branch_probability: c.min_branch_probability.unwrap_or(d.min_branch_probability),
            max_branch_probability: c.max_branch_probability.unwrap_or(d.max_branch_probability),
            branch_probability_ramp: c.branch_probability_ramp.unwrap_or(d.branch_probability_ramp),
            only_backward_branches: c.only_backward_branches.unwrap_or(d.only_backward_branches),
            only_long_branches: c.only_long_branches.unwrap_or(d.only_long_branches),
            remove_sequential_branches: c
                .remove_sequential_branches
                .unwrap_or(d.remove_sequential_branches),
            add_last_branch: c.add_last_branch.unwrap_or(d.add_last_branch),
            always_follow_last_branch: c
                .always_follow_last_branch
                .unwrap_or(d.always_follow_last_branch),
            max_play_time_secs: c.max_play_time_secs.unwrap_or(d.max_play_time_secs),
        }
    }
}

impl Default for JukeboxSettings {
    fn default() -> Self {
        Self {
            max_branch_distance: 30,
            dynamic_threshold: false,
            min_branch_probability: 0.18,
            max_branch_probability: 0.5,
            branch_probability_ramp: 0.018,
            only_backward_branches: false,
            only_long_branches: false,
            remove_sequential_branches: false,
            add_last_branch: true,
            always_follow_last_branch: true,
            max_play_time_secs: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_yields_spicetify_defaults() {
        let s = JukeboxSettings::from_config(&JukeboxConfig::default());
        assert_eq!(s, JukeboxSettings::default());
        assert_eq!(s.max_branch_distance, 30);
        assert_eq!(s.min_branch_probability, 0.18);
        assert!(s.always_follow_last_branch);
    }

    #[test]
    fn config_overrides_apply() {
        let c = JukeboxConfig { branch_similarity_threshold: Some(50), ..Default::default() };
        let s = JukeboxSettings::from_config(&c);
        assert_eq!(s.max_branch_distance, 50);
        assert_eq!(s.min_branch_probability, 0.18); // untouched
    }
}
