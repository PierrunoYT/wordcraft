//! Footnotes, endnotes and bookmarks: `PlcffndRef`/`PlcffndTxt`, `PlcfendRef`/`PlcfendTxt`,
//! and `SttbfBkmk` + `Plcfbkf`/`Plcfbkl` ([MS-DOC] §2.9).

/// Cap on notes and bookmarks per document.
const MAX: usize = 10_000;
/// Longest bookmark name kept, in characters (Word's limit).
const MAX_NAME: usize = 40;

/// Parse the note reference/story PLCs. `ref_pair`/`txt_pair` are the FIB pair indices
/// (`(2, 3)` for footnotes, `(46, 47)` for endnotes); story ranges are relative to the
/// note subdocument.
///
/// The story CPs of a well-formed PLC ascend, so the stories are disjoint. A hostile table
/// that steps back would make many notes read the same text again (each one materialised
/// as its own part), so a story starting before the end of the last one kept is dropped:
/// every subdocument CP becomes at most one note.
pub(crate) fn parse_notes(table: &[u8], fib: &crate::fib::Fib, ref_pair: usize, txt_pair: usize) -> Vec<(u32, (u32, u32))> {
    let refs = fib.pair(ref_pair).and_then(|(fc, lcb)| plc_of(table, fc, lcb, 2)).unwrap_or_default();
    let txts = fib.pair(txt_pair).and_then(|(fc, lcb)| plc_of(table, fc, lcb, 0)).unwrap_or_default();
    if txts.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut kept_to = 0u32;
    // The reference PLC's aFtnIdx order matches the story order; pair them by index.
    for (i, &cp) in refs.iter().enumerate() {
        if let (Some(&a), Some(&b)) = (txts.get(i), txts.get(i + 1))
            && b > a
            && a >= kept_to
        {
            out.push((cp, (a, b)));
            kept_to = b;
        }
    }
    out
}

/// The CPs of a PLC (the trailing element is a limit and is kept so callers can pair
/// range ends) plus the data-element size check. Returns the CPs only.
fn plc_of(table: &[u8], fc: u32, lcb: u32, data_size: usize) -> Option<Vec<u32>> {
    if lcb == 0 {
        return None;
    }
    let plc = table.get(fc as usize..(fc as usize).saturating_add(lcb as usize))?;
    if plc.len() < 4 {
        return None;
    }
    // aCP has one more element than aData: n = (len - data_size) / (4 + data_size).
    let n = (plc.len() - data_size) / (4 + data_size);
    if n == 0 || n > MAX {
        return None;
    }
    let mut cps = Vec::with_capacity(n.min(MAX));
    for i in 0..=n.min(MAX) {
        match plc.get(i * 4..i * 4 + 4) {
            Some(b) => cps.push(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            None => break,
        }
    }
    Some(cps)
}

/// A bookmark boundary to insert while walking.
pub(crate) enum Mark {
    Start(String),
    End(String),
}

/// Parse `SttbfBkmk` + `Plcfbkf` + `Plcfbkl` into (cp, event) pairs sorted for a sequential
/// walk. Hidden bookmarks (leading `_`) are kept: the engine can decide.
pub(crate) fn parse_bookmarks(table: &[u8], fib: &crate::fib::Fib) -> Vec<(u32, Mark)> {
    let names = fib.pair(crate::fib::pair::STTBF_BKMK).and_then(|(fc, lcb)| sttb_strings(table, fc, lcb)).unwrap_or_default();
    let starts = fib.pair(crate::fib::pair::PLCF_BKF).and_then(|(fc, lcb)| plc_of(table, fc, lcb, 4)).unwrap_or_default();
    let ends = fib.pair(crate::fib::pair::PLCF_BKL).and_then(|(fc, lcb)| plc_of(table, fc, lcb, 2)).unwrap_or_default();
    let n = starts.len().saturating_sub(1).min(ends.len().saturating_sub(1)).min(names.len()).min(MAX);
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let (Some(&a), Some(&b)) = (starts.get(i), ends.get(i)) else { break };
        if b < a {
            continue;
        }
        if let Some(name) = names.get(i) {
            if name.is_empty() {
                continue;
            }
            out.push((a, Mark::Start(name.clone())));
            out.push((b, Mark::End(name.clone())));
        }
    }
    out.sort_by_key(|(cp, m)| (*cp, matches!(m, Mark::End(_))));
    out
}

/// Strings of an extended STTB (fExtend = 0xFFFF, 2-byte chars, no extra data).
fn sttb_strings(table: &[u8], fc: u32, lcb: u32) -> Option<Vec<String>> {
    let b = table.get(fc as usize..(fc as usize).saturating_add(lcb as usize))?;
    if b.len() < 6 {
        return None;
    }
    let c_data = u16::from_le_bytes([b[2], b[3]]) as usize;
    if c_data > MAX {
        return None;
    }
    let mut at = 6usize;
    let mut out = Vec::with_capacity(c_data);
    for _ in 0..c_data {
        let cch = match b.get(at..at + 2) {
            Some(x) => u16::from_le_bytes([x[0], x[1]]) as usize,
            None => break,
        };
        at += 2;
        // Skip the whole string so the next one is read from its own start; the name keeps
        // Word's 40 characters at most.
        let chars = b.get(at..at + cch * 2)?;
        at += cch * 2;
        let chars = chars.get(..cch.min(MAX_NAME) * 2).unwrap_or(chars);
        let s: String = chars.as_chunks::<2>().0.iter().map(|c| char::from_u32(u16::from_le_bytes(*c) as u32).unwrap_or('\u{FFFD}')).collect();
        out.push(s);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit minor: a bookmark name over 40 characters advanced the reader by 40 only, so every
    /// later name was read from the middle of the long one.
    #[test]
    fn long_bookmark_names_dont_desync_the_names_after_them() {
        let long = "L".repeat(50);
        let mut sttb = 0xFFFFu16.to_le_bytes().to_vec();
        sttb.extend_from_slice(&3u16.to_le_bytes());
        sttb.extend_from_slice(&0u16.to_le_bytes());
        for name in [long.as_str(), "next", "last"] {
            sttb.extend_from_slice(&(name.chars().count() as u16).to_le_bytes());
            for c in name.encode_utf16() {
                sttb.extend_from_slice(&c.to_le_bytes());
            }
        }
        let names = sttb_strings(&sttb, 0, sttb.len() as u32).unwrap();
        assert_eq!(names, ["L".repeat(MAX_NAME), "next".into(), "last".into()]);
    }
}
