//! Print Markup for the text formats: HTML, Markdown and plain text copies end with the
//! document's comments, numbered in document order, each with its author, the text it comments
//! on, what it says and its replies (like the notes at the end of a printed document).

use std::collections::{BTreeMap, HashMap, HashSet};

use wordcraft_doc::para::{InlineObject, OBJ};
use wordcraft_doc::{Document, StoryRef};

/// Longest quoted anchor text (characters).
const MAX_QUOTE: usize = 200;
const MAX_COMMENTS: usize = 5_000;
const MAX_REPLIES: usize = 200;
const MAX_PARENT_CHAIN: usize = 64;

/// One comment as written out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub author: String,
    /// The text it comments on (shortened), or empty.
    pub quote: String,
    pub text: String,
    /// (author, text) of each reply, oldest first.
    pub replies: Vec<(String, String)>,
}

fn root(doc: &Document, id: u32) -> u32 {
    let mut cur = id;
    for _ in 0..MAX_PARENT_CHAIN {
        match doc.comments.get(&cur).and_then(|c| c.parent) {
            Some(p) if p != cur && doc.comments.contains_key(&p) => cur = p,
            _ => break,
        }
    }
    cur
}

fn author(a: &str) -> String {
    if a.trim().is_empty() { "Author".to_string() } else { a.trim().to_string() }
}

fn text_of(doc: &Document, part: u32) -> String {
    doc.plain_text(StoryRef::Part(part)).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The document's top-level comments: those anchored in the body in document order, then the
/// rest; replies go under the comment they answer.
pub fn notes(doc: &Document) -> Vec<Note> {
    if doc.comments.is_empty() {
        return Vec::new();
    }
    // Anchor order and the text between each comment's start and end.
    let mut order: Vec<u32> = Vec::new();
    let mut seen = HashSet::new();
    // Each quote with its length in characters, so a long anchor costs O(1) per character
    // rather than a recount of the quote so far.
    let mut quotes: HashMap<u32, (String, usize)> = HashMap::new();
    let mut open: Vec<u32> = Vec::new();
    for path in doc.para_paths(StoryRef::Body) {
        let Some(p) = doc.para(StoryRef::Body, &path) else { continue };
        for (off, c) in p.text.char_indices() {
            if c == OBJ {
                match p.object_at(off) {
                    Some(InlineObject::CommentStart { id }) => {
                        if seen.insert(*id) {
                            order.push(*id);
                        }
                        if !open.contains(id) && open.len() < 64 {
                            open.push(*id);
                        }
                    }
                    Some(InlineObject::CommentEnd { id }) => open.retain(|x| x != id),
                    _ => {}
                }
                continue;
            }
            for id in &open {
                let (q, n) = quotes.entry(*id).or_default();
                if *n <= MAX_QUOTE {
                    q.push(c);
                    *n += 1;
                }
            }
        }
        for id in &open {
            // Past the cap nothing more is kept, and trailing spaces are dropped below.
            let (q, n) = quotes.entry(*id).or_default();
            if *n <= MAX_QUOTE {
                q.push(' ');
                *n += 1;
            }
        }
    }
    for id in doc.comments.keys() {
        if seen.insert(*id) {
            order.push(*id);
        }
    }
    let mut replies: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for id in doc.comments.keys() {
        let r = root(doc, *id);
        if r != *id {
            let v = replies.entry(r).or_default();
            if v.len() < MAX_REPLIES {
                v.push(*id);
            }
        }
    }
    let mut out = Vec::new();
    for id in order {
        let Some(c) = doc.comments.get(&id) else { continue };
        if root(doc, id) != id {
            continue;
        }
        let quote = quotes.get(&id).map(|(q, _)| q.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
        let quote = if quote.chars().count() > MAX_QUOTE { format!("{}…", quote.chars().take(MAX_QUOTE).collect::<String>()) } else { quote };
        let replies =
            replies.get(&id).into_iter().flatten().filter_map(|r| doc.comments.get(r)).map(|r| (author(&r.author), text_of(doc, r.part))).collect();
        out.push(Note { author: author(&c.author), quote, text: text_of(doc, c.part), replies });
        if out.len() >= MAX_COMMENTS {
            break;
        }
    }
    out
}

/// `bytes` (a copy in format `ext`) with the comments written at its end.
pub fn append(ext: &str, doc: &Document, bytes: Vec<u8>) -> Vec<u8> {
    let notes = notes(doc);
    if notes.is_empty() {
        return bytes;
    }
    match ext {
        "html" | "htm" | "xhtml" => {
            let mut s = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => return e.into_bytes(),
            };
            let at = s.rfind("</body>").unwrap_or(s.len());
            s.insert_str(at, &html(&notes));
            s.into_bytes()
        }
        "md" | "markdown" => {
            let mut s = bytes;
            s.extend_from_slice(markdown(&notes).as_bytes());
            s
        }
        "txt" | "text" => {
            let mut s = bytes;
            s.extend_from_slice(text(&notes).as_bytes());
            s
        }
        _ => bytes,
    }
}

fn html(notes: &[Note]) -> String {
    use crate::html::esc;
    let mut s = String::from("<section class=\"comments\">\n<h2>Comments</h2>\n<ol>\n");
    for (i, n) in notes.iter().enumerate() {
        s.push_str(&format!("<li id=\"comment-{}\"><p><strong>{}</strong>", i + 1, esc(&n.author)));
        if !n.quote.is_empty() {
            s.push_str(&format!(" on <q>{}</q>", esc(&n.quote)));
        }
        s.push_str(&format!(": {}</p>", esc(&n.text)));
        if !n.replies.is_empty() {
            s.push_str("<ul>");
            for (a, t) in &n.replies {
                s.push_str(&format!("<li><strong>{}</strong>: {}</li>", esc(a), esc(t)));
            }
            s.push_str("</ul>");
        }
        s.push_str("</li>\n");
    }
    s.push_str("</ol>\n</section>\n");
    s
}

fn markdown(notes: &[Note]) -> String {
    use crate::markdown::escape;
    let mut s = String::from("\n\n## Comments\n\n");
    for (i, n) in notes.iter().enumerate() {
        s.push_str(&format!("{}. **{}**", i + 1, escape(&n.author)));
        if !n.quote.is_empty() {
            s.push_str(&format!(" on “{}”", escape(&n.quote)));
        }
        s.push_str(&format!(": {}\n", escape(&n.text)));
        for (a, t) in &n.replies {
            s.push_str(&format!("    - **{}**: {}\n", escape(a), escape(t)));
        }
    }
    s
}

fn text(notes: &[Note]) -> String {
    let mut s = String::from("\r\n\r\nComments\r\n");
    for (i, n) in notes.iter().enumerate() {
        s.push_str(&format!("\r\n[{}] {}", i + 1, n.author));
        if !n.quote.is_empty() {
            s.push_str(&format!(" on “{}”", n.quote));
        }
        s.push_str(&format!(": {}\r\n", n.text));
        for (a, t) in &n.replies {
            s.push_str(&format!("    {a}: {t}\r\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::{CharProps, Comment, Paragraph, PartKind, Pos, para_block};

    fn comment(d: &mut Document, id: u32) {
        let part = d.add_part(PartKind::Comment, vec![para_block(Paragraph::with_text("Note", CharProps::default()))]);
        d.comments.insert(id, Comment { author: "Ada".into(), part, ..Default::default() });
    }

    fn start(d: &mut Document, para: usize, off: usize, id: u32) {
        d.insert_object(&Pos::body(para, off), InlineObject::CommentStart { id }, &CharProps::default()).unwrap();
    }

    /// Audit minor (quadratic hotspots): a comment left open over a long document used to recount
    /// its quote, spaces included, for every character it spans. The quote is still the first
    /// `MAX_QUOTE` characters, and it is fast.
    #[test]
    fn long_open_comments_quote_quickly() {
        let mut d = Document::from_text(&vec!["word word word word"; 20_000].join("\n"));
        for id in 0..8 {
            comment(&mut d, id);
            start(&mut d, 0, 0, id);
        }
        let t = std::time::Instant::now();
        let notes = notes(&d);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
        assert_eq!(notes.len(), 8);
        let want = format!("{}…", vec!["word"; 1_000].join(" ").chars().take(MAX_QUOTE).collect::<String>());
        assert!(notes.iter().all(|n| n.quote == want), "{:?}", notes[0].quote);
    }

    /// Spaces between paragraphs count toward the cap, as before.
    #[test]
    fn paragraph_breaks_count_toward_the_quote() {
        let mut d = Document::from_text(&format!("ab{}cd", "\n".repeat(MAX_QUOTE + 5)));
        comment(&mut d, 0);
        start(&mut d, 0, 0, 0);
        assert_eq!(notes(&d)[0].quote, "ab");
        let mut d = Document::from_text("The quarterly\nfigures look strong.");
        comment(&mut d, 0);
        d.insert_object(&Pos::body(1, 7), InlineObject::CommentEnd { id: 0 }, &CharProps::default()).unwrap();
        start(&mut d, 0, 4, 0);
        assert_eq!(notes(&d)[0].quote, "quarterly figures");
    }
}
