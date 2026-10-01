//! Emulation of Python's `csv.Sniffer().sniff(sample, delimiters=",;\t|")`:
//! the quote-aware regex pass first, then the per-line frequency-consistency
//! pass. Ported from CPython's `Lib/csv.py` so ambiguous files resolve the same.

use fancy_regex::Regex;

pub const DELIMITERS: &str = ",;\t|";
/// CPython's `Sniffer.preferred`.
const PREFERRED: [char; 5] = [',', '\t', ';', ' ', ':'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialect {
    pub delimiter: u8,
    pub quote: u8,
    pub double_quote: bool,
    pub skip_initial_space: bool,
}

/// `None` is Python's `csv.Error: Could not determine delimiter`.
pub fn sniff(sample: &str) -> Option<Dialect> {
    let (quote, double_quote, mut delimiter, mut skip) = guess_quote_and_delimiter(sample);
    if delimiter.is_none() {
        let (d, s) = guess_delimiter(sample)?;
        delimiter = Some(d);
        skip = s;
    }
    let delimiter = delimiter?;
    if !delimiter.is_ascii() {
        return None;
    }
    Some(Dialect {
        delimiter: delimiter as u8,
        quote: quote.filter(|q| q.is_ascii()).map(|q| q as u8).unwrap_or(b'"'),
        double_quote,
        skip_initial_space: skip,
    })
}

fn is_delim(c: char) -> bool {
    DELIMITERS.contains(c)
}

fn bump(counts: &mut Vec<(char, usize)>, key: char) {
    match counts.iter_mut().find(|(k, _)| *k == key) {
        Some((_, n)) => *n += 1,
        None => counts.push((key, 1)),
    }
}

/// First maximum in insertion order, like `max(d, key=d.get)`.
fn argmax(counts: &[(char, usize)]) -> Option<(char, usize)> {
    let mut best: Option<(char, usize)> = None;
    for &(k, n) in counts {
        if best.is_none_or(|(_, bn)| n > bn) {
            best = Some((k, n));
        }
    }
    best
}

fn guess_quote_and_delimiter(data: &str) -> (Option<char>, bool, Option<char>, bool) {
    let patterns = [
        r#"(?sm)(?P<delim>[^\w\n"'])(?P<space> ?)(?P<quote>["']).*?(?P=quote)(?P=delim)"#,
        r#"(?sm)(?:^|\n)(?P<quote>["']).*?(?P=quote)(?P<delim>[^\w\n"'])(?P<space> ?)"#,
        r#"(?sm)(?P<delim>[^\w\n"'])(?P<space> ?)(?P<quote>["']).*?(?P=quote)(?:$|\n)"#,
        r#"(?sm)(?:^|\n)(?P<quote>["']).*?(?P=quote)(?:$|\n)"#,
    ];
    let mut found: Vec<fancy_regex::Captures> = Vec::new();
    for pat in patterns {
        let Ok(re) = Regex::new(pat) else { continue };
        let matches: Vec<_> = re.captures_iter(data).filter_map(|c| c.ok()).collect();
        if !matches.is_empty() {
            found = matches;
            break;
        }
    }
    if found.is_empty() {
        return (None, false, None, false);
    }
    let mut quotes: Vec<(char, usize)> = Vec::new();
    let mut delims: Vec<(char, usize)> = Vec::new();
    let mut spaces = 0usize;
    for m in &found {
        if let Some(q) = m.name("quote").and_then(|g| g.as_str().chars().next()) {
            bump(&mut quotes, q);
        }
        let Some(dg) = m.name("delim") else { continue };
        if let Some(d) = dg.as_str().chars().next() {
            if is_delim(d) {
                bump(&mut delims, d);
            }
        }
        if m.name("space").is_some_and(|g| !g.as_str().is_empty()) {
            spaces += 1;
        }
    }
    let quote = argmax(&quotes).map(|(q, _)| q);
    let (delim, skip) = match argmax(&delims) {
        Some((d, n)) => (Some(d), n == spaces),
        None => (None, false),
    };
    // A doubled quote between delimiters means "" is an escaped quote.
    let delim_s = delim.map(|d| d.to_string()).unwrap_or_default();
    let esc = fancy_regex::escape(&delim_s).to_string();
    let q = quote.map(|q| q.to_string()).unwrap_or_default();
    let dq = format!(
        r"(?m)(({esc})|^)\W*{q}[^{esc}\n]*{q}[^{esc}\n]*{q}\W*(({esc})|$)"
    );
    let double_quote = Regex::new(&dq).ok().and_then(|re| re.is_match(data).ok()).unwrap_or(false);
    (quote, double_quote, delim, skip)
}

fn count(line: &str, pat: &str) -> usize {
    line.matches(pat).count()
}

fn guess_delimiter(data: &str) -> Option<(char, bool)> {
    let lines: Vec<&str> = data.split('\n').filter(|l| !l.is_empty()).collect();
    let chunk = lines.len().min(10);
    let cands: Vec<char> = DELIMITERS.chars().collect();
    // char -> [(frequency, occurrences)] in first-seen order
    let mut freq: Vec<Vec<(usize, usize)>> = vec![Vec::new(); cands.len()];
    // char -> (mode frequency, adjusted count), insertion-ordered
    let mut modes: Vec<(char, (usize, i64))> = Vec::new();
    let mut delims: Vec<(char, (usize, i64))> = Vec::new();
    let (mut start, mut end, mut iteration) = (0usize, chunk, 0usize);
    while start < lines.len() {
        iteration += 1;
        for line in &lines[start..end.min(lines.len())] {
            for (ci, c) in cands.iter().enumerate() {
                let f = count(line, &c.to_string());
                match freq[ci].iter_mut().find(|(k, _)| *k == f) {
                    Some((_, n)) => *n += 1,
                    None => freq[ci].push((f, 1)),
                }
            }
        }
        for (ci, c) in cands.iter().enumerate() {
            let items = &freq[ci];
            if items.len() == 1 && items[0].0 == 0 {
                continue;
            }
            let mode = if items.len() > 1 {
                let mut best = items[0];
                for it in items {
                    if it.1 > best.1 {
                        best = *it;
                    }
                }
                let others: usize = items.iter().filter(|it| **it != best).map(|it| it.1).sum();
                (best.0, best.1 as i64 - others as i64)
            } else {
                (items[0].0, items[0].1 as i64)
            };
            match modes.iter_mut().find(|(k, _)| k == c) {
                Some((_, m)) => *m = mode,
                None => modes.push((*c, mode)),
            }
        }
        let total = (chunk * iteration).min(lines.len()) as f64;
        let mut consistency = 1.0f64;
        while delims.is_empty() && consistency >= 0.9 {
            for (k, v) in &modes {
                if v.0 > 0 && v.1 > 0 && (v.1 as f64 / total) >= consistency && is_delim(*k) {
                    delims.push((*k, *v));
                }
            }
            consistency -= 0.01;
        }
        if delims.len() == 1 {
            let d = delims[0].0;
            return Some((d, skip_space(lines[0], d)));
        }
        start = end;
        end += chunk;
    }
    if delims.is_empty() {
        return None;
    }
    if delims.len() > 1 {
        for p in PREFERRED {
            if delims.iter().any(|(k, _)| *k == p) {
                return Some((p, skip_space(lines[0], p)));
            }
        }
    }
    let mut items: Vec<((usize, i64), char)> = delims.iter().map(|(k, v)| (*v, *k)).collect();
    items.sort();
    let d = items.last()?.1;
    Some((d, skip_space(lines[0], d)))
}

fn skip_space(first: &str, d: char) -> bool {
    count(first, &d.to_string()) == count(first, &format!("{d} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delim(s: &str) -> Option<u8> {
        sniff(s).map(|d| d.delimiter)
    }

    #[test]
    fn plain_delimiters() {
        assert_eq!(delim("a,b,c\n1,2,3\n"), Some(b','));
        assert_eq!(delim("a;b;c\n1;2;3\n"), Some(b';'));
        assert_eq!(delim("a\tb\tc\n1\t2\t3\n"), Some(b'\t'));
        assert_eq!(delim("a|b\n1|2\n"), Some(b'|'));
    }

    #[test]
    fn quoted_fields() {
        let d = sniff("\"a\",\"b\"\n\"1\",\"2\"\n").unwrap();
        assert_eq!(d.delimiter, b',');
        assert_eq!(d.quote, b'"');
        let d = sniff("'a';'b'\n'1';'2'\n").unwrap();
        assert_eq!(d.delimiter, b';');
        assert_eq!(d.quote, b'\'');
    }

    #[test]
    fn single_column_cannot_be_sniffed() {
        assert_eq!(delim("Artist\nSlam\n"), None);
        assert_eq!(delim(""), None);
    }
}
