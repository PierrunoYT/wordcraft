//! Bounds on what one hostile package can make the reader do (security audit, #2744): case
//! variants of one part name, and one heavy chart drawn at many sizes.

use std::io::Write;

use wordcraft_doc::para::InlineObject;
use wordcraft_doc::{Block, Document};

const W_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture""#;
const ROOT_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        zw.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
        zw.write_all(bytes).unwrap();
    }
    zw.finish().unwrap().into_inner()
}

/// A package with `body` and document relationships `(id, type, target)`, plus `extra` parts.
fn docx(body: &str, rels: &[(String, &str, String)], extra: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let list: String = rels.iter().map(|(id, ty, target)| format!(r#"<Relationship Id="{id}" Type="{REL}/{ty}" Target="{target}"/>"#)).collect();
    let mut entries = vec![
        ("_rels/.rels".to_string(), ROOT_RELS.as_bytes().to_vec()),
        ("word/document.xml".to_string(), format!("<w:document {W_NS}><w:body>{body}</w:body></w:document>").into_bytes()),
        (
            "word/_rels/document.xml.rels".to_string(),
            format!(r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{list}</Relationships>"#).into_bytes(),
        ),
    ];
    entries.extend(extra);
    zip(&entries)
}

/// `target` with the letters at the set bits of `n` upper-cased: a distinct spelling per `n`.
fn spelling(target: &str, n: usize) -> String {
    target.chars().enumerate().map(|(i, c)| if n >> (i % 16) & 1 == 1 { c.to_ascii_uppercase() } else { c }).collect()
}

fn graphics(d: &Document) -> Vec<std::sync::Arc<wordcraft_doc::graphic::Graphic>> {
    d.body
        .iter()
        .filter_map(|b| match &**b {
            Block::Para(p) => Some(p),
            _ => None,
        })
        .flat_map(|p| p.objects.iter())
        .filter_map(|o| match o {
            InlineObject::Graphic { graphic, .. } => Some(graphic.clone()),
            _ => None,
        })
        .collect()
}

/// WC-08: 64 relationships spelling one picture's name in different cases keep one picture,
/// shared with the package rather than copied per spelling, and saving writes one part.
#[test]
fn case_variant_targets_keep_one_picture() {
    let pic = |id: &str| {
        format!(
            r#"<w:r><w:drawing><wp:inline><wp:extent cx="12700" cy="12700"/><wp:docPr id="1" name="P"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:blipFill><a:blip r:embed="{id}"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
        )
    };
    let rels: Vec<(String, &str, String)> = (0..64).map(|n| (format!("rId{n}"), "image", spelling("media/image1.png", n))).collect();
    let body = format!("<w:p>{}</w:p>", rels.iter().map(|(id, _, _)| pic(id)).collect::<String>());
    let png = b"\x89PNG\r\n\x1a\n not really a png".to_vec();
    let d = wordcraft_docx::read(&docx(&body, &rels, vec![("word/media/image1.png".into(), png)])).unwrap();
    assert_eq!(d.media.len(), 1, "{:?}", d.media.keys().collect::<Vec<_>>());
    let pictures =
        d.body.iter().filter_map(|b| b.as_para()).flat_map(|p| p.objects.iter()).filter(|o| matches!(o, InlineObject::Image { .. })).count();
    assert_eq!(pictures, 64);
    let out = wordcraft_docx::write(&d).unwrap();
    let z = zip::ZipArchive::new(std::io::Cursor::new(out)).unwrap();
    let mut names: Vec<String> = z.file_names().map(str::to_ascii_lowercase).collect();
    let all = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), all, "case-duplicate parts written: {names:?}");
}

/// WC-08: a chart part reached through case variants of its name is kept (and written) once.
#[test]
fn case_variant_targets_keep_one_chart_part() {
    let chart = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:barChart><c:ser><c:val><c:numLit><c:ptCount val="1"/><c:pt idx="0"><c:v>1</c:v></c:pt></c:numLit></c:val></c:ser></c:barChart></c:plotArea></c:chart></c:chartSpace>"#;
    let frame = |id: &str| {
        format!(
            r#"<w:r><w:drawing><wp:inline><wp:extent cx="2743200" cy="1371600"/><wp:docPr id="1" name="C"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="{id}"/></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
        )
    };
    let rels: Vec<(String, &str, String)> = (0..16).map(|n| (format!("rId{n}"), "chart", spelling("charts/chart1.xml", n))).collect();
    let body = format!("<w:p>{}</w:p>", rels.iter().map(|(id, _, _)| frame(id)).collect::<String>());
    let d = wordcraft_docx::read(&docx(&body, &rels, vec![("word/charts/chart1.xml".into(), chart.as_bytes().to_vec())])).unwrap();
    let kept: Vec<&String> = d.passthrough.keys().filter(|k| k.to_ascii_lowercase().contains("chart1")).collect();
    assert!(kept.len() <= 1, "{kept:?}");
    let gs = graphics(&d);
    assert_eq!(gs.len(), 16);
    assert!(gs.iter().all(|g| !g.items.is_empty()));
    assert!(gs.iter().all(|g| std::sync::Arc::ptr_eq(g, &gs[0])), "one graphic for one part at one size");
}

/// WC-24: one heavy chart drawn at hundreds of sizes is built at most until the file's graphic
/// budget is spent (its input counts, not just what it draws); later frames are left empty.
#[test]
fn one_heavy_chart_at_many_sizes_spends_the_budget() {
    let pad = " ".repeat(1 << 20);
    let chart = format!(
        r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart><c:plotArea><c:barChart><c:ser><c:val><c:numLit><c:ptCount val="1"/><c:pt idx="0"><c:v>1</c:v></c:pt></c:numLit></c:val></c:ser></c:barChart></c:plotArea></c:chart>{pad}</c:chartSpace>"#
    );
    let frames: String = (0..300)
        .map(|i| {
            format!(
                r#"<w:r><w:drawing><wp:inline><wp:extent cx="{}" cy="1371600"/><wp:docPr id="1" name="C"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" r:id="rId1"/></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#,
                2_743_200 + i * 12_700
            )
        })
        .collect();
    let rels = [("rId1".to_string(), "chart", "charts/chart1.xml".to_string())];
    let d = wordcraft_docx::read(&docx(&format!("<w:p>{frames}</w:p>"), &rels, vec![("word/charts/chart1.xml".into(), chart.into_bytes())])).unwrap();
    let gs = graphics(&d);
    assert_eq!(gs.len(), 300);
    let drawn = gs.iter().filter(|g| !g.items.is_empty()).count();
    assert!(drawn > 0, "the first frames are drawn");
    assert!(drawn < 200, "{drawn} builds of a 1 MB chart");
}
