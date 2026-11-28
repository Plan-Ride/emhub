//! Stop search words, as timetable.py has them (fold, prefix_match, match_score, word_index, line_key, name_keys).

use std::collections::{HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

/// Lowercase ASCII words: "Zvolen,,Zlatý Potok" -> ["zvolen", "zlaty", "potok"] (NFD, non-ASCII dropped).
pub fn fold(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    for c in text.nfd() {
        if !c.is_ascii() {
            continue; // Python: .encode("ascii", "ignore") drops it without splitting the word
        }
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            word.push(c);
        } else if !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

pub fn fold_joined(text: &str) -> String {
    fold(text).join(" ")
}

/// Can `token` be split into non-empty prefixes of words[i], words[i+1], ...? Returns words used, or 0.
pub fn prefix_match(token: &str, words: &[String], i: usize) -> usize {
    if i >= words.len() {
        return 0;
    }
    let w = words[i].as_str();
    if w.len() >= 3 && token.len() > w.len() && token.starts_with(w) {
        return 1;
    }
    let (t, wb) = (token.as_bytes(), w.as_bytes());
    for k in (1..=t.len().min(wb.len())).rev() {
        if t[..k] != wb[..k] {
            continue;
        }
        if k == t.len() {
            return 1;
        }
        let rest = prefix_match(&token[k..], words, i + 1);
        if rest > 0 {
            return 1 + rest;
        }
    }
    0
}

/// cp.sk-style shortcuts (see timetable.match_score). None = no match; higher is better.
pub fn match_score(tokens: &[String], words: &[String]) -> Option<i64> {
    fn go(tokens: &[String], words: &[String], t: usize, i: usize, penalty: i64, first: usize, best: &mut Option<i64>) {
        if t == tokens.len() {
            let whole = if i >= 1 && tokens[tokens.len() - 1] == words[i - 1] { 2 } else if i == 0 && tokens[tokens.len() - 1] == words[words.len() - 1] { 2 } else { 0 };
            let score = -penalty + if first == 0 { 4 } else { 0 } + whole;
            *best = Some(best.map_or(score, |b| b.max(score)));
            return;
        }
        for j in i..words.len() {
            let used = prefix_match(&tokens[t], words, j);
            if used > 0 {
                let p = penalty + (j - i) as i64 * 3 * i64::from(t > 0) + (used as i64 - 1) * 2;
                go(tokens, words, t + 1, j + used, p, if t == 0 { j } else { first }, best);
            }
        }
    }
    let mut best = None;
    go(tokens, words, 0, 0, 0, 0, &mut best);
    best
}

/// {key: [n]}: first letter, first two letters, and the first letters of two words in a row.
pub fn word_index(names: &[Vec<String>]) -> HashMap<String, Vec<u32>> {
    let mut index: HashMap<String, Vec<u32>> = HashMap::new();
    for (n, words) in names.iter().enumerate() {
        let mut keys: HashSet<String> = HashSet::new();
        for w in words.iter().filter(|w| !w.is_empty()) {
            keys.insert(w[..1].to_string());
            if w.len() >= 2 {
                keys.insert(w[..2].to_string());
            }
        }
        for pair in words.windows(2) {
            if !pair[0].is_empty() && !pair[1].is_empty() {
                keys.insert(format!("{}{}", &pair[0][..1], &pair[1][..1]));
            }
        }
        for key in keys {
            index.entry(key).or_default().push(n as u32);
        }
    }
    index
}

pub fn candidates<'a>(index: &'a HashMap<String, Vec<u32>>, token: &str) -> &'a [u32] {
    let key = &token[..token.len().min(2)];
    index.get(key).map(Vec::as_slice).unwrap_or(&[])
}

/// A line as the dashboard writes it and the timetable has it: "60101", "Linka 34" -> "34".
pub fn line_key(line: &str) -> String {
    line.trim().split(' ').last().unwrap_or("").to_lowercase()
}

/// A stop's name as a direction can say it: the whole name, or just its town.
pub fn name_keys(name: &str) -> [String; 2] {
    [fold_joined(name), fold_joined(name.split(',').next().unwrap_or(""))]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str) -> Vec<String> {
        fold(text)
    }

    #[test]
    fn folding() {
        assert_eq!(fold("Zvolen,,Zlatý Potok"), ["zvolen", "zlaty", "potok"]);
        assert_eq!(fold("Nám.Š.Moysesa"), ["nam", "s", "moysesa"]);
        assert_eq!(fold("Žiar n.Hronom 2"), ["ziar", "n", "hronom", "2"]);
        assert_eq!(line_key("Linka 34"), "34");
        assert_eq!(line_key(" 60101 "), "60101");
    }

    #[test]
    fn scores_like_python() {
        let name = w("Village, Zlatý Potok, konečná");
        assert!(match_score(&w("v z p k"), &name).is_some());
        assert!(match_score(&w("vill zla po kon"), &name).is_some());
        assert!(match_score(&w("bb as"), &w("Banská Bystrica, AS")).is_some());
        assert!(match_score(&w("nabrezie"), &w("Štadlerovo nábr.")).is_some());
        assert!(match_score(&w("zzz"), &name).is_none());
        // values checked against timetable.match_score
        assert_eq!(match_score(&w("zvolen"), &w("Zvolen, AS")), Some(6));
        assert_eq!(match_score(&w("as"), &w("Zvolen, AS")), Some(2));
        assert_eq!(match_score(&w("zv a"), &w("Zvolen, AS")), Some(4));
    }
}
