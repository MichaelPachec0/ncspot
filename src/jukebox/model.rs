use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioAnalysis {
    #[serde(default)]
    pub bars: Vec<TimeInterval>,
    #[serde(default)]
    pub beats: Vec<TimeInterval>,
    #[serde(default)]
    pub tatums: Vec<TimeInterval>,
    #[serde(default)]
    pub sections: Vec<Section>,
    #[serde(default)]
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimeInterval {
    pub start: f64,
    pub duration: f64,
    #[serde(default)]
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Section {
    pub start: f64,
    pub duration: f64,
    #[serde(default)]
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub duration: f64,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub loudness_start: f64,
    #[serde(default)]
    pub loudness_max: f64,
    #[serde(default)]
    pub loudness_max_time: f64,
    #[serde(default)]
    pub loudness_end: f64,
    #[serde(default)]
    pub pitches: Vec<f64>,
    #[serde(default)]
    pub timbre: Vec<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_spotify_analysis_payload() {
        let json = r#"{
            "bars": [{"start": 0.0, "duration": 2.0, "confidence": 0.5}],
            "beats": [{"start": 0.0, "duration": 0.5, "confidence": 0.9}],
            "tatums": [{"start": 0.0, "duration": 0.25, "confidence": 0.8}],
            "sections": [{"start": 0.0, "duration": 10.0, "confidence": 0.7}],
            "segments": [{
                "start": 0.0, "duration": 0.3, "confidence": 0.6,
                "loudness_start": -20.0, "loudness_max": -5.0,
                "loudness_max_time": 0.1, "loudness_end": 0.0,
                "pitches": [0.1, 0.2], "timbre": [1.0, 2.0]
            }]
        }"#;
        let a: AudioAnalysis = serde_json::from_str(json).unwrap();
        assert_eq!(a.beats.len(), 1);
        assert_eq!(a.segments[0].pitches, vec![0.1, 0.2]);
        assert_eq!(a.segments[0].timbre, vec![1.0, 2.0]);
    }

    #[test]
    fn tolerates_missing_optional_arrays() {
        let a: AudioAnalysis = serde_json::from_str(r#"{"beats": []}"#).unwrap();
        assert!(a.segments.is_empty());
    }
}
