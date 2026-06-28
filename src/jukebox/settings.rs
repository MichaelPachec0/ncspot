use crate::config::JukeboxConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopIdentity {
    Edge,
    Destination,
    Distance,
}
impl LoopIdentity {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "destination" => Self::Destination,
            "distance" => Self::Distance,
            "edge" => Self::Edge,
            other => {
                log::debug!("unknown loop_identity '{other}', using edge");
                Self::Edge
            }
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Destination => "destination",
            Self::Distance => "distance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopCountMode {
    Consecutive,
    Cumulative,
}
impl LoopCountMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "cumulative" => Self::Cumulative,
            "consecutive" => Self::Consecutive,
            other => {
                log::debug!("unknown loop_count_mode '{other}', using consecutive");
                Self::Consecutive
            }
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Consecutive => "consecutive",
            Self::Cumulative => "cumulative",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopSkipAction {
    DifferentElseContinue,
    Continue,
    DifferentOnly,
}
impl LoopSkipAction {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "continue" => Self::Continue,
            "different_only" => Self::DifferentOnly,
            "different_else_continue" => Self::DifferentElseContinue,
            other => {
                log::debug!("unknown loop_skip_action '{other}', using different_else_continue");
                Self::DifferentElseContinue
            }
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DifferentElseContinue => "different_else_continue",
            Self::Continue => "continue",
            Self::DifferentOnly => "different_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopCounter {
    Reset,
    Retire,
}
impl LoopCounter {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "retire" => Self::Retire,
            "reset" => Self::Reset,
            other => {
                log::debug!("unknown loop_counter '{other}', using reset");
                Self::Reset
            }
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::Retire => "retire",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AntiLoopSettings {
    pub enabled: bool,
    pub threshold: usize,
    pub identity: LoopIdentity,
    pub count_mode: LoopCountMode,
    pub skip_action: LoopSkipAction,
    pub break_last_branch: bool,
    pub counter: LoopCounter,
}

impl Default for AntiLoopSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: 4,
            identity: LoopIdentity::Edge,
            count_mode: LoopCountMode::Consecutive,
            skip_action: LoopSkipAction::DifferentElseContinue,
            break_last_branch: false,
            counter: LoopCounter::Reset,
        }
    }
}

impl AntiLoopSettings {
    pub fn from_config(c: &JukeboxConfig) -> Self {
        let d = Self::default();
        Self {
            enabled: c.break_loops.unwrap_or(d.enabled),
            threshold: c.loop_threshold.unwrap_or(d.threshold).max(1),
            identity: c
                .loop_identity
                .as_deref()
                .map(LoopIdentity::parse)
                .unwrap_or(d.identity),
            count_mode: c
                .loop_count_mode
                .as_deref()
                .map(LoopCountMode::parse)
                .unwrap_or(d.count_mode),
            skip_action: c
                .loop_skip_action
                .as_deref()
                .map(LoopSkipAction::parse)
                .unwrap_or(d.skip_action),
            break_last_branch: c.break_last_branch.unwrap_or(d.break_last_branch),
            counter: c
                .loop_counter
                .as_deref()
                .map(LoopCounter::parse)
                .unwrap_or(d.counter),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub anti_loop: AntiLoopSettings,
}

impl JukeboxSettings {
    pub const MIN_BEATS_BEFORE_BRANCHING: usize = 5;
    pub const MAX_BRANCHES: usize = 4;
    pub const RANGE_MAX_BRANCH_DISTANCE: f64 = 80.0;

    pub fn from_config(c: &JukeboxConfig) -> Self {
        let d = Self::default();
        Self {
            max_branch_distance: c
                .branch_similarity_threshold
                .unwrap_or(d.max_branch_distance),
            dynamic_threshold: c.dynamic_threshold.unwrap_or(d.dynamic_threshold),
            min_branch_probability: c.min_branch_probability.unwrap_or(d.min_branch_probability),
            max_branch_probability: c.max_branch_probability.unwrap_or(d.max_branch_probability),
            branch_probability_ramp: c
                .branch_probability_ramp
                .unwrap_or(d.branch_probability_ramp),
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
            anti_loop: AntiLoopSettings::from_config(c),
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
            anti_loop: AntiLoopSettings::default(),
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
        let c = JukeboxConfig {
            branch_similarity_threshold: Some(50),
            ..Default::default()
        };
        let s = JukeboxSettings::from_config(&c);
        assert_eq!(s.max_branch_distance, 50);
        assert_eq!(s.min_branch_probability, 0.18); // untouched
    }

    #[test]
    fn anti_loop_defaults_off() {
        let s = JukeboxSettings::from_config(&JukeboxConfig::default());
        assert!(!s.anti_loop.enabled);
        assert_eq!(s.anti_loop.threshold, 4);
        assert_eq!(s.anti_loop.identity, LoopIdentity::Edge);
        assert_eq!(s.anti_loop.count_mode, LoopCountMode::Consecutive);
        assert_eq!(
            s.anti_loop.skip_action,
            LoopSkipAction::DifferentElseContinue
        );
        assert!(!s.anti_loop.break_last_branch);
        assert_eq!(s.anti_loop.counter, LoopCounter::Reset);
    }

    #[test]
    fn anti_loop_parses_config() {
        let c = JukeboxConfig {
            break_loops: Some(true),
            loop_threshold: Some(2),
            loop_identity: Some("distance".into()),
            loop_count_mode: Some("cumulative".into()),
            loop_skip_action: Some("continue".into()),
            break_last_branch: Some(true),
            loop_counter: Some("retire".into()),
            ..Default::default()
        };
        let s = JukeboxSettings::from_config(&c);
        assert!(s.anti_loop.enabled);
        assert_eq!(s.anti_loop.threshold, 2);
        assert_eq!(s.anti_loop.identity, LoopIdentity::Distance);
        assert_eq!(s.anti_loop.count_mode, LoopCountMode::Cumulative);
        assert_eq!(s.anti_loop.skip_action, LoopSkipAction::Continue);
        assert!(s.anti_loop.break_last_branch);
        assert_eq!(s.anti_loop.counter, LoopCounter::Retire);
    }

    #[test]
    fn anti_loop_unknown_strings_and_zero_threshold_fall_back() {
        let c = JukeboxConfig {
            loop_identity: Some("bogus".into()),
            loop_threshold: Some(0),
            ..Default::default()
        };
        let s = JukeboxSettings::from_config(&c);
        assert_eq!(s.anti_loop.identity, LoopIdentity::Edge);
        assert_eq!(s.anti_loop.threshold, 1); // clamped to >= 1
    }
}
