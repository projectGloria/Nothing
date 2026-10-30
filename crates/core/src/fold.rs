//! Case folding that never changes byte length.
//!
//! The index keeps two name arenas: the original names (for display) and a folded copy
//! (for case-insensitive matching). If folding could change a name's UTF-8 length, the
//! folded arena would need its own offset table (+4 bytes per entry). So we fold a char
//! only when its lowercase form is a single char of identical UTF-8 width. That covers
//! ASCII, Latin-1, Greek, Cyrillic, etc. Oddballs like Turkish 'İ' (2 bytes -> 3) stay
//! as-is, which costs nothing in practice.

/// Append the folded form of `s` to `out`. `out` grows by exactly `s.len()` bytes.
#[inline]
pub fn fold_into(s: &str, out: &mut Vec<u8>) {
    if s.is_ascii() {
        // Fast path for ~99% of real filenames: a straight byte map the compiler vectorizes.
        out.extend(s.bytes().map(|b| b.to_ascii_lowercase()));
        return;
    }
    let mut buf = [0u8; 4];
    for c in s.chars() {
        if c.is_ascii() {
            out.push((c as u8).to_ascii_lowercase());
            continue;
        }
        let mut lower = c.to_lowercase();
        let l = lower.next().unwrap_or(c);
        let c2 = if lower.next().is_none() && l.len_utf8() == c.len_utf8() { l } else { c };
        out.extend_from_slice(c2.encode_utf8(&mut buf).as_bytes());
    }
}

pub fn fold(s: &str) -> String {
    let mut v = Vec::with_capacity(s.len());
    fold_into(s, &mut v);
    // Every byte we pushed came from a valid `char::encode_utf8`.
    String::from_utf8(v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_length() {
        for s in ["Hello.TXT", "ÄÖÜ ß", "ПРИВЕТ", "İstanbul", "ΣΊΣΥΦΟΣ", "日本語"] {
            assert_eq!(fold(s).len(), s.len(), "{s}");
        }
        assert_eq!(fold("Hello.TXT"), "hello.txt");
        assert_eq!(fold("ПРИВЕТ"), "привет");
    }
}
