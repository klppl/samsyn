//! Single-span text diff on byte offsets, never splitting a UTF-8 character.
//! yrs documents created with default options index text by UTF-8 bytes.

#[derive(Debug, PartialEq, Eq)]
pub struct TextDiff {
    pub index: usize,
    pub remove: usize,
    pub insert: String,
}

pub fn diff_text(a: &str, b: &str) -> Option<TextDiff> {
    if a == b {
        return None;
    }
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let max_start = ab.len().min(bb.len());
    let mut start = 0;
    while start < max_start && ab[start] == bb[start] {
        start += 1;
    }
    while start > 0 && !(a.is_char_boundary(start) && b.is_char_boundary(start)) {
        start -= 1;
    }
    let mut end_a = ab.len();
    let mut end_b = bb.len();
    while end_a > start && end_b > start && ab[end_a - 1] == bb[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }
    while !(a.is_char_boundary(end_a) && b.is_char_boundary(end_b)) {
        end_a += 1;
        end_b += 1;
    }
    Some(TextDiff { index: start, remove: end_a - start, insert: b[start..end_b].to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(a: &str, d: &TextDiff) -> String {
        format!("{}{}{}", &a[..d.index], d.insert, &a[d.index + d.remove..])
    }

    #[test]
    fn round_trips() {
        let cases = [
            ("", "hello"),
            ("hello", ""),
            ("hello world", "hello there world"),
            ("abc", "abd"),
            ("aaa", "aa"),
            ("x😀y", "x😀😀y"),
            ("prefix 😀 suffix", "prefix 🙂 suffix"),
            ("åäö", "åöö"),
            ("line1\nline2\nline3", "line1\nline2 edited\nline3"),
        ];
        for (a, b) in cases {
            match diff_text(a, b) {
                Some(d) => assert_eq!(apply(a, &d), b, "{a:?} -> {b:?}"),
                None => assert_eq!(a, b),
            }
        }
    }

    #[test]
    fn yrs_apply_converges() {
        use yrs::{Doc, GetString, Text, Transact};
        for (a, b) in [("hello", "hello world"), ("x😀y", "x🙂y"), ("åäö", "")] {
            let doc = Doc::new();
            let t = doc.get_or_insert_text("t");
            t.insert(&mut doc.transact_mut(), 0, a);
            if let Some(d) = diff_text(a, b) {
                let mut txn = doc.transact_mut();
                if d.remove > 0 {
                    t.remove_range(&mut txn, d.index as u32, d.remove as u32);
                }
                if !d.insert.is_empty() {
                    t.insert(&mut txn, d.index as u32, &d.insert);
                }
            }
            assert_eq!(t.get_string(&doc.transact()), b);
        }
    }
}
