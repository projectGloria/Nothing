//! Shortening text to a pixel width, for the status-bar details.
//!
//! Every function takes the measuring function as a parameter instead of an HDC, so the
//! trimming rules are plain logic: the UI passes GetTextExtentPoint32W, the tests pass a
//! character count.
//!
//! Text is only ever cut at the end, marked with "..": "The Happiness of All Mankind..".
//! A middle ellipsis or a collapsed "(…)" makes you reassemble the name in your head;
//! a clean start reads at a glance.

const DOTS: &str = "..";
/// Where a cut may land: after a whole word, never mid-word if it can be helped.
const BREAKS: &[char] = &[' ', '-', '_', '.', ','];
/// Trailing junk left after a cut: "Title - (" -> "Title".
const TRIM: &[char] = &[' ', '-', '_', '.', ',', '(', '[', '{'];
/// Bracket groups with more inside than this are release tags; shorter ones are a year
/// or a short tag worth keeping: "(2019)", "[EN]".
const TAG_MIN: usize = 6;
/// A containing folder name longer than this (in characters) counts as "too long" and is
/// shortened even when the line would fit.
const LONG_FOLDER: usize = 24;
/// What a shortened folder name is cut down to (in characters, before the dots).
const SHORT_FOLDER: usize = 20;

/// Cut `s` at the end to fit `max`, with "..": at a word boundary when one is reasonably
/// close (the last 40% of what fits), otherwise mid-word. Empty if not even ".." fits.
pub fn end(s: &str, max: i32, w: &impl Fn(&str) -> i32) -> String {
    if w(s) <= max { s.to_string() } else { cut(s, max, w) }
}

/// Like `end`, but always marks the result with "..", even if `s` itself would fit:
/// for text that has already lost its tail (a stripped release tag), where showing it
/// bare would pass it off as the whole name.
fn cut(s: &str, max: i32, w: &impl Fn(&str) -> i32) -> String {
    if w(DOTS) > max {
        return String::new();
    }
    let c: Vec<char> = s.chars().collect();
    let with_dots = |n: usize| c[..n].iter().collect::<String>() + DOTS;
    // Width grows with n: binary-search the longest prefix that fits with the dots.
    let (mut lo, mut hi) = (0, c.len());
    while lo < hi {
        let m = (lo + hi).div_ceil(2);
        if w(&with_dots(m)) <= max {
            lo = m;
        } else {
            hi = m - 1;
        }
    }
    // Back up to a word boundary: i == lo means the next char is a break, i.e. the
    // prefix already ends on a whole word.
    let at = (lo * 6 / 10..=lo).rev().find(|&i| i == c.len() || BREAKS.contains(&c[i])).unwrap_or(lo);
    let head: String = c[..at].iter().collect();
    let head = head.trim_end_matches(TRIM);
    if head.is_empty() {
        with_dots(lo)
    } else {
        head.to_string() + DOTS
    }
}

/// Start of the first release-tag group, if there's meaningful text before it:
/// "Show - E01 - Title (1080p BluRay x265)" -> "Show - E01 - Title".
fn untagged(s: &str) -> Option<&str> {
    let mut depth = 0;
    let mut open = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' | '{' => {
                if depth == 0 {
                    open = i;
                }
                depth += 1;
            }
            ')' | ']' | '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 && s[open + 1..i].chars().count() > TAG_MIN {
                    let head = s[..open].trim_end_matches(TRIM);
                    return (head.chars().count() >= 3).then_some(head);
                }
            }
            _ => {}
        }
    }
    None
}

/// A file name shortened to `max`: release tags go first ("…Mankind.." rather than
/// "…Mankind (1080p Blu.."), then a plain end cut.
pub fn name(s: &str, max: i32, w: &impl Fn(&str) -> i32) -> String {
    if w(s) <= max {
        return s.to_string();
    }
    match untagged(s) {
        Some(head) => cut(head, max, w),
        None => end(s, max, w),
    }
}

/// The containing folder's name, cut to what identifies it: up to its first bracket
/// ("Chernobyl (2019) Season 1 S01 (1080p…)" -> "Chernobyl.."), then to a word
/// boundary within SHORT_FOLDER characters. Unchanged if it isn't long.
fn short_folder(last: &str) -> String {
    if last.chars().count() <= LONG_FOLDER {
        return last.to_string();
    }
    let base = last
        .find(['(', '[', '{'])
        .map(|i| last[..i].trim_end_matches(TRIM))
        .filter(|h| h.chars().count() >= 3)
        .unwrap_or(last);
    let chars = |s: &str| s.chars().count() as i32;
    let short = end(base, (SHORT_FOLDER + DOTS.len()) as i32, &chars);
    if short.ends_with(DOTS) { short } else { short + DOTS }
}

/// A folder path for the details line. The parent chain is what says where a file is,
/// so it stays whole; the containing folder's own name, usually the long part (a
/// release folder, a hash), is cut short: `C:\Users\Nihil\Downloads\Chernobyl..`.
/// Done whenever that name is long, not only when the line overflows.
/// If even that doesn't fit, folders are dropped from the middle (`C:\…\Downloads\
/// Chernobyl..`), and as a last resort the whole thing is cut at the end.
pub fn path(s: &str, max: i32, w: &impl Fn(&str) -> i32) -> String {
    let comps: Vec<&str> = s.split('\\').collect();
    let last = comps[comps.len() - 1];
    let long = last.chars().count() > LONG_FOLDER;
    if !long && w(s) <= max {
        return s.to_string();
    }
    if comps.len() < 2 {
        return end(s, max, w);
    }
    let short = short_folder(last);
    let mut parts: Vec<&str> = comps[..comps.len() - 1].to_vec();
    parts.push(&short);
    let whole = parts.join("\\");
    if w(&whole) <= max {
        return whole;
    }
    // "\\server\share\x" splits as ["", "", "server", "share", "x"].
    let root = if s.starts_with(r"\\") { 4 } else { 1 };
    if parts.len() > root + 1 {
        let head = parts[..root].join("\\");
        // k = trailing components kept; at least one must be dropped.
        for k in (1..parts.len() - root).rev() {
            let c = format!("{head}\\…\\{}", parts[parts.len() - k..].join("\\"));
            if w(&c) <= max {
                return c;
            }
        }
    }
    end(&whole, max, w)
}

/// The fields of the details line, trimmed so that together with `sep` between them
/// they fit in `max`. Type and size are short and always shown whole; name and folder
/// share what's left. The folder gives way first, but the name is never squeezed below
/// ~60% of the space while the folder still has room to give. In a very narrow window
/// the type, then the folder, are dropped outright. Empty inputs (no folder for a drive
/// root, no size for a folder) are left out.
///
/// Returned as (name + folder, type + size): paint centres the first pair in the bar and
/// right-aligns the second, so `max` must leave room for one `sep` between the groups.
pub fn fields(name_: &str, folder: &str, kind: &str, size: &str, max: i32, sep: &str, w: &impl Fn(&str) -> i32) -> (Vec<String>, Vec<String>) {
    let fixed: Vec<&str> = [kind, size].into_iter().filter(|s| !s.is_empty()).collect();
    let count = 1 + !folder.is_empty() as i32 + fixed.len() as i32;
    let budget = max - fixed.iter().map(|s| w(s)).sum::<i32>() - w(sep) * (count - 1);
    // Below a few characters' width nothing readable fits: shed a field and retry.
    let floor = w("MMMMMMMM");
    if budget < floor && !kind.is_empty() {
        return fields(name_, folder, "", size, max, sep, w);
    }
    if budget < floor && !folder.is_empty() {
        return fields(name_, "", kind, size, max, sep, w);
    }
    let nw = w(name_);
    // The folder at its preferred length (long containing-folder name already short).
    let p_pref = if folder.is_empty() { String::new() } else { path(folder, budget, w) };
    let (n, p) = if nw + w(&p_pref) <= budget {
        (name_.to_string(), p_pref)
    } else if folder.is_empty() {
        (name(name_, budget, w), String::new())
    } else {
        // The name claims no more than it needs with its release tags dropped.
        let compact = untagged(name_).map_or(nw, |h| w(h) + w(DOTS));
        let name_cap = compact.min(nw).min((budget * 60 / 100).max(budget - w(&p_pref)));
        let p = path(folder, budget - name_cap, w);
        // The path rarely uses its whole allowance (it shrinks in whole folders);
        // hand the slack back to the name.
        (name(name_, budget - w(&p), w), p)
    };
    let keep = |v: [&str; 2]| v.into_iter().filter(|s| !s.is_empty()).map(String::from).collect();
    (keep([&n, &p]), keep([kind, size]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // One "pixel" per character keeps the expected strings easy to count.
    fn w(s: &str) -> i32 {
        s.chars().count() as i32
    }

    #[test]
    fn end_cuts_at_words() {
        assert_eq!(end("short", 10, &w), "short");
        assert_eq!(end("The Happiness of All Mankind", 20, &w), "The Happiness of..");
        // No boundary close enough: mid-word.
        assert_eq!(end("abcdefghij", 6, &w), "abcd..");
        // Trailing separators don't survive the cut.
        assert_eq!(end("Show - Episode", 9, &w), "Show..");
        assert_eq!(end("abc", 1, &w), "");
    }

    #[test]
    fn name_drops_tags_then_cuts() {
        let n = "Chernobyl (2019) - S01E04 - The Happiness of All Mankind (1080p BluRay x265 Silence)";
        assert_eq!(name(n, 200, &w), n);
        assert_eq!(name(n, 60, &w), "Chernobyl (2019) - S01E04 - The Happiness of All Mankind..");
        assert_eq!(name(n, 40, &w), "Chernobyl (2019) - S01E04 - The..");
        // Title alone would fit but not with the dots: still cut and marked, never shown
        // bare as if it were the whole name.
        assert_eq!(name(n, 57, &w), "Chernobyl (2019) - S01E04 - The Happiness of All..");
        // A short group like a year is not a tag.
        assert_eq!(untagged("Show (2019) E01"), None);
        assert_eq!(untagged("(1080p BluRay x265) Show"), None); // nothing before it
    }

    #[test]
    fn path_keeps_parents_and_shortens_folder() {
        let p = r"C:\Users\Nihil\Downloads\Chernobyl (2019) Season 1 S01 (1080p BluRay x265 HEVC 10bit AAC 5.1 Silence)";
        // Shortened even with room to spare: the folder name is long.
        assert_eq!(path(p, 500, &w), r"C:\Users\Nihil\Downloads\Chernobyl..");
        // Short folder names are left alone while they fit.
        assert_eq!(path(r"C:\Program Files\Steam", 100, &w), r"C:\Program Files\Steam");
        // No bracket: word boundary within the limit.
        assert_eq!(short_folder("Counter-Strike Global Offensive Extras"), "Counter-Strike..");
        // Too narrow for the whole chain: drop middle folders.
        assert_eq!(path(p, 30, &w), r"C:\…\Downloads\Chernobyl..");
        assert_eq!(path(r"C:\Program Files\Steam\common\csgo\maps", 21, &w), r"C:\…\common\csgo\maps");
        assert_eq!(path(r"\\srv\share\a\b\c", 16, &w), r"\\srv\share\…\c");
        // Nothing to drop: end cut.
        assert_eq!(path(r"C:\abcdefghij", 8, &w), r"C:\abc..");
    }

    #[test]
    fn fields_share_space() {
        let sep = " · "; // 3 wide
        let flat = |(a, b): (Vec<String>, Vec<String>)| [a, b].concat();
        assert_eq!(fields("a", r"C:\x", "Text", "1 KB", 100, sep, &w), (vec!["a".into(), r"C:\x".into()], vec!["Text".into(), "1 KB".into()]));
        let f = flat(fields(
            "Show - E01 - Title (1080p BluRay x265)",
            r"C:\Users\Nihil\Downloads\Show (2019) Season 1 (1080p BluRay x265)",
            "MKV File",
            "2 GB",
            72,
            sep,
            &w,
        ));
        assert_eq!(f, ["Show - E01 - Title..", r"C:\Users\Nihil\Downloads\Show..", "MKV File", "2 GB"]);
        assert!(f.iter().map(|s| w(s)).sum::<i32>() + 3 * 3 <= 72);
        // No size for folders, no folder for drive roots.
        assert_eq!(flat(fields("C:", "", "Local Disk", "", 100, sep, &w)), ["C:", "Local Disk"]);
        // Very narrow: type goes first.
        assert_eq!(flat(fields("movie", r"C:\a\b", "MKV File", "2 GB", 21, sep, &w)), ["movie", r"C:\a\b", "2 GB"]);
    }
}
