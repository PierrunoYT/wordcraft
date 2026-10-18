//! Fonts documents embed (#1171), kept apart from the fonts every document sees (audit WC-40).
//!
//! An opened document's embedded font files go into a store of their own, and the document gets a
//! scope naming its files (`Document::font_scope`). Only lookups made for that scope
//! ([`FontDb::face_in`], [`FontDb::has_family_in`], `word::resolve_in`) see them: a font one
//! document embeds never draws, or passes itself off as, a font in another document or in that
//! document's PDF, and it is never a fallback for other text. Within its own document a face
//! already available (bundled, user or installed) still wins, as before: an embedded face only
//! fills in a family and style nothing else provides.
//!
//! Faces are never unloaded (a [`FaceRef`](super::FaceRef) lives for the rest of the process), so
//! the store is bounded instead of freed: a file opened again, or embedded by another document, is
//! stored once (its bytes compared, not just hashed), the same set of files is the same scope, and
//! the total bytes, faces and scopes are capped. A font past the caps isn't loaded: its text falls
//! back as for a font that isn't embedded.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::{FontBytes, FontDb, FontFace, base_style, closest, enumerate_faces, make_face, norm};

/// Most bytes of embedded font files kept for the whole process.
pub const MAX_DOCUMENT_FONT_BYTES: usize = 256 << 20;
/// Most embedded faces kept for the whole process.
const MAX_DOCUMENT_FACES: usize = 4096;
/// Most faces taken from one embedded file (collections, variable fonts' named instances).
const MAX_FACES_PER_FILE: usize = 256;
/// Most distinct sets of embedded files (scopes) for the whole process.
const MAX_SCOPES: usize = 1 << 16;

/// The embedded font files of every document opened, and each document's scope.
#[derive(Default)]
pub(super) struct DocumentFonts {
    files: Vec<DocumentFile>,
    /// Each scope's files, as sorted indexes into `files`: scope `n` is `scopes[n - 1]`.
    scopes: Vec<Box<[usize]>>,
    /// Bytes held by `files`.
    bytes: usize,
    /// Faces held by `files`.
    faces: usize,
}

struct DocumentFile {
    hash: u64,
    data: Arc<Vec<u8>>,
    faces: Vec<Arc<FontFace>>,
}

impl DocumentFonts {
    fn file_index(&self, hash: u64, data: &[u8]) -> Option<usize> {
        self.files.iter().position(|f| f.hash == hash && f.data.as_slice() == data)
    }

    /// The faces of scope `scope`'s files, in the order they were embedded.
    fn scope_faces(&self, scope: u32) -> Vec<Arc<FontFace>> {
        let Some(files) = (scope as usize).checked_sub(1).and_then(|i| self.scopes.get(i)) else { return Vec::new() };
        files.iter().filter_map(|i| self.files.get(*i)).flat_map(|f| f.faces.iter().cloned()).collect()
    }
}

fn content_hash(data: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    h.finish()
}

/// The faces of an embedded font file (at most [`MAX_FACES_PER_FILE`]).
fn parse_faces(data: &Arc<Vec<u8>>) -> Vec<Arc<FontFace>> {
    enumerate_faces(data)
        .into_iter()
        .take(MAX_FACES_PER_FILE)
        .filter_map(|f| {
            let mut face = make_face(FontBytes::Owned(data.clone()), f.index, f.family, f.style, f.legacy, f.coords)?;
            face.local_names = f.local;
            Some(Arc::new(face))
        })
        .collect()
}

/// An embedded file being added: parsed when the store doesn't have it yet.
enum Incoming {
    New(u64, Arc<Vec<u8>>, Vec<Arc<FontFace>>),
    Known(u64, Vec<u8>),
}

impl FontDb {
    fn read_documents(&self) -> std::sync::RwLockReadGuard<'_, DocumentFonts> {
        self.documents.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Load the font files a document embeds (#1171) for this run, in memory only (not user
    /// fonts, never stored), and return the document's font scope: what `Document::font_scope`
    /// holds, so the document's own lookups see them and no other document's do. 0 when none of
    /// the files is usable. Files already loaded, by this document or another, are not loaded
    /// again, and the same files give the same scope.
    pub fn add_document_fonts(&self, files: Vec<Vec<u8>>) -> u32 {
        if files.is_empty() {
            return 0;
        }
        let files: Vec<(u64, Vec<u8>)> = files.into_iter().map(|d| (content_hash(&d), d)).collect();
        // Which files are new (the first copy of each), found before parsing any outside the lock.
        let new: Vec<bool> = {
            let store = self.read_documents();
            files
                .iter()
                .enumerate()
                .map(|(i, (h, d))| store.file_index(*h, d).is_none() && !files.iter().take(i).any(|(h2, d2)| h2 == h && d2 == d))
                .collect()
        };
        let incoming: Vec<Incoming> = files
            .into_iter()
            .zip(new)
            .map(|((hash, data), new)| {
                if new {
                    let data = Arc::new(data);
                    let faces = parse_faces(&data);
                    Incoming::New(hash, data, faces)
                } else {
                    Incoming::Known(hash, data)
                }
            })
            .collect();
        let mut store = self.documents.write().unwrap_or_else(|e| e.into_inner());
        let mut scope: Vec<usize> = Vec::new();
        for file in incoming {
            let (hash, data) = match &file {
                Incoming::New(h, d, _) => (*h, d.as_slice()),
                Incoming::Known(h, d) => (*h, d.as_slice()),
            };
            // Another document may have added it since it was found new.
            if let Some(i) = store.file_index(hash, data) {
                scope.push(i);
                continue;
            }
            let Incoming::New(hash, data, faces) = file else { continue };
            if faces.is_empty() {
                continue;
            }
            if store.bytes.saturating_add(data.len()) > MAX_DOCUMENT_FONT_BYTES || store.faces.saturating_add(faces.len()) > MAX_DOCUMENT_FACES {
                log::warn!(
                    "embedded fonts: the limit of {MAX_DOCUMENT_FONT_BYTES} bytes or {MAX_DOCUMENT_FACES} faces is reached; a font isn't loaded"
                );
                continue;
            }
            store.bytes = store.bytes.saturating_add(data.len());
            store.faces = store.faces.saturating_add(faces.len());
            scope.push(store.files.len());
            store.files.push(DocumentFile { hash, data, faces });
        }
        scope.sort_unstable();
        scope.dedup();
        if scope.is_empty() {
            return 0;
        }
        if let Some(i) = store.scopes.iter().position(|s| **s == *scope) {
            return u32::try_from(i + 1).unwrap_or(0);
        }
        if store.scopes.len() >= MAX_SCOPES {
            log::warn!("embedded fonts: more than {MAX_SCOPES} sets of embedded fonts; this document's aren't loaded");
            return 0;
        }
        store.scopes.push(scope.into_boxed_slice());
        u32::try_from(store.scopes.len()).unwrap_or(0)
    }

    /// Is `family` (a typographic or legacy family name) one of the fonts scope `scope` embeds?
    pub fn document_has_family(&self, scope: u32, family: &str) -> bool {
        scope != 0 && self.read_documents().scope_faces(scope).iter().any(|f| f.named(family))
    }

    /// [`FontDb::has_family`] for a document whose font scope is `scope`: also its embedded fonts.
    pub fn has_family_in(&self, scope: u32, family: &str) -> bool {
        self.has_family(family) || self.document_has_family(scope, family)
    }

    /// [`FontDb::face`] for a document whose font scope is `scope`. A face already available
    /// (bundled, user or installed) of that family and style is kept; the document's own face
    /// takes its place only for a family or style nothing else provides. Scope 0 is
    /// [`FontDb::face`] itself.
    pub fn face_in(&self, scope: u32, family: &str, style: &str) -> Arc<FontFace> {
        let own = if scope == 0 { None } else { closest(&self.read_documents().scope_faces(scope), family, base_style(style)) };
        let Some(own) = own else { return self.face(family, style) };
        let wanted = norm(base_style(style));
        let exact = |f: &FontFace| f.named(family) && norm(base_style(&f.style)) == wanted;
        if self.has_family(family) {
            let available = self.face(family, style);
            if exact(&available) || (available.named(family) && !exact(&own)) {
                return available;
            }
        }
        own
    }

    /// The embedded font files loaded (each once), their faces and bytes, and the scopes made.
    pub fn document_font_counts(&self) -> (usize, usize, usize, usize) {
        let store = self.read_documents();
        (store.files.len(), store.faces, store.bytes, store.scopes.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bundled Source Sans 3 file renamed `family` (as long as the original name), so it is a
    /// font the database doesn't have.
    fn renamed(file: &str, family: &str) -> Vec<u8> {
        assert_eq!(family.len(), "Source Sans 3".len());
        let mut data = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts").join(file)).unwrap();
        let utf16 = |s: &str| s.encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<u8>>();
        for (from, to) in [(utf16("Source Sans 3"), utf16(family)), (b"Source Sans 3".to_vec(), family.as_bytes().to_vec())] {
            let mut i = 0;
            while let Some(at) = data[i..].windows(from.len()).position(|w| w == from) {
                data[i + at..i + at + to.len()].copy_from_slice(&to);
                i += at + to.len();
            }
        }
        data
    }

    fn db() -> FontDb {
        let db = FontDb::with_font_dirs(vec![]);
        db.set_system_fallback(false);
        db
    }

    #[test]
    fn a_document_font_resolves_only_for_the_document_that_embeds_it() {
        const FAMILY: &str = "Docfonts Sans";
        let db = db();
        let faces0 = db.read_faces().len();
        let a = db.add_document_fonts(vec![renamed("SourceSans3-Bold.ttf", FAMILY)]);
        assert_ne!(a, 0);
        // Document A sees it.
        assert!(db.has_family_in(a, FAMILY));
        let own = db.face_in(a, FAMILY, "Bold");
        assert_eq!((own.family.as_str(), own.style.as_str()), (FAMILY, "Bold"));
        // Its closest style stands in for the others, as for any family.
        assert_eq!(db.face_in(a, FAMILY, "Regular").id(), own.id());
        // Nobody else does: not the global lookups, not a document without fonts, not one
        // embedding something else.
        let b = db.add_document_fonts(vec![renamed("SourceSans3-It.ttf", "Otherfont Sns")]);
        assert_ne!(b, a);
        for scope in [0, b, 9999] {
            assert!(!db.has_family_in(scope, FAMILY), "scope {scope}");
            assert_ne!(db.face_in(scope, FAMILY, "Bold").family, FAMILY, "scope {scope}");
        }
        assert!(!db.has_family(FAMILY));
        assert!(!db.families().iter().any(|f| f == FAMILY));
        assert_eq!(db.read_faces().len(), faces0, "the loaded faces are unchanged");
        // Never a fallback for other text either.
        let latin = db.face("Source Serif 4", "Regular");
        assert!(db.fallback_for('W', latin.id()).is_some_and(|f| f.family != FAMILY));
    }

    #[test]
    fn faces_already_available_are_kept() {
        let db = db();
        // A document embedding a copy of a bundled family, under its name.
        let bundled = db.face("Source Sans 3", "Bold");
        let bytes = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts/SourceSans3-Bold.ttf")).unwrap();
        let s = db.add_document_fonts(vec![bytes]);
        assert_eq!(db.face_in(s, "Source Sans 3", "Bold").id(), bundled.id());
        assert_eq!(db.face_in(s, "Source Sans 3", "Regular").id(), db.face("Source Sans 3", "Regular").id());
    }

    #[test]
    fn the_same_fonts_are_loaded_once() {
        const FAMILY: &str = "Repeat Sans 3";
        let db = db();
        let font = renamed("SourceSans3-Regular.ttf", FAMILY);
        let first = db.add_document_fonts(vec![font.clone(), font.clone()]);
        let counts = db.document_font_counts();
        assert_eq!(counts.0, 1, "a file embedded twice is kept once");
        for _ in 0..20 {
            assert_eq!(db.add_document_fonts(vec![font.clone()]), first, "the same files are the same scope");
        }
        assert_eq!(db.document_font_counts(), counts);
        // Another document embedding the same file and one more: the file is shared.
        let bold = renamed("SourceSans3-Bold.ttf", FAMILY);
        let other = db.add_document_fonts(vec![bold, font]);
        assert_ne!(other, first);
        assert_eq!(db.document_font_counts().0, 2);
        assert_eq!(db.face_in(other, FAMILY, "Bold").style, "Bold");
        assert_eq!(db.face_in(first, FAMILY, "Bold").style, "Regular");
    }

    #[test]
    fn junk_and_nothing_give_no_scope() {
        let db = db();
        assert_eq!(db.add_document_fonts(vec![]), 0);
        assert_eq!(db.add_document_fonts(vec![b"not a font".to_vec(), Vec::new()]), 0);
        assert_eq!(db.document_font_counts(), (0, 0, 0, 0));
        assert!(!db.has_family_in(0, "Docfonts Sans"));
    }

    #[test]
    fn embedded_fonts_are_capped() {
        let db = db();
        let font = renamed("SourceSans3-Regular.ttf", "Capfonts Sans");
        // Fill the store up to just under the byte cap with stand-in entries.
        {
            let mut store = db.documents.write().unwrap();
            store.bytes = MAX_DOCUMENT_FONT_BYTES - font.len() + 1;
        }
        assert_eq!(db.add_document_fonts(vec![font.clone()]), 0, "past the cap: not loaded");
        assert_eq!(db.document_font_counts().0, 0);
        db.documents.write().unwrap().bytes = 0;
        assert_ne!(db.add_document_fonts(vec![font]), 0);
    }
}
