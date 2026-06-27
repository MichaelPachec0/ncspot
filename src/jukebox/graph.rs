use crate::jukebox::model::Segment;
use crate::jukebox::remixer::RemixedAnalysis;
use crate::jukebox::settings::JukeboxSettings;

#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub source: usize,
    pub destination: usize,
    pub distance: f64,
}

#[derive(Debug, Clone)]
pub struct Beat {
    pub index: usize,
    pub start_ms: f64,
    pub duration_ms: f64,
    pub neighbours: Vec<Edge>,
}

impl Beat {
    pub fn end_ms(&self) -> f64 {
        self.start_ms + self.duration_ms
    }
}

#[derive(Debug, Clone, Default)]
pub struct SongGraph {
    pub beats: Vec<Beat>,
    pub last_branch_point: usize,
    pub longest_reach: f64,
}

fn euclidean(a: &[f64], b: &[f64]) -> f64 {
    let mut sum = 0.0;
    for (i, &av) in a.iter().enumerate() {
        let d = b.get(i).copied().unwrap_or(0.0) - av;
        sum += d * d;
    }
    sum.sqrt()
}

/// Port of GraphGenerator.getSegmentsDistance (weights: timbre 1, pitch 10,
/// loudness_start 1, loudness_max 1, duration 100, confidence 1).
pub fn segments_distance(s1: &Segment, s2: &Segment) -> f64 {
    let timbre = euclidean(&s1.timbre, &s2.timbre);
    let pitch = euclidean(&s1.pitches, &s2.pitches);
    let sloud_start = (s1.loudness_start - s2.loudness_start).abs();
    let sloud_max = (s1.loudness_max - s2.loudness_max).abs();
    let duration = (s1.duration - s2.duration).abs();
    let confidence = (s1.confidence - s2.confidence).abs();
    timbre + pitch * 10.0 + sloud_start + sloud_max + duration * 100.0 + confidence
}

struct GraphGenerator<'a> {
    settings: &'a JukeboxSettings,
    analysis: &'a RemixedAnalysis,
    all_neighbours: Vec<Vec<Edge>>,
    min_long_branch: f64,
    computed_max_branch_distance: f64,
}

pub fn generate(settings: &JukeboxSettings, analysis: &RemixedAnalysis) -> SongGraph {
    let n = analysis.beats.len();
    let mut generator = GraphGenerator {
        settings,
        analysis,
        all_neighbours: vec![Vec::new(); n],
        min_long_branch: n as f64 / 5.0,
        computed_max_branch_distance: 0.0,
    };
    generator.run()
}

impl GraphGenerator<'_> {
    fn run(&mut self) -> SongGraph {
        let mut graph = SongGraph {
            beats: self
                .analysis
                .beats
                .iter()
                .map(|b| Beat {
                    index: b.index,
                    start_ms: b.start * 1000.0,
                    duration_ms: b.duration * 1000.0,
                    neighbours: Vec::new(),
                })
                .collect(),
            last_branch_point: 0,
            longest_reach: 0.0,
        };

        self.precalculate_nearest_neighbors();

        if self.settings.dynamic_threshold {
            self.computed_max_branch_distance = self.dynamic_collect(&mut graph);
        } else {
            self.collect(&mut graph, self.settings.max_branch_distance as f64);
            self.computed_max_branch_distance = self.settings.max_branch_distance as f64;
        }

        self.post_process(&mut graph);
        graph
    }

    fn precalculate_nearest_neighbors(&mut self) {
        for i in 0..self.analysis.beats.len() {
            self.calculate_for_beat(i);
        }
    }

    fn calculate_for_beat(&mut self, beat_index: usize) {
        let current = &self.analysis.beats[beat_index];
        if current.overlapping_segments.is_empty() {
            return; // avoid divide-by-zero; such beats get no branches
        }
        let segments = &self.analysis.segments;
        let mut edges: Vec<Edge> = Vec::new();

        for (other_index, other) in self.analysis.beats.iter().enumerate() {
            if other_index == beat_index {
                continue;
            }
            let mut sum = 0.0;
            for (seg_pos, &seg_id) in current.overlapping_segments.iter().enumerate() {
                let mut distance = 100.0;
                if seg_pos < other.overlapping_segments.len() {
                    let other_seg_id = other.overlapping_segments[seg_pos];
                    if seg_id != other_seg_id {
                        distance = segments_distance(&segments[seg_id], &segments[other_seg_id]);
                    }
                }
                sum += distance;
            }
            let parent_distance =
                if current.index_in_parent == other.index_in_parent { 0.0 } else { 100.0 };
            let total = sum / current.overlapping_segments.len() as f64 + parent_distance;
            if total < JukeboxSettings::RANGE_MAX_BRANCH_DISTANCE {
                edges.push(Edge { source: beat_index, destination: other_index, distance: total });
            }
        }

        edges.sort_by(|a, b| {
            a.distance.partial_cmp(&b.distance).unwrap_or(std::cmp::Ordering::Equal)
        });
        for e in edges.into_iter().take(JukeboxSettings::MAX_BRANCHES) {
            self.all_neighbours[beat_index].push(e);
        }
    }

    fn collect(&self, graph: &mut SongGraph, max_branch_distance: f64) -> usize {
        let mut branching = 0;
        for i in 0..graph.beats.len() {
            let filtered = self.filter_neighbors(i, max_branch_distance);
            if !filtered.is_empty() {
                branching += 1;
            }
            graph.beats[i].neighbours = filtered;
        }
        branching
    }

    fn filter_neighbors(&self, beat_index: usize, max_branch_distance: f64) -> Vec<Edge> {
        self.all_neighbours[beat_index]
            .iter()
            .copied()
            .filter(|n| {
                if self.settings.only_backward_branches && n.destination > beat_index {
                    return false;
                }
                if self.settings.only_long_branches
                    && (n.destination as f64 - beat_index as f64).abs() < self.min_long_branch
                {
                    return false;
                }
                n.distance <= max_branch_distance
            })
            .collect()
    }

    fn dynamic_collect(&self, graph: &mut SongGraph) -> f64 {
        let target = self.analysis.beats.len() as f64 / 6.0;
        let mut threshold = 10.0;
        while threshold < JukeboxSettings::RANGE_MAX_BRANCH_DISTANCE {
            let count = self.collect(graph, threshold);
            if count as f64 >= target {
                break;
            }
            threshold += 5.0;
        }
        threshold
    }

    fn post_process(&self, graph: &mut SongGraph) {
        if self.settings.add_last_branch {
            let lbb = self.longest_backward_branch(graph);
            let max_distance = if lbb < 50.0 { 65.0 } else { 55.0 };
            self.insert_best_backward_branch(graph, self.computed_max_branch_distance, max_distance);
        }
        graph.last_branch_point = self.find_best_last_beat(graph);
        self.filter_out_bad_branches(graph);
        if self.settings.remove_sequential_branches {
            self.filter_out_sequential_branches(graph);
        }
    }

    fn longest_backward_branch(&self, graph: &SongGraph) -> f64 {
        let mut longest = 0i64;
        for beat in &graph.beats {
            for n in &beat.neighbours {
                let delta = beat.index as i64 - n.destination as i64;
                if delta > longest {
                    longest = delta;
                }
            }
        }
        (longest as f64 * 100.0) / self.analysis.beats.len() as f64
    }

    fn insert_best_backward_branch(
        &self,
        graph: &mut SongGraph,
        threshold: f64,
        max_branch_distance: f64,
    ) {
        let n_beats = graph.beats.len() as f64;
        let mut best: Option<(f64, usize, Edge)> = None;
        for beat_index in 0..graph.beats.len() {
            for n in &self.all_neighbours[beat_index] {
                let delta = beat_index as i64 - n.destination as i64;
                if delta > 0 && n.distance < max_branch_distance {
                    let percent = (delta as f64 * 100.0) / n_beats;
                    if best.is_none_or(|(p, _, _)| percent > p) {
                        best = Some((percent, beat_index, *n));
                    }
                }
            }
        }
        if let Some((_, beat_index, edge)) = best
            && edge.distance > threshold
        {
            graph.beats[beat_index].neighbours.push(edge);
        }
    }

    fn calculate_reachability(&self, graph: &SongGraph) -> Vec<i64> {
        let n = graph.beats.len();
        let max_iter = 1000;
        let mut reaches: Vec<i64> = (0..n).map(|i| (n - i) as i64).collect();
        for _ in 0..max_iter {
            let mut change_count = 0;
            for beat_index in 0..n {
                let mut changed = false;
                for neighbor in &graph.beats[beat_index].neighbours {
                    let nr = reaches[neighbor.destination];
                    if nr > reaches[beat_index] {
                        reaches[beat_index] = nr;
                        changed = true;
                    }
                }
                if beat_index < n - 1 {
                    let next_reach = reaches[beat_index + 1];
                    if next_reach > reaches[beat_index] {
                        reaches[beat_index] = next_reach;
                        changed = true;
                    }
                }
                if changed {
                    change_count += 1;
                    for i in 0..beat_index {
                        if reaches[i] < reaches[beat_index] {
                            reaches[i] = reaches[beat_index];
                        }
                    }
                }
            }
            if change_count == 0 {
                break;
            }
        }
        reaches
    }

    fn find_best_last_beat(&self, graph: &mut SongGraph) -> usize {
        let reaches = self.calculate_reachability(graph);
        let n = graph.beats.len();
        let reach_threshold = 50.0;
        let mut longest = 0usize;
        let mut longest_reach = 0.0f64;
        for idx in (0..n).rev() {
            let distance_to_end = (n - idx) as i64;
            let reach =
                ((reaches[idx] - distance_to_end) as f64 * 100.0) / self.analysis.beats.len() as f64;
            if reach > longest_reach && !graph.beats[idx].neighbours.is_empty() {
                longest_reach = reach;
                longest = idx;
                if reach >= reach_threshold {
                    break;
                }
            }
        }
        graph.longest_reach = longest_reach;
        longest
    }

    fn filter_out_bad_branches(&self, graph: &mut SongGraph) {
        let last_index = graph.last_branch_point;
        for i in 0..last_index {
            graph.beats[i].neighbours.retain(|n| n.destination < last_index);
        }
    }

    fn filter_out_sequential_branches(&self, graph: &mut SongGraph) {
        for i in (1..graph.beats.len()).rev() {
            let kept: Vec<Edge> = graph.beats[i]
                .neighbours
                .iter()
                .copied()
                .filter(|n| !self.has_sequential_branch(graph, i, *n))
                .collect();
            graph.beats[i].neighbours = kept;
        }
    }

    fn has_sequential_branch(&self, graph: &SongGraph, beat_index: usize, branch: Edge) -> bool {
        if beat_index == graph.last_branch_point || beat_index == 0 {
            return false;
        }
        let previous = beat_index - 1;
        let branch_distance = beat_index as i64 - branch.destination as i64;
        graph.beats[previous]
            .neighbours
            .iter()
            .any(|pb| previous as i64 - pb.destination as i64 == branch_distance)
    }
}

/// Test-only: expose precalculated nearest neighbours (pre-threshold, pre-post-process)
/// so similarity logic can be asserted deterministically.
#[cfg(test)]
pub fn precalc_neighbours_for_test(
    settings: &JukeboxSettings,
    analysis: &RemixedAnalysis,
) -> Vec<Vec<Edge>> {
    let n = analysis.beats.len();
    let mut generator = GraphGenerator {
        settings,
        analysis,
        all_neighbours: vec![Vec::new(); n],
        min_long_branch: n as f64 / 5.0,
        computed_max_branch_distance: 0.0,
    };
    generator.precalculate_nearest_neighbors();
    generator.all_neighbours
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jukebox::model::{AudioAnalysis, Segment, TimeInterval};
    use crate::jukebox::remixer::remix;

    fn seg_feat(timbre: Vec<f64>, duration: f64) -> Segment {
        Segment {
            start: 0.0,
            duration,
            confidence: 0.0,
            loudness_start: 0.0,
            loudness_max: 0.0,
            loudness_max_time: 0.0,
            loudness_end: 0.0,
            pitches: vec![0.0; 12],
            timbre,
        }
    }

    #[test]
    fn segments_distance_weights_duration_by_100() {
        let s1 = seg_feat(vec![0.0; 12], 0.3);
        let s2 = seg_feat(vec![0.0; 12], 0.5); // only duration differs by 0.2
        assert!((segments_distance(&s1, &s2) - 20.0).abs() < 1e-9);
    }

    // Beats overlap their own segment plus the next one at the boundary, so make the timbre
    // pattern periodic with period 3: beats 0 and 3 then have identical overlapping-segment
    // sequences ([0,1]) and must be each other's nearest neighbour in the precalc layer.
    fn similarity_fixture() -> AudioAnalysis {
        let mut beats = Vec::new();
        let mut segments = Vec::new();
        for i in 0..6 {
            let start = i as f64;
            beats.push(TimeInterval { start, duration: 1.0, confidence: 1.0 });
            let timbre_val = (i % 3) as f64;
            let mut s = seg_feat(vec![timbre_val], 0.5);
            s.start = start;
            segments.push(s);
        }
        AudioAnalysis { bars: vec![], beats, tatums: vec![], sections: vec![], segments }
    }

    #[test]
    fn identical_beats_are_nearest_neighbours() {
        let analysis = remix(&similarity_fixture());
        let settings = JukeboxSettings::default();
        let all = precalc_neighbours_for_test(&settings, &analysis);
        // beat 0's closest neighbour (distance 0) is beat 3.
        assert_eq!(all[0][0].destination, 3);
        assert!((all[0][0].distance - 0.0).abs() < 1e-9);
    }

    #[test]
    fn generate_produces_well_formed_graph() {
        let analysis = remix(&similarity_fixture());
        let graph = generate(&JukeboxSettings::default(), &analysis);
        assert_eq!(graph.beats.len(), 6);
        assert!(graph.last_branch_point < graph.beats.len());
        // start_ms is seconds * 1000
        assert!((graph.beats[1].start_ms - 1000.0).abs() < 1e-9);
    }
}
