use crate::jukebox::graph::{Edge, SongGraph};
use crate::jukebox::settings::JukeboxSettings;

pub trait RandomSource: Send {
    fn random_unit(&mut self) -> f64;
}

pub trait Clock: Send {
    fn now_ms(&self) -> u64;
}

/// Dependency-free PRNG (xorshift64) so we need no `rand` dependency.
pub struct XorShiftRandom {
    state: u64,
}
impl XorShiftRandom {
    pub fn new(seed: u64) -> Self {
        Self { state: seed | 1 }
    }
}
impl RandomSource for XorShiftRandom {
    fn random_unit(&mut self) -> f64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[derive(Debug, PartialEq)]
pub enum DriverAction {
    /// Still inside the current beat, or advanced to the sequential next beat: do nothing.
    None,
    /// A branch was taken: seek the player to `seek_ms`.
    Jump { seek_ms: f64 },
    /// Reached the end of the song with no branch: stop the driver.
    Stop,
}

pub struct Driver {
    graph: SongGraph,
    settings: JukeboxSettings,
    rng: Box<dyn RandomSource>,
    clock: Box<dyn Clock>,
    current_beat: Option<usize>,
    bouncing: bool,
    bounce_seed: Option<usize>,
    bounce_count: u64,
    last_branch: Option<Edge>,
    beats_since_last_branch: usize,
    current_branch_chance: f64,
    beats_played: u64,
    jumps: u64,
    start_ms: u64,
    neighbour_cursor: Vec<usize>,
}

impl Driver {
    pub fn new(
        graph: SongGraph,
        settings: JukeboxSettings,
        rng: Box<dyn RandomSource>,
        clock: Box<dyn Clock>,
    ) -> Self {
        let n = graph.beats.len();
        let start_ms = clock.now_ms();
        Self {
            current_branch_chance: settings.min_branch_probability,
            graph,
            settings,
            rng,
            clock,
            current_beat: None,
            bouncing: false,
            bounce_seed: None,
            bounce_count: 0,
            last_branch: None,
            beats_since_last_branch: 0,
            beats_played: 0,
            jumps: 0,
            start_ms,
            neighbour_cursor: vec![0; n],
        }
    }

    pub fn current_beat(&self) -> Option<usize> {
        self.current_beat
    }
    pub fn beats_played(&self) -> u64 {
        self.beats_played
    }
    pub fn jumps(&self) -> u64 {
        self.jumps
    }
    pub fn branch_chance(&self) -> f64 {
        self.current_branch_chance
    }
    pub fn last_branch(&self) -> Option<Edge> {
        self.last_branch
    }
    pub fn graph(&self) -> &SongGraph {
        &self.graph
    }
    pub fn set_bouncing(&mut self, bouncing: bool) {
        self.bouncing = bouncing;
    }

    pub fn process(&mut self, progress_ms: f64) -> DriverAction {
        if let Some(idx) = self.current_beat {
            let b = &self.graph.beats[idx];
            if progress_ms >= b.start_ms && progress_ms <= b.end_ms() {
                return DriverAction::None;
            }
        }

        self.beats_since_last_branch += 1;

        let out_of_sync = self.is_out_of_sync(progress_ms);
        let last_beat = self.current_beat;
        let Some(next_idx) = self.get_next_beat(progress_ms, out_of_sync) else {
            return DriverAction::Stop;
        };

        let action = self.play_beat(last_beat, next_idx, progress_ms, out_of_sync);
        self.current_beat = Some(next_idx);
        self.beats_played += 1;
        action
    }

    fn is_out_of_sync(&self, progress_ms: f64) -> bool {
        let Some(idx) = self.current_beat else {
            return false;
        };
        let next_end = self.graph.beats.get(idx + 1).map(|b| b.end_ms());
        let prev_start = if idx > 0 {
            Some(self.graph.beats[idx - 1].start_ms)
        } else {
            None
        };
        next_end.is_some_and(|e| progress_ms > e) || prev_start.is_some_and(|s| progress_ms < s)
    }

    fn play_beat(
        &self,
        last_beat: Option<usize>,
        current: usize,
        progress_ms: f64,
        out_of_sync: bool,
    ) -> DriverAction {
        let Some(last) = last_beat else {
            return DriverAction::None;
        };
        if last + 1 == current || out_of_sync {
            return DriverAction::None;
        }
        let offset = progress_ms - self.graph.beats[last].end_ms();
        let seek_ms = self.graph.beats[current].start_ms + offset;
        DriverAction::Jump { seek_ms }
    }

    fn get_next_beat(&mut self, progress_ms: f64, out_of_sync: bool) -> Option<usize> {
        if self.current_beat.is_none() || out_of_sync {
            for b in &self.graph.beats {
                if progress_ms >= b.start_ms && progress_ms <= b.end_ms() {
                    return Some(b.index);
                }
            }
            return self.graph.beats.first().map(|b| b.index);
        }

        if self.bouncing {
            if self.bounce_seed.is_none() {
                self.bounce_seed = self.current_beat;
                self.bounce_count = 0;
            }
            let seed = self.bounce_seed.unwrap();
            let pick = if self.bounce_count % 2 == 1 {
                self.select_next_neighbor(seed)
            } else {
                seed
            };
            self.bounce_count += 1;
            return Some(pick);
        }

        if let Some(seed) = self.bounce_seed.take() {
            return Some(seed);
        }

        let next_index = self.current_beat.unwrap() + 1;
        if next_index >= self.graph.beats.len() {
            return None;
        }
        Some(self.select_random_next_beat(next_index))
    }

    fn rotate_neighbour(&mut self, beat_index: usize) -> Edge {
        let len = self.graph.beats[beat_index].neighbours.len();
        let cur = self.neighbour_cursor[beat_index];
        let edge = self.graph.beats[beat_index].neighbours[cur % len];
        self.neighbour_cursor[beat_index] = (cur + 1) % len;
        edge
    }

    fn select_random_next_beat(&mut self, next_index: usize) -> usize {
        if self.graph.beats[next_index].neighbours.is_empty() {
            return next_index;
        }
        if self.should_random_branch(next_index) {
            let edge = self.rotate_neighbour(next_index);
            self.beats_since_last_branch = 0;
            self.last_branch = Some(edge);
            self.jumps += 1;
            return edge.destination;
        }
        next_index
    }

    fn select_next_neighbor(&mut self, beat_index: usize) -> usize {
        if self.graph.beats[beat_index].neighbours.is_empty() {
            return beat_index;
        }
        let edge = self.rotate_neighbour(beat_index);
        self.last_branch = Some(edge);
        edge.destination
    }

    fn should_random_branch(&mut self, beat_index: usize) -> bool {
        let current_play_time = self.clock.now_ms().saturating_sub(self.start_ms);
        let max = self.settings.max_play_time_secs.saturating_mul(1000);

        if max > 0 && current_play_time > max {
            self.current_branch_chance = 0.0;
            return false;
        }

        if beat_index == self.graph.last_branch_point
            && self.settings.always_follow_last_branch
            && (max == 0 || current_play_time <= max)
        {
            return true;
        }

        if self.beats_since_last_branch <= JukeboxSettings::MIN_BEATS_BEFORE_BRANCHING {
            return false;
        }

        self.current_branch_chance = (self.current_branch_chance
            + self.settings.branch_probability_ramp)
            .min(self.settings.max_branch_probability);

        let should = self.rng.random_unit() < self.current_branch_chance;
        if should {
            self.current_branch_chance = self.settings.min_branch_probability;
        }
        should
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jukebox::graph::{Beat, Edge, SongGraph};

    struct SeqRandom {
        vals: Vec<f64>,
        i: usize,
    }
    impl RandomSource for SeqRandom {
        fn random_unit(&mut self) -> f64 {
            let v = self.vals[self.i % self.vals.len()];
            self.i += 1;
            v
        }
    }
    struct FakeClock {
        ms: u64,
    }
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.ms
        }
    }

    // 10 beats, 1000ms each. beat 5 has a backward neighbour to beat 1.
    fn graph_with_branch_at(last_branch_point: usize) -> SongGraph {
        let mut beats: Vec<Beat> = (0..10)
            .map(|i| Beat {
                index: i,
                start_ms: i as f64 * 1000.0,
                duration_ms: 1000.0,
                neighbours: vec![],
            })
            .collect();
        beats[5].neighbours = vec![Edge { source: 5, destination: 1, distance: 10.0 }];
        SongGraph { beats, last_branch_point, longest_reach: 0.0 }
    }

    fn driver(rng: Vec<f64>, last_branch_point: usize) -> Driver {
        Driver::new(
            graph_with_branch_at(last_branch_point),
            JukeboxSettings::default(),
            Box::new(SeqRandom { vals: rng, i: 0 }),
            Box::new(FakeClock { ms: 0 }),
        )
    }

    #[test]
    fn min_beats_gate_blocks_early_branching() {
        let mut d = driver(vec![0.0], 99); // rng would always branch, but gate blocks
        // beats_since_last_branch starts at 0, <= 5 → no branch
        assert!(!d.should_random_branch(5));
    }

    #[test]
    fn probability_ramps_until_branch() {
        let mut d = driver(vec![1.0, 1.0, 1.0], 99); // rng never < chance → never branches
        d.beats_since_last_branch = 100; // past the gate
        d.current_branch_chance = 0.18;
        d.should_random_branch(0);
        assert!((d.current_branch_chance - (0.18 + 0.018)).abs() < 1e-9);
        d.should_random_branch(0);
        assert!((d.current_branch_chance - (0.18 + 0.036)).abs() < 1e-9);
    }

    #[test]
    fn branching_resets_chance_to_min() {
        let mut d = driver(vec![0.0], 99); // rng 0.0 < chance → branches
        d.beats_since_last_branch = 100;
        d.current_branch_chance = 0.4;
        assert!(d.should_random_branch(0));
        assert!((d.current_branch_chance - 0.18).abs() < 1e-9);
    }

    #[test]
    fn forced_branch_at_last_branch_point() {
        let mut d = driver(vec![1.0], 5); // last_branch_point = 5, always_follow default true
        d.beats_since_last_branch = 0; // even within the gate, the forced branch wins
        assert!(d.should_random_branch(5));
    }

    #[test]
    fn first_process_locks_onto_containing_beat() {
        let mut d = driver(vec![1.0], 99);
        let action = d.process(3200.0); // inside beat 3 [3000,4000]
        assert_eq!(action, DriverAction::None);
        assert_eq!(d.current_beat(), Some(3));
    }

    #[test]
    fn sequential_advance_does_not_seek() {
        let mut d = driver(vec![1.0], 99); // rng never branches
        d.process(500.0); // lock onto beat 0
        let action = d.process(1500.0); // now inside beat 1
        assert_eq!(action, DriverAction::None);
        assert_eq!(d.current_beat(), Some(1));
        assert_eq!(d.jumps(), 0);
    }

    #[test]
    fn reaching_end_stops() {
        let mut d = driver(vec![1.0], 99);
        d.process(9500.0); // beat 9 (last)
        let action = d.process(10500.0); // past the end
        assert_eq!(action, DriverAction::Stop);
    }
}
