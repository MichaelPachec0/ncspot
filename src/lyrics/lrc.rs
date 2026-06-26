use std::sync::LazyLock;

use regex::Regex;

use crate::lyrics::model::LyricLine;

static TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\d{1,2}):(\d{2})(?:[.:](\d{1,3}))?\]").unwrap());

pub fn parse_lrc(input: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for raw in input.lines() {
        let text = TAG.replace_all(raw, "").trim().to_string();
        for cap in TAG.captures_iter(raw) {
            let min: u32 = cap[1].parse().unwrap_or(0);
            let sec: u32 = cap[2].parse().unwrap_or(0);
            let frac_ms = cap.get(3).map_or(0, |m| {
                let s = m.as_str();
                let n: u32 = s.parse().unwrap_or(0);
                match s.len() {
                    1 => n * 100,
                    2 => n * 10,
                    _ => n,
                }
            });
            let start = (min * 60 + sec) * 1000 + frac_ms;
            out.push(LyricLine {
                start_ms: Some(start),
                text: text.clone(),
                translation: None,
                romanization: None,
            });
        }
    }
    out.sort_by_key(|l| l.start_ms);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_and_three_digit_fractions() {
        let lines = parse_lrc("[00:01.50]half\n[00:02.123]ms");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].start_ms, Some(1500));
        assert_eq!(lines[0].text, "half");
        assert_eq!(lines[1].start_ms, Some(2123));
    }

    #[test]
    fn skips_metadata_and_blank_lines() {
        let lines = parse_lrc("[ar:Artist]\n[ti:Title]\n\n[00:00.00]first");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "first");
    }

    #[test]
    fn expands_multiple_timestamps_on_one_line() {
        let lines = parse_lrc("[00:01.00][00:05.00]chorus");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].start_ms, Some(1000));
        assert_eq!(lines[1].start_ms, Some(5000));
        assert!(lines.iter().all(|l| l.text == "chorus"));
    }

    #[test]
    fn results_are_sorted_by_time() {
        let lines = parse_lrc("[00:05.00]b\n[00:01.00]a");
        assert_eq!(lines[0].text, "a");
        assert_eq!(lines[1].text, "b");
    }
}
