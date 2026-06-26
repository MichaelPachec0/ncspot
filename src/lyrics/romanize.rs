use pinyin::ToPinyin;

fn has_hangul(s: &str) -> bool {
    s.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c))
}

fn has_han(s: &str) -> bool {
    s.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

/// Romanize a single Hangul syllable block (U+AC00..=U+D7A3) via Revised
/// Romanization (no special inter-syllable assimilation rules).
fn romanize_hangul(s: &str) -> String {
    const LEAD: [&str; 19] = [
        "g", "kk", "n", "d", "tt", "r", "m", "b", "pp", "s", "ss", "", "j", "jj", "ch",
        "k", "t", "p", "h",
    ];
    const VOWEL: [&str; 21] = [
        "a", "ae", "ya", "yae", "eo", "e", "yeo", "ye", "o", "wa", "wae", "oe", "yo", "u",
        "wo", "we", "wi", "yu", "eu", "ui", "i",
    ];
    const TAIL: [&str; 28] = [
        "", "k", "k", "k", "n", "n", "n", "t", "l", "l", "l", "l", "l", "l", "l", "l", "m",
        "p", "p", "t", "t", "ng", "t", "t", "k", "t", "p", "t",
    ];
    let mut out = String::new();
    for c in s.chars() {
        let cp = c as u32;
        if ('\u{AC00}'..='\u{D7A3}').contains(&c) {
            let i = cp - 0xAC00;
            let lead = (i / 588) as usize;
            let vowel = ((i % 588) / 28) as usize;
            let tail = (i % 28) as usize;
            out.push_str(LEAD[lead]);
            out.push_str(VOWEL[vowel]);
            out.push_str(TAIL[tail]);
        } else if !c.is_whitespace() {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    out
}

fn romanize_pinyin(s: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for ch in s.chars() {
        if let Some(py) = ch.to_pinyin() {
            parts.push(py.plain().to_string());
        } else if !ch.is_whitespace() {
            parts.push(ch.to_string());
        }
    }
    parts.join(" ")
}

pub fn romanize_line(text: &str) -> Option<String> {
    if has_hangul(text) {
        Some(romanize_hangul(text))
    } else if has_han(text) {
        Some(romanize_pinyin(text))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn romanizes_chinese_to_pinyin() {
        let r = romanize_line("中国").unwrap();
        assert_eq!(r, "zhong guo");
    }

    #[test]
    fn romanizes_korean_hangul() {
        // 한국 -> han + guk (revised romanization, simplified)
        let r = romanize_line("한국").unwrap();
        assert_eq!(r, "hanguk");
    }

    #[test]
    fn returns_none_for_latin() {
        assert_eq!(romanize_line("hello world"), None);
    }
}
