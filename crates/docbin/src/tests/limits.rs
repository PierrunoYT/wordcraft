//! Work and memory bounds against hostile `.doc` structures: tab stops, field instructions,
//! repeated anchors and failed picture decodes. Every fixture is built in code.

use std::time::{Duration, Instant};

use super::{grpprl, read, struct_doc, structured_spec, tabs_papx};
use crate::stsh::RawStyle;

/// A paragraph style.
fn raw_style(base: u16, name: &str, papx: Vec<u8>, chpx: Vec<u8>) -> RawStyle {
    RawStyle { stk: 1, istd_base: base, name: name.into(), papx, chpx }
}

/// WC-11 (#2755). A style's tab stops were uncapped (each sprmPChgTabsPapx adds up to 64, and
/// a 64 KB papx holds hundreds of them, so tens of thousands of stops, each new one scanning
/// all the others) and every style re-applied its whole base chain, so 4,095 children of such
/// a base did that work 4,095 times: minutes of CPU and about 1 GB from ~140 KB of Table stream.
/// Stops are now capped at Word's 64 and each style is resolved once, over its parent.
#[test]
fn style_tab_stops_are_capped_and_each_style_resolved_once() {
    // ~64 KB of tab sprms, each adding 64 stops at distinct positions.
    let mut papx = Vec::new();
    let mut pos = 0i16;
    while papx.len() < 64_000 {
        let add: Vec<(i16, u8)> = (0..64)
            .map(|_| {
                pos = pos.wrapping_add(3);
                (pos, 0)
            })
            .collect();
        papx.extend(grpprl(&[(0xC60D, &tabs_papx(&[], &add))]));
    }
    let bold = grpprl(&[(0x0835, &[0x01])]);
    let mut raw = vec![raw_style(0x0FFF, "Heavy", papx, bold)];
    // 2,047 children of the heavy base, then a 2,048-deep chain hanging off the last of them.
    for i in 1..2048u16 {
        raw.push(raw_style(0, &format!("Child {i}"), Vec::new(), Vec::new()));
    }
    for i in 2048..4096u16 {
        let own = grpprl(&[(0xC60D, &tabs_papx(&[], &[(i as i16, 0)]))]);
        raw.push(raw_style(i - 1, &format!("Deep {i}"), own, Vec::new()));
    }
    let fonts = vec!["Times New Roman".to_string()];
    let start = Instant::now();
    let sheet = crate::fmt::stylesheet(&raw, &fonts);
    let took = start.elapsed();
    // Before the fix this took minutes; resolved once, it is milliseconds (debug included).
    assert!(took < Duration::from_secs(5), "stylesheet took {took:?}");
    assert_eq!(sheet.styles.len(), 4096);
    for s in &sheet.styles {
        let n = s.para.tabs.as_ref().map_or(0, Vec::len);
        assert!(n <= crate::fmt::MAX_TABS, "{} has {n} tab stops", s.name);
        assert_eq!(s.chr.bold, Some(true), "{} inherits from the heavy root", s.name);
    }
    let heavy = sheet.styles.first().and_then(|s| s.para.tabs.as_ref()).map_or(0, Vec::len);
    assert_eq!(heavy, crate::fmt::MAX_TABS, "the first 64 stops are kept");
}

/// A corrupt base cycle still resolves (each style once) and keeps its own properties.
#[test]
fn style_cycles_resolve_without_looping() {
    let italic = grpprl(&[(0x0836, &[0x01])]);
    let raw = vec![
        raw_style(1, "A", Vec::new(), grpprl(&[(0x0835, &[0x01])])),
        raw_style(0, "B", Vec::new(), italic),
        raw_style(2, "Self", Vec::new(), Vec::new()),
    ];
    let sheet = crate::fmt::stylesheet(&raw, &[]);
    let a = sheet.styles.first().expect("A");
    assert_eq!((a.chr.bold, a.chr.italic), (Some(true), Some(true)), "A over B");
    let b = sheet.styles.get(1).expect("B");
    assert_eq!(b.chr.italic, Some(true));
    assert_eq!(sheet.styles.len(), 3);
}

/// WC-12 (#2756). A HYPERLINK's target was cloned into, and compared for, every character of
/// its result, with neither the instruction nor the result capped below the document's whole
/// character budget: O(target × result) bytes from one field. The instruction is now capped at
/// 8 KB and the target is set once per run; runs past the first are charged against the walk
/// budget, so the link bytes a file can make are bounded by its size.
#[test]
fn hyperlink_target_is_capped_and_set_once_per_run() {
    let url = "a".repeat(20_000);
    let result = "ab".repeat(20);
    let text: &'static str = Box::leak(format!("\u{13}HYPERLINK \"http://x/{url}\"\u{14}{result}\u{15}\r").into_boxed_str());
    let ccp = text.chars().count() as u32;
    let start = text.chars().position(|c| c == '\u{14}').expect("separator") as u32 + 1;
    // Every result character alternates bold on and off: one run each.
    let mut runs = vec![(0, Vec::new())];
    for i in 0..result.len() as u32 {
        runs.push((start + i, grpprl(&[(0x0835, &[(i % 2) as u8])])));
    }
    runs.push((start + result.len() as u32, Vec::new()));
    let spec = super::StructSpec { text, chpx_runs: runs, ..structured_spec(text, ccp) };
    let file = struct_doc(&spec);
    let doc = read(&file).expect("opens");
    let p0 = doc.body.first().and_then(|b| b.as_para()).expect("para");
    assert_eq!(p0.text, result, "the result text is all kept");
    let links: Vec<&str> = p0.runs.iter().filter_map(|r| r.props.link.as_deref()).collect();
    let first = links.first().expect("the first run is linked");
    assert!(first.starts_with("http://x/aaa") && first.len() <= 8 << 10, "target capped: {} bytes", first.len());
    let linked: usize = links.iter().map(|l| l.len()).sum();
    assert!(linked <= 2 * file.len(), "{linked} link bytes from a {}-byte file", file.len());
    assert!(p0.runs.len() >= result.len(), "one run per formatting change");
}
