use crate::jukebox::model::{AudioAnalysis, Segment, TimeInterval};

#[derive(Debug, Clone)]
pub struct RemixedBeat {
    pub index: usize,
    pub start: f64,    // seconds
    pub duration: f64, // seconds
    pub index_in_parent: usize,
    pub overlapping_segments: Vec<usize>, // indices into RemixedAnalysis.segments
}

#[derive(Debug, Clone)]
pub struct RemixedAnalysis {
    pub beats: Vec<RemixedBeat>,
    pub segments: Vec<Segment>,
}

/// Port of Remixer.preprocessTrack, reduced to beats (the only quanta the graph uses).
pub fn remix(analysis: &AudioAnalysis) -> RemixedAnalysis {
    let segments = analysis.segments.clone();
    let mut beats: Vec<RemixedBeat> = analysis
        .beats
        .iter()
        .enumerate()
        .map(|(index, b)| RemixedBeat {
            index,
            start: b.start,
            duration: b.duration,
            index_in_parent: 0,
            overlapping_segments: Vec::new(),
        })
        .collect();

    connect_index_in_parent(&analysis.bars, &mut beats);
    connect_overlapping_segments(&segments, &mut beats);

    RemixedAnalysis { beats, segments }
}

/// Port of connectQuanta(bars, beats): a beat's index within its containing bar.
fn connect_index_in_parent(bars: &[TimeInterval], beats: &mut [RemixedBeat]) {
    let mut last = 0usize;
    for bar in bars {
        let mut count = 0usize;
        let mut i = last;
        while i < beats.len() {
            let start = beats[i].start;
            if start >= bar.start && start < bar.start + bar.duration {
                beats[i].index_in_parent = count;
                count += 1;
                last = i;
            } else if start > bar.start {
                break;
            }
            i += 1;
        }
    }
}

/// Port of connectAllOverlappingSegments(beats): segments sorted by start, so once a
/// segment starts after the beat ends we can stop.
fn connect_overlapping_segments(segments: &[Segment], beats: &mut [RemixedBeat]) {
    for beat in beats.iter_mut() {
        let beat_end = beat.start + beat.duration;
        for (idx, seg) in segments.iter().enumerate() {
            if seg.start + seg.duration < beat.start {
                continue;
            }
            if seg.start > beat_end {
                break;
            }
            beat.overlapping_segments.push(idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jukebox::model::Segment;

    fn ti(start: f64, duration: f64) -> TimeInterval {
        TimeInterval { start, duration, confidence: 1.0 }
    }
    fn seg(start: f64, duration: f64) -> Segment {
        Segment {
            start,
            duration,
            confidence: 1.0,
            loudness_start: 0.0,
            loudness_max: 0.0,
            loudness_max_time: 0.0,
            loudness_end: 0.0,
            pitches: vec![0.0],
            timbre: vec![0.0],
        }
    }

    fn fixture() -> AudioAnalysis {
        AudioAnalysis {
            bars: vec![ti(0.0, 2.0), ti(2.0, 2.0)],
            beats: vec![ti(0.0, 1.0), ti(1.0, 1.0), ti(2.0, 1.0), ti(3.0, 1.0)],
            tatums: vec![],
            sections: vec![],
            segments: vec![seg(0.0, 0.5), seg(0.5, 0.5), seg(1.0, 1.0), seg(2.0, 2.0)],
        }
    }

    #[test]
    fn index_in_parent_is_position_within_bar() {
        let r = remix(&fixture());
        assert_eq!(r.beats[0].index_in_parent, 0);
        assert_eq!(r.beats[1].index_in_parent, 1);
        assert_eq!(r.beats[2].index_in_parent, 0); // first beat of bar 1
        assert_eq!(r.beats[3].index_in_parent, 1);
    }

    #[test]
    fn overlapping_segments_are_collected() {
        let r = remix(&fixture());
        // beat0 spans [0,1]: segments 0 (0-0.5), 1 (0.5-1), 2 (1-2). Segment 3 starts at 2 > 1.
        assert_eq!(r.beats[0].overlapping_segments, vec![0, 1, 2]);
    }
}
