//! Multi-page PDF import for the reference setup: render each page to a
//! PNG at drawing-readable resolution so it lands in the dialog tray as an
//! ordinary image the user can assign (or ignore).
//!
//! Rendering is pure Rust (hayro), so native and wasm share this module;
//! callers pick the thread — the native pickers render off the UI thread,
//! wasm renders inline in the file-read callback.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, RenderCache, RenderSettings};

/// Target render resolution. 200 DPI keeps dimension text on architectural
/// sheets readable; the pixel cap keeps large sheet formats (A1/A0) inside
/// common GPU texture limits.
const TARGET_DPI: f32 = 200.0;
const MAX_SIDE_PX: f32 = 4096.0;

/// Does this file look like a PDF? The `%PDF-` header must appear near the
/// start (the spec tolerates leading junk within the first kilobyte).
pub fn is_pdf(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(1024)]
        .windows(5)
        .any(|w| w == b"%PDF-")
}

/// Render every page to a PNG and hand each to `deliver` as
/// `(name, png_bytes)` — one at a time, so pages can pop into the UI while
/// later ones are still rendering. A single-page PDF keeps the plain `stem`
/// name; multi-page pages get a ` p1`, ` p2`… suffix. Unreadable PDFs
/// (corrupt, encrypted) deliver nothing.
pub fn render_pages(stem: &str, bytes: Vec<u8>, mut deliver: impl FnMut(String, Vec<u8>)) {
    let Ok(pdf) = Pdf::new(bytes) else { return };
    let pages = pdf.pages();
    let cache = RenderCache::new();
    let interpreter = InterpreterSettings::default();
    for (index, page) in pages.iter().enumerate() {
        let (w_pt, h_pt) = page.render_dimensions();
        let long_side_pt = w_pt.max(h_pt).max(1.0);
        let scale = (TARGET_DPI / 72.0).min(MAX_SIDE_PX / long_side_pt);
        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE, // pages are paper: opaque white, not transparent
            ..Default::default()
        };
        let pixmap = render(page, &cache, &interpreter, &settings);
        let Ok(png) = pixmap.into_png() else { continue };
        let name = if pages.len() == 1 {
            stem.to_string()
        } else {
            format!("{stem} p{}", index + 1)
        };
        deliver(name, png);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid PDF with one blank page per `(w, h)` in points,
    /// computing the xref table offsets for real.
    fn minimal_pdf(pages: &[(u32, u32)]) -> Vec<u8> {
        let kids: Vec<String> = (0..pages.len()).map(|i| format!("{} 0 R", 3 + i)).collect();
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {} >>",
                kids.join(" "),
                pages.len()
            ),
        ];
        for (w, h) in pages {
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] >>"
            ));
        }

        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref_at = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn detects_pdf_headers() {
        assert!(is_pdf(b"%PDF-1.7 rest of file"));
        assert!(is_pdf(b"\xef\xbb\xbfjunk%PDF-1.4"), "header may sit past leading junk");
        assert!(!is_pdf(b"\x89PNG\r\n"));
        assert!(!is_pdf(b""));
    }

    #[test]
    fn single_page_keeps_the_plain_name() {
        // A4 in points; 200 DPI -> 595 * 200/72 = 1652 px wide
        let pdf = minimal_pdf(&[(595, 842)]);
        let mut delivered = Vec::new();
        render_pages("plan", pdf, |name, png| delivered.push((name, png)));
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, "plan");
        let size = image::load_from_memory(&delivered[0].1).unwrap();
        assert_eq!(size.width(), 1652);
        assert_eq!(size.height(), 2338);
    }

    #[test]
    fn multi_page_names_and_caps_huge_sheets() {
        // second page is A0 landscape: 3370 pt long side exceeds the cap
        let pdf = minimal_pdf(&[(595, 842), (3370, 2384)]);
        let mut delivered = Vec::new();
        render_pages("set", pdf, |name, png| delivered.push((name, png)));
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].0, "set p1");
        assert_eq!(delivered[1].0, "set p2");
        let big = image::load_from_memory(&delivered[1].1).unwrap();
        assert_eq!(big.width(), 4096, "long side clamps to the texture cap");
    }

    #[test]
    fn garbage_delivers_nothing() {
        let mut count = 0;
        render_pages("x", b"%PDF-1.4 but not actually a pdf".to_vec(), |_, _| count += 1);
        assert_eq!(count, 0);
    }
}
