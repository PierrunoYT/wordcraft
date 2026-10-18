//! OPC package access: bounded zip reading, relationships, part names.

use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read, Write};
use std::sync::Arc;

use crate::DocxError;
use crate::xml::{self, El};

/// Largest single part we decompress.
pub const MAX_PART: u64 = 256 * 1024 * 1024;
/// Largest total of all decompressed parts.
pub const MAX_TOTAL: u64 = 512 * 1024 * 1024;
/// Most zip entries we look at.
pub const MAX_ENTRIES: usize = 20_000;
/// Most names of unreadable zip entries we keep for the report (all of them are counted).
pub const MAX_SKIPPED_NAMES: usize = 32;
/// Longest name of an unreadable zip entry we keep, in characters.
const MAX_SKIPPED_NAME: usize = 200;

/// Relationship type suffixes (after `.../relationships/`).
pub mod rt {
    pub const OFFICE_DOC: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
    pub const CORE: &str = "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties";
    pub const APP: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties";
    pub const CUSTOM: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/custom-properties";
    pub const CUSTOM_XML_PROPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXmlProps";
    pub const STYLES: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
    pub const NUMBERING: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering";
    pub const SETTINGS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings";
    pub const ATTACHED_TEMPLATE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/attachedTemplate";
    pub const FONT_TABLE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/fontTable";
    pub const THEME: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme";
    pub const FOOTNOTES: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";
    pub const ENDNOTES: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes";
    pub const COMMENTS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
    pub const COMMENTS_EX: &str = "http://schemas.microsoft.com/office/2011/relationships/commentsExtended";
    pub const COMMENTS_IDS: &str = "http://schemas.microsoft.com/office/2016/09/relationships/commentsIds";
    pub const COMMENTS_EXTENSIBLE: &str = "http://schemas.microsoft.com/office/2018/08/relationships/commentsExtensible";
    pub const HEADER: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/header";
    pub const FOOTER: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer";
    pub const IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
    pub const HYPERLINK: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";
    pub const VBA_PROJECT: &str = "http://schemas.microsoft.com/office/2006/relationships/vbaProject";
    pub const CHART: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart";
    pub const DIAGRAM_DATA: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramData";
    pub const DIAGRAM_DRAWING: &str = "http://schemas.microsoft.com/office/2007/relationships/diagramDrawing";
    pub const DIAGRAM_LAYOUT: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramLayout";
    pub const DIAGRAM_QUICK_STYLE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramQuickStyle";
    pub const DIAGRAM_COLORS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/diagramColors";
    pub const CUSTOM_XML: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml";
}

/// `Document::passthrough` key for a macro project, kept as opaque bytes (never parsed or run).
pub const VBA_PROJECT_PART: &str = "word/vbaProject.bin";
/// `Document::passthrough` key listing the parts the project relates to (VBA data, signatures…),
/// one `relationship type \t part name \t content type` line each; each part's bytes are kept
/// under its part name. Not a package part: WordCraft's own bookkeeping.
pub const VBA_RELATED: &str = "wordcraft:vbaProject.related";
/// Most parts we carry along with a macro project.
pub const MAX_VBA_RELATED: usize = 64;

/// `Document::passthrough` key listing the parts that objects read from a file refer to (charts,
/// SmartArt diagrams, OLE objects: see `wordcraft_doc::graphic::Embedded`), with their content
/// types and own relationships, as [`EmbeddedManifest`] lines. Each part's bytes are kept under its
/// part name. Not a package part: WordCraft's own bookkeeping.
pub const EMBEDDED_PARTS: &str = "wordcraft:embedded.parts";
/// The font table kept from a file (read by `Reader::keep_part_tree`), with the fonts it embeds.
pub const FONT_TABLE_PART: &str = "word/fontTable.xml";
/// Custom XML roots kept by the package (P) or main document (D): `owner\ttype\tpart`.
/// Their opaque bytes and related parts use the same bounded manifest as embedded objects.
pub const CUSTOM_XML_ROOTS: &str = "wordcraft:customXml.roots";
/// The original `word/settings.xml` when it holds settings the model doesn't (merged into on
/// save by `write::settings_merge`). Not a package part: WordCraft's own bookkeeping.
pub const SETTINGS_ORIG: &str = "wordcraft:settings.orig";
/// Extended properties (`docProps/app.xml`) the model doesn't hold but a save keeps
/// ([`APP_KEPT_PROPS`]): `name\tvalue` lines. Not a package part: WordCraft's own bookkeeping.
pub const APP_KEPT: &str = "wordcraft:app.kept";
/// The `docProps/app.xml` properties carried over from the file. Counts (pages, lines,
/// characters, editing time) and the producing application's version are not: they describe
/// the file Word saved, not the one WordCraft writes.
pub const APP_KEPT_PROPS: &[&str] =
    &["Template", "Manager", "Company", "HyperlinkBase", "ScaleCrop", "LinksUpToDate", "SharedDoc", "HyperlinksChanged"];
/// Parts WordCraft doesn't model, carried verbatim: `owner\ttype\tpart` lines (owner `P` for
/// the package, `D` for the main document), with their bytes and relationships in the
/// [`EmbeddedManifest`]. Not a package part: WordCraft's own bookkeeping.
pub const UNMODELLED_ROOTS: &str = "wordcraft:unmodelled.roots";
/// `Document::passthrough` key reporting the zip entries the file held that couldn't be read
/// (corrupt data, a bad checksum) and so are missing from the document, as [`UnreadableParts`]
/// (the count, then a name per line). Never written to a file: WordCraft's own bookkeeping,
/// taken out by [`take_unreadable_parts`] when the file is opened.
pub const UNREADABLE_PARTS: &str = "wordcraft:unreadable.parts";
/// Most parts we carry along for a file's embedded objects.
pub const MAX_EMBEDDED_PARTS: usize = 4096;
/// Most relationships we keep for one such part.
pub const MAX_EMBEDDED_RELS: usize = 1024;
/// Longest markup we keep for one embedded object.
pub const MAX_EMBEDDED_XML: usize = 1024 * 1024;

/// One part an embedded object needs: its content type and its own relationships (internal
/// targets are part names).
#[derive(Clone, Debug, Default)]
pub struct EmbeddedPart {
    pub content_type: String,
    pub rels: Vec<Rel>,
}

/// The parts kept for embedded objects, by part name (see [`EMBEDDED_PARTS`]). Stored as text:
/// `P\tpart\tcontent type` per part, then `R\tpart\tid\ttype\ttarget\tI|E` per relationship of
/// that part (`E`: `target` is an external URL).
#[derive(Clone, Debug, Default)]
pub struct EmbeddedManifest {
    pub parts: BTreeMap<String, EmbeddedPart>,
}

/// Can `s` be a field of a manifest line?
pub fn manifest_field_ok(s: &str) -> bool {
    !s.is_empty() && !s.contains(['\t', '\r', '\n'])
}

impl EmbeddedManifest {
    pub fn parse(bytes: &[u8]) -> EmbeddedManifest {
        let mut m = EmbeddedManifest::default();
        for line in String::from_utf8_lossy(bytes).lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["P", part, ct] if m.parts.len() < MAX_EMBEDDED_PARTS => {
                    m.parts.entry((*part).to_string()).or_insert_with(|| EmbeddedPart { content_type: (*ct).to_string(), rels: Vec::new() });
                }
                ["R", part, id, kind, target, mode] => {
                    if let Some(p) = m.parts.get_mut(*part)
                        && p.rels.len() < MAX_EMBEDDED_RELS
                        && !p.rels.iter().any(|r| r.id == *id)
                    {
                        p.rels.push(Rel { id: (*id).to_string(), kind: (*kind).to_string(), target: (*target).to_string(), external: *mode == "E" });
                    }
                }
                _ => {}
            }
        }
        m
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = String::new();
        for (part, p) in &self.parts {
            if !manifest_field_ok(part) || !manifest_field_ok(&p.content_type) {
                continue;
            }
            s.push_str(&format!("P\t{part}\t{}\n", p.content_type));
        }
        for (part, p) in &self.parts {
            for r in p.rels.iter().filter(|r| [&r.id, &r.kind, &r.target].iter().all(|f| manifest_field_ok(f))) {
                s.push_str(&format!("R\t{part}\t{}\t{}\t{}\t{}\n", r.id, r.kind, r.target, if r.external { "E" } else { "I" }));
            }
        }
        s.into_bytes()
    }
}

/// Does relationship type `t` end with `suffix` (ignoring the transitional/strict prefix)?
pub fn rel_is(t: &str, full: &str) -> bool {
    let tail = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    t == full || tail(t) == tail(full)
}

/// The decompressed parts of a package (bounded).
pub struct Package {
    /// Shared, so a reader keeping a part (a picture, an opaque part) doesn't copy it.
    files: BTreeMap<String, Arc<Vec<u8>>>,
    /// Lowercased part name → the name it is stored under. OPC part names compare
    /// case-insensitively, so a package holds one part per lowercased name.
    lower: HashMap<String, String>,
    /// The zip entries that couldn't be read (corrupt data, a bad checksum): their names (at
    /// most [`MAX_SKIPPED_NAMES`]) and how many there were. They are missing from the document,
    /// so a save drops them: the reader reports them ([`UNREADABLE_PARTS`]).
    skipped: Vec<String>,
    skipped_total: usize,
}

impl Package {
    pub fn open(bytes: &[u8]) -> Result<Package, DocxError> {
        Package::open_limited(bytes, MAX_PART, MAX_TOTAL)
    }

    pub fn open_limited(bytes: &[u8], max_part: u64, max_total: u64) -> Result<Package, DocxError> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| DocxError::Zip(e.to_string()))?;
        if zip.len() > MAX_ENTRIES {
            return Err(DocxError::Limit(format!("{} zip entries", zip.len())));
        }
        let mut files = BTreeMap::new();
        let mut skipped = Vec::new();
        let mut skipped_total = 0usize;
        let mut skip = |name: &str| {
            skipped_total = skipped_total.saturating_add(1);
            if skipped.len() < MAX_SKIPPED_NAMES {
                skipped.push(report_name(name));
            }
        };
        let mut total: u64 = 0;
        // Compressed data ranges read so far (start → end): entries sharing or overlapping their
        // data would make one small stream inflate once per entry.
        let mut ranges: BTreeMap<u64, u64> = BTreeMap::new();
        for i in 0..zip.len() {
            // Named from the raw entry (nothing inflated) before the entry is opened, which borrows
            // the archive: a failed open reports it.
            let name = zip.by_index_raw(i).map_or_else(|_| format!("zip entry {}", i.saturating_add(1)), |raw| entry_name(raw.name_raw()));
            let mut f = match zip.by_index(i) {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("docx: skipping unreadable zip entry {name}: {e}");
                    skip(&name);
                    continue;
                }
            };
            if f.is_dir() {
                continue;
            }
            if f.size() > max_part {
                return Err(DocxError::Limit(format!("part {name} declares {} bytes", f.size())));
            }
            let start = local_data_start(bytes, f.header_start()).unwrap_or(f.header_start());
            let end = start.saturating_add(f.compressed_size());
            if end > start {
                let before = ranges.range(..=start).next_back().is_some_and(|(_, e)| *e > start);
                let after = ranges.range(start..).next().is_some_and(|(s, _)| *s < end);
                if before || after {
                    return Err(DocxError::Zip(format!("zip entry {name} overlaps another entry's data")));
                }
                ranges.insert(start, end);
            }
            let room = max_part.min(max_total.saturating_sub(total));
            let mut buf = Vec::with_capacity(f.size().min(16 * 1024 * 1024) as usize);
            let read = (&mut f).take(room + 1).read_to_end(&mut buf);
            // What was inflated counts against the total whether or not the entry turns out
            // readable (a bad checksum is only found at its end).
            total = total.saturating_add(buf.len() as u64);
            if buf.len() as u64 > room {
                return Err(DocxError::Limit(format!("part {name} decompresses past the size limit")));
            }
            if let Err(e) = read {
                // A corrupt entry: keep going unless it's a part we can't do without.
                log::warn!("docx: zip entry {name} unreadable: {e}");
                skip(&name);
                continue;
            }
            files.insert(name, buf);
        }
        let mut pkg = Package::from_parts(files);
        pkg.skipped = skipped;
        pkg.skipped_total = skipped_total;
        Ok(pkg)
    }

    /// The zip entries that couldn't be read, as reported ([`UNREADABLE_PARTS`]); `None` when
    /// every entry was read.
    pub fn unreadable(&self) -> Option<UnreadableParts> {
        (self.skipped_total > 0).then(|| UnreadableParts { names: self.skipped.clone(), total: self.skipped_total })
    }

    /// A package from parts already in memory (a Flat OPC file, a converted document), by part
    /// name without the leading `/`. Of parts whose names differ only in case, the first (in
    /// name order) is kept: they name the same part.
    pub fn from_parts(mut files: BTreeMap<String, Vec<u8>>) -> Package {
        let mut lower: HashMap<String, String> = HashMap::new();
        let mut dropped = Vec::new();
        for name in files.keys() {
            match lower.entry(name.to_ascii_lowercase()) {
                std::collections::hash_map::Entry::Occupied(_) => dropped.push(name.clone()),
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(name.clone());
                }
            }
        }
        for name in dropped {
            log::warn!("docx: dropping part {name}, a case variant of another part's name");
            files.remove(&name);
        }
        Package { files: files.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(), lower, skipped: Vec::new(), skipped_total: 0 }
    }

    /// The name part `name` is stored under (leading `/` ignored; case-insensitive), if any.
    /// Readers key their caches by it, so case variants of one name share an entry.
    pub fn canonical(&self, name: &str) -> Option<&str> {
        let n = name.trim_start_matches('/');
        if let Some((k, _)) = self.files.get_key_value(n) {
            return Some(k);
        }
        self.lower.get(&n.to_ascii_lowercase()).map(String::as_str)
    }

    /// Part bytes by name (leading `/` ignored; case-insensitive).
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.files.get(self.canonical(name)?).map(|b| b.as_slice())
    }

    /// [`Package::get`], shared rather than borrowed (for keeping a part in a document).
    pub fn shared(&self, name: &str) -> Option<Arc<Vec<u8>>> {
        self.files.get(self.canonical(name)?).cloned()
    }

    pub fn xml(&self, name: &str) -> Result<Option<El>, DocxError> {
        match self.get(name) {
            Some(b) => xml::parse(b).map(Some),
            None => Ok(None),
        }
    }

    /// Relationships of `part` (from `dir/_rels/name.rels`).
    pub fn rels(&self, part: &str) -> Rels {
        let (dir, file) = split_dir(part);
        let path = if dir.is_empty() { format!("_rels/{file}.rels") } else { format!("{dir}/_rels/{file}.rels") };
        let mut rels = Rels::default();
        let root = match self.xml(&path) {
            Ok(Some(root)) => root,
            Ok(None) => return rels,
            Err(e) => {
                log::warn!("docx: ignoring unreadable relationships {path}: {e}");
                return rels;
            }
        };
        for r in root.els().filter(|e| e.local() == "Relationship") {
            let (Some(id), Some(target)) = (r.attr("Id"), r.attr("Target")) else { continue };
            let external = r.attr("TargetMode").is_some_and(|m| m.eq_ignore_ascii_case("External"));
            let resolved = if external { target.to_string() } else { resolve(dir, target) };
            rels.list.push(Rel { id: id.to_string(), kind: r.attr("Type").unwrap_or("").to_string(), target: resolved, external });
        }
        rels
    }
}

/// A zip entry's part name from its raw bytes, without the leading `/`. OPC part names are
/// ASCII (non-ASCII percent-encoded) and producers write UTF-8, so the bytes are read as
/// UTF-8. (The raw name and the header offset, unlike the decoded name and data offset, have
/// kept their types across zip crate versions.)
fn entry_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).trim_start_matches('/').to_string()
}

/// Where the data of the zip entry whose local file header starts at `header` begins in the
/// archive `bytes`: past the header's 30 fixed bytes, its name and its extra field (PKWARE
/// APPNOTE §4.3.7). `None` if the header isn't in `bytes`.
fn local_data_start(bytes: &[u8], header: u64) -> Option<u64> {
    let at = usize::try_from(header).ok()?;
    let lens = bytes.get(at.checked_add(26)?..at.checked_add(30)?)?;
    let [n0, n1, e0, e1] = <[u8; 4]>::try_from(lens).ok()?;
    let skip = 30u64.saturating_add(u64::from(u16::from_le_bytes([n0, n1]))).saturating_add(u64::from(u16::from_le_bytes([e0, e1])));
    Some(header.saturating_add(skip))
}

/// A zip entry name fit for a report line: control characters replaced, at most
/// [`MAX_SKIPPED_NAME`] characters.
fn report_name(name: &str) -> String {
    let mut out: String = name.chars().take(MAX_SKIPPED_NAME).map(|c| if c.is_control() { '?' } else { c }).collect();
    if name.chars().nth(MAX_SKIPPED_NAME).is_some() {
        out.push('…');
    }
    out
}

/// The zip entries a file held that couldn't be read and so are missing from its document
/// (see [`UNREADABLE_PARTS`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnreadableParts {
    /// Their names, at most [`MAX_SKIPPED_NAMES`].
    pub names: Vec<String>,
    /// How many there were in all (at least `names.len()`).
    pub total: usize,
}

impl UnreadableParts {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = self.total.to_string();
        for n in &self.names {
            s.push('\n');
            s.push_str(n);
        }
        s.into_bytes()
    }

    pub fn parse(bytes: &[u8]) -> UnreadableParts {
        let text = String::from_utf8_lossy(bytes);
        let mut lines = text.lines();
        let total = lines.next().and_then(|t| t.trim().parse::<usize>().ok()).unwrap_or(0);
        let names: Vec<String> = lines.filter(|l| !l.is_empty()).take(MAX_SKIPPED_NAMES).map(report_name).collect();
        UnreadableParts { total: total.max(names.len()), names }
    }
}

/// Take the report of unreadable parts ([`UNREADABLE_PARTS`]) out of `doc`, for telling the
/// user when the file is opened. `None` when every part was read.
pub fn take_unreadable_parts(doc: &mut wordcraft_doc::Document) -> Option<UnreadableParts> {
    let r = UnreadableParts::parse(&doc.passthrough.remove(UNREADABLE_PARTS)?);
    (r.total > 0).then_some(r)
}

/// `[Content_Types].xml`: Override by part name, then Default by extension (both case-insensitive).
#[derive(Default)]
pub struct ContentTypes {
    overrides: HashMap<String, String>,
    defaults: HashMap<String, String>,
}

impl ContentTypes {
    pub fn read(pkg: &Package) -> ContentTypes {
        let mut ct = ContentTypes::default();
        let root = match pkg.xml("[Content_Types].xml") {
            Ok(Some(root)) => root,
            Ok(None) => return ct,
            Err(e) => {
                log::warn!("docx: ignoring unreadable [Content_Types].xml: {e}");
                return ct;
            }
        };
        for e in root.els() {
            let (Some(key), Some(t)) = (e.attr("PartName").or_else(|| e.attr("Extension")), e.attr("ContentType")) else { continue };
            let map = if e.local() == "Override" { &mut ct.overrides } else { &mut ct.defaults };
            map.entry(key.trim_start_matches('/').to_ascii_lowercase()).or_insert_with(|| t.to_string());
        }
        ct
    }

    /// Content type of `part` (a package path without the leading `/`).
    pub fn of(&self, part: &str) -> Option<&str> {
        let part = part.trim_start_matches('/').to_ascii_lowercase();
        if let Some(t) = self.overrides.get(&part) {
            return Some(t);
        }
        let ext = part.rsplit_once('.').map(|(_, e)| e)?;
        self.defaults.get(ext).map(String::as_str)
    }
}

#[derive(Clone, Debug)]
pub struct Rel {
    pub id: String,
    pub kind: String,
    /// Package path (internal) or URL (external).
    pub target: String,
    pub external: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Rels {
    pub list: Vec<Rel>,
}

impl Rels {
    pub fn by_id(&self, id: &str) -> Option<&Rel> {
        self.list.iter().find(|r| r.id == id)
    }
    pub fn by_type(&self, full: &str) -> Option<&Rel> {
        self.list.iter().find(|r| rel_is(&r.kind, full))
    }
}

fn split_dir(part: &str) -> (&str, &str) {
    let p = part.trim_start_matches('/');
    match p.rfind('/') {
        Some(i) => (p.get(..i).unwrap_or(""), p.get(i + 1..).unwrap_or(p)),
        None => ("", p),
    }
}

/// Resolve a relative target against a source directory into a package path.
pub fn resolve(dir: &str, target: &str) -> String {
    let target = target.split('#').next().unwrap_or(target);
    let mut parts: Vec<&str> = if target.starts_with('/') { Vec::new() } else { dir.split('/').filter(|s| !s.is_empty()).collect() };
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// The relative reference from part `from` to part `to` (both package paths), as a
/// relationship target: `word/document.xml` → `word/charts/chart1.xml` is `charts/chart1.xml`.
pub fn relative(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let from_dir = from_dir.get(..from_dir.len().saturating_sub(1)).unwrap_or(&[]);
    let to: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let to_dir = to.get(..to.len().saturating_sub(1)).unwrap_or(&[]);
    let common = from_dir.iter().zip(to_dir).take_while(|(a, b)| a == b).count();
    let mut out: Vec<&str> = vec![".."; from_dir.len() - common];
    out.extend(to.get(common..).unwrap_or(&[]));
    let s = out.join("/");
    // A relative reference whose first segment has a colon would read as a URI scheme.
    if s.split('/').next().is_some_and(|seg| seg.contains(':')) { format!("./{s}") } else { s }
}

/// Build a zip from (name, bytes) entries.
pub fn zip_entries(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>, DocxError> {
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).compression_level(Some(6));
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in entries {
        // Already-compressed media gain nothing from deflate.
        let lower = name.to_ascii_lowercase();
        let o = if [".png", ".jpg", ".jpeg", ".gif", ".webp"].iter().any(|e| lower.ends_with(e)) { stored } else { opts };
        zw.start_file(name.as_str(), o).map_err(|e| DocxError::Zip(e.to_string()))?;
        zw.write_all(bytes).map_err(|e| DocxError::Zip(e.to_string()))?;
    }
    let c = zw.finish().map_err(|e| DocxError::Zip(e.to_string()))?;
    Ok(c.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip_of(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let e: Vec<(String, Vec<u8>)> = entries.iter().map(|(n, b)| (n.to_string(), b.clone())).collect();
        zip_entries(&e).unwrap()
    }

    #[test]
    fn decompression_bomb_is_rejected() {
        let z = zip_of(&[("word/document.xml", vec![b' '; 2_000_000])]);
        assert!(z.len() < 100_000);
        assert!(matches!(Package::open_limited(&z, 1_000_000, 10_000_000), Err(DocxError::Limit(_))));
        assert!(matches!(Package::open_limited(&z, 10_000_000, 1_000_000), Err(DocxError::Limit(_))));
        assert!(Package::open_limited(&z, 10_000_000, 10_000_000).is_ok());
    }

    #[test]
    fn lying_declared_size_is_rejected_or_bounded() {
        let mut z = zip_of(&[("a.xml", b"<a/>".to_vec())]);
        // Patch the uncompressed size in the central directory and local header to ~4 GB.
        let mut patched = 0;
        for sig in [[0x50u8, 0x4B, 0x01, 0x02], [0x50, 0x4B, 0x03, 0x04]] {
            if let Some(pos) = z.windows(4).position(|w| w == sig) {
                let off = if sig[2] == 1 { pos + 24 } else { pos + 22 };
                z[off..off + 4].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
                patched += 1;
            }
        }
        assert_eq!(patched, 2);
        match Package::open(&z) {
            Err(_) => {}
            Ok(p) => assert!(p.get("a.xml").is_none_or(|b| b.len() < 1024)),
        }
    }

    /// Offset of the `n`th record with signature `sig` in `z`.
    fn record(z: &[u8], sig: [u8; 4], n: usize) -> usize {
        z.windows(4).enumerate().filter(|(_, w)| *w == sig).nth(n).map(|(i, _)| i).unwrap()
    }
    const LOCAL: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];
    const CENTRAL: [u8; 4] = [0x50, 0x4B, 0x01, 0x02];

    /// WC-23: an entry that inflates in full and then fails its checksum still counts against
    /// the total, so a package can't inflate the same bomb once per entry for free.
    #[test]
    fn unreadable_entries_count_against_the_total() {
        let mut z = zip_of(&[("a.xml", vec![b' '; 600_000]), ("b.xml", vec![b' '; 600_000])]);
        assert!(Package::open_limited(&z, 1_000_000, 1_000_000).is_err(), "both fit only separately");
        // Break a.xml's checksum (local header and central directory): it reads in full, then fails.
        for (pos, off) in [(record(&z, LOCAL, 0), 14), (record(&z, CENTRAL, 0), 16)] {
            z[pos + off] ^= 0xFF;
        }
        let skipped = Package::open_limited(&z, 1_000_000, 10_000_000).unwrap();
        assert!(skipped.get("a.xml").is_none() && skipped.get("b.xml").is_some());
        assert_eq!(skipped.unreadable(), Some(UnreadableParts { names: vec!["a.xml".into()], total: 1 }));
        assert!(matches!(Package::open_limited(&z, 1_000_000, 1_000_000), Err(DocxError::Limit(_))));
    }

    /// A minimal package whose `customXml/item1.xml` (first) and `word/media/x.bin` (second)
    /// are broken as `break_entry` says, with an intact main part after them.
    fn package_with_broken_parts(break_entry: impl Fn(&mut Vec<u8>, usize)) -> Vec<u8> {
        let main = br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Kept</w:t></w:r></w:p></w:body></w:document>"#;
        let mut z =
            zip_of(&[("customXml/item1.xml", vec![b'x'; 5_000]), ("word/media/x.bin", vec![b'y'; 5_000]), ("word/document.xml", main.to_vec())]);
        break_entry(&mut z, 0);
        break_entry(&mut z, 1);
        z
    }

    /// WC-23: a corrupt part doesn't stop the file opening, and the document reports it by name
    /// (it is missing, so a save drops it) instead of losing it silently.
    #[test]
    fn unreadable_parts_are_reported() {
        // A bad checksum (local header and central directory): the data inflates, then fails.
        let bad_crc = |z: &mut Vec<u8>, n: usize| {
            for (pos, off) in [(record(z, LOCAL, n), 14), (record(z, CENTRAL, n), 16)] {
                z[pos + off] ^= 0xFF;
            }
        };
        let mut doc = crate::read(&package_with_broken_parts(bad_crc)).unwrap();
        assert!(doc.plain_text(wordcraft_doc::StoryRef::Body).contains("Kept"));
        assert!(doc.passthrough.contains_key(UNREADABLE_PARTS));
        let u = take_unreadable_parts(&mut doc).unwrap();
        assert_eq!(u, UnreadableParts { names: vec!["customXml/item1.xml".into(), "word/media/x.bin".into()], total: 2 });
        assert!(!doc.passthrough.contains_key(UNREADABLE_PARTS), "taken out");
        // The report is bookkeeping, never a part of a saved file.
        let mut again = crate::read(&package_with_broken_parts(bad_crc)).unwrap();
        let saved = crate::write(&again).unwrap();
        assert!(!saved.windows(b"unreadable".len()).any(|w| w == b"unreadable"));
        assert!(take_unreadable_parts(&mut again).is_some());
        assert!(take_unreadable_parts(&mut crate::read(&saved).unwrap()).is_none(), "nothing unreadable in the saved file");

        // Truncated deflate data: the compressed size is shrunk, so the stream ends early.
        let truncated = |z: &mut Vec<u8>, n: usize| {
            for (pos, off) in [(record(z, LOCAL, n), 18), (record(z, CENTRAL, n), 20)] {
                z[pos + off..pos + off + 4].copy_from_slice(&4u32.to_le_bytes());
            }
        };
        let u = take_unreadable_parts(&mut crate::read(&package_with_broken_parts(truncated)).unwrap()).unwrap();
        assert_eq!(u.names, ["customXml/item1.xml", "word/media/x.bin"]);
    }

    /// WC-23: the data offset worked out from the local header is where the entry's bytes are.
    #[test]
    fn local_data_start_finds_the_entry_data() {
        let data = b"stored, not deflated".to_vec();
        let z = zip_of(&[("word/media/a.png", data.clone()), ("word/media/b.png", b"second".to_vec())]);
        let mut zip = zip::ZipArchive::new(Cursor::new(&z[..])).unwrap();
        for (i, want) in [(0, &data[..]), (1, &b"second"[..])] {
            let header = zip.by_index_raw(i).unwrap().header_start();
            let start = usize::try_from(local_data_start(&z, header).unwrap()).unwrap();
            assert_eq!(z.get(start..start + want.len()), Some(want));
        }
        assert_eq!(local_data_start(&z, z.len() as u64 - 10), None);
        assert_eq!(local_data_start(&z, u64::MAX), None);
        assert_eq!(entry_name(b"/word/document.xml"), "word/document.xml");
    }

    /// WC-23: the report keeps a bounded number of tidied names but counts every entry.
    #[test]
    fn unreadable_report_is_bounded() {
        let long = "n".repeat(MAX_SKIPPED_NAME + 50);
        let name = report_name(&format!("a\nb\t{long}"));
        assert!(name.starts_with("a?b?") && name.chars().count() == MAX_SKIPPED_NAME + 1 && name.ends_with('…'));
        let u = UnreadableParts { names: (0..MAX_SKIPPED_NAMES).map(|i| format!("p{i}")).collect(), total: 1_000 };
        assert_eq!(UnreadableParts::parse(&u.to_bytes()), u);
        let junk = UnreadableParts::parse(b"lots\n\n\xff\xfe");
        assert_eq!((junk.total, junk.names.len()), (1, 1));
        let mut lines = b"9".to_vec();
        for i in 0..MAX_SKIPPED_NAMES + 10 {
            lines.extend_from_slice(format!("\nq{i}").as_bytes());
        }
        let many = UnreadableParts::parse(&lines);
        assert_eq!((many.names.len(), many.total), (MAX_SKIPPED_NAMES, MAX_SKIPPED_NAMES));
    }

    /// WC-23: entries sharing (or overlapping) compressed data are refused, so one small stream
    /// can't be inflated once per entry.
    #[test]
    fn entries_sharing_data_are_refused() {
        let mut z = zip_of(&[("a.xml", vec![b' '; 10_000]), ("b.xml", vec![b' '; 10_000])]);
        assert!(Package::open(&z).is_ok());
        // Point b.xml's central directory entry at a.xml's local header.
        let pos = record(&z, CENTRAL, 1) + 42;
        z[pos..pos + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(Package::open(&z), Err(DocxError::Zip(_))));
    }

    /// WC-08: part names compare case-insensitively: case variants find the one stored part,
    /// and a package holding two spellings of one name keeps one.
    #[test]
    fn case_variant_names_are_one_part() {
        let mut files = BTreeMap::new();
        files.insert("word/media/Pic.png".to_string(), b"one".to_vec());
        files.insert("word/media/pic.PNG".to_string(), b"two".to_vec());
        files.insert("word/document.xml".to_string(), b"<x/>".to_vec());
        let p = Package::from_parts(files);
        assert_eq!(p.canonical("/WORD/MEDIA/PIC.png"), Some("word/media/Pic.png"));
        assert_eq!(p.get("word/media/pic.png"), Some(&b"one"[..]));
        assert_eq!(p.canonical("word/document.xml"), Some("word/document.xml"));
        assert_eq!(p.canonical("word/missing.xml"), None);
        assert_eq!(p.files.len(), 2);
        assert!(Arc::ptr_eq(&p.shared("word/media/PIC.PNG").unwrap(), &p.shared("word/media/Pic.png").unwrap()));
    }

    #[test]
    fn resolves_targets() {
        assert_eq!(resolve("word", "media/image1.png"), "word/media/image1.png");
        assert_eq!(resolve("word", "../customXml/item1.xml"), "customXml/item1.xml");
        assert_eq!(resolve("word", "/word/styles.xml"), "word/styles.xml");
        assert_eq!(resolve("", "word/document.xml"), "word/document.xml");
        assert_eq!(resolve("word", "../../../x"), "x");
    }

    #[test]
    fn relative_targets_resolve_back() {
        for (from, to, want) in [
            ("word/document.xml", "word/charts/chart1.xml", "charts/chart1.xml"),
            ("word/charts/chart1.xml", "word/embeddings/book.xlsx", "../embeddings/book.xlsx"),
            ("word/charts/chart1.xml", "word/charts/colors1.xml", "colors1.xml"),
            ("word/document.xml", "customXml/item1.xml", "../customXml/item1.xml"),
            ("word/document.xml", "word/a:b.bin", "./a:b.bin"),
        ] {
            let r = relative(from, to);
            assert_eq!(r, want);
            assert_eq!(resolve(from.rsplit_once('/').map_or("", |(d, _)| d), &r), to);
        }
    }

    #[test]
    fn embedded_manifest_round_trips_and_ignores_junk() {
        let mut m = EmbeddedManifest::default();
        let rel = Rel { id: "rId1".into(), kind: "k".into(), target: "word/embeddings/x.xlsx".into(), external: false };
        let url = Rel { id: "rId2".into(), kind: "k".into(), target: "https://example.com/x".into(), external: true };
        m.parts.insert("word/charts/chart1.xml".into(), EmbeddedPart { content_type: "ct".into(), rels: vec![rel, url] });
        let back = EmbeddedManifest::parse(&m.to_bytes());
        let p = &back.parts["word/charts/chart1.xml"];
        assert_eq!((p.content_type.as_str(), p.rels.len(), p.rels[1].external), ("ct", 2, true));
        let junk = EmbeddedManifest::parse(b"R\tnope\trId1\tk\tt\tI\nP\tonly-two\n\t\t\n\xff\xfe");
        assert!(junk.parts.is_empty());
    }
}
