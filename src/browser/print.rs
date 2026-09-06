//! Print / PDF export settings — pure, UI/engine-independent normalization
//! and filename suggestion for Issue #40.
//!
//! See docs/decisions.md D75 for the investigation this sits on top of: the
//! interactive "印刷" path (Ctrl/Cmd+P) is `wry::WebView::print()`, which
//! delegates to each OS's own native print dialog and needs no settings at
//! all (the dialog itself has UI for page range/paper size/orientation/
//! margins). Only the Windows-only *headless* PDF export path
//! (`ICoreWebView2_7::PrintToPdf`, see `ui::webview2_print`) actually
//! consumes [`PdfExportSettings`] — this module holds that value shape and
//! its validation so it can be unit-tested without any WebView2/COM
//! machinery, per `docs/architecture.md`'s `browser::`/`ui::` split.

/// Page orientation for a PDF export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

/// A named paper size, carrying its portrait-orientation dimensions in
/// inches — the unit `ICoreWebView2PrintSettings::PageWidth`/`PageHeight`
/// expects (Microsoft's WebView2 API is inch-based, unlike CSS's mm/px).
///
/// `A4` is VeloX's own default (see [`PdfExportSettings::default`]) rather
/// than `Letter` (WebView2's own native default): VeloX has no
/// locale-aware default yet (no settings-screen field for this either, see
/// D75's "見送ったもの"), and A4 is the more common size worldwide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaperSize {
    #[default]
    A4,
    Letter,
    Legal,
}

impl PaperSize {
    /// `(width, height)` in inches, in portrait orientation — swap them for
    /// landscape (see [`PdfExportSettings::page_dimensions_in`]).
    pub fn portrait_dimensions_in(self) -> (f64, f64) {
        match self {
            PaperSize::A4 => (8.27, 11.69),
            PaperSize::Letter => (8.5, 11.0),
            PaperSize::Legal => (8.5, 14.0),
        }
    }
}

/// Page margins, in inches, one independent value per edge (matching
/// `ICoreWebView2PrintSettings`'s four separate `MarginTop`/`MarginBottom`/
/// `MarginLeft`/`MarginRight` setters — there is no single "margin" knob to
/// mirror).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margins {
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
}

/// One inch on every edge — a plain, conservative default matching common
/// "Normal" print presets, not a value pulled from any particular OS.
impl Default for Margins {
    fn default() -> Self {
        Margins {
            top: 1.0,
            bottom: 1.0,
            left: 1.0,
            right: 1.0,
        }
    }
}

/// Smallest margin VeloX accepts (no negative margins — the underlying API
/// would likely reject them anyway, but this keeps the invalid-value
/// decision in pure, tested Rust rather than an untested COM round-trip).
pub const MIN_MARGIN_IN: f64 = 0.0;
/// Largest margin VeloX accepts — generous enough for any real print
/// layout while still catching a clearly-wrong value (e.g. a stray "36"
/// meant as millimeters, or points, fed in by a future settings UI).
pub const MAX_MARGIN_IN: f64 = 3.0;

impl Margins {
    /// Clamp every edge into `[MIN_MARGIN_IN, MAX_MARGIN_IN]`; a non-finite
    /// value (`NaN`/`inf`, which cannot occur from this module's own
    /// `Default` but could from a future settings-UI/IPC input) falls back
    /// to [`Margins::default`]'s value for that edge rather than being
    /// clamped (there is no meaningful "closest finite value" to a `NaN`).
    pub fn sanitize(&self) -> Margins {
        let fallback = Margins::default();
        Margins {
            top: sanitize_margin(self.top, fallback.top),
            bottom: sanitize_margin(self.bottom, fallback.bottom),
            left: sanitize_margin(self.left, fallback.left),
            right: sanitize_margin(self.right, fallback.right),
        }
    }
}

fn sanitize_margin(value: f64, fallback: f64) -> f64 {
    if value.is_finite() {
        value.clamp(MIN_MARGIN_IN, MAX_MARGIN_IN)
    } else {
        fallback
    }
}

/// Smallest/largest print scale factor VeloX accepts — matches the range
/// Microsoft documents for `ICoreWebView2PrintSettings::ScaleFactor`
/// (10%-200%).
pub const MIN_SCALE: f64 = 0.1;
pub const MAX_SCALE: f64 = 2.0;

/// Settings for the Windows-only headless PDF export
/// (`ICoreWebView2_7::PrintToPdf`, see `ui::webview2_print`). VeloX has no
/// settings-screen UI for these yet (Issue #40's scope is "make print/PDF
/// work at all" — see docs/decisions.md D75's "見送ったもの"), so every
/// export currently uses [`PdfExportSettings::default`]; the type and its
/// `sanitize` step exist regardless, ready for a future UI to feed
/// user-chosen values through the same validated path rather than trusting
/// raw IPC input straight into COM setters.
#[derive(Debug, Clone, PartialEq)]
pub struct PdfExportSettings {
    pub paper: PaperSize,
    pub orientation: Orientation,
    pub margins: Margins,
    /// `1.0` = 100%. See [`MIN_SCALE`]/[`MAX_SCALE`].
    pub scale: f64,
    /// Whether to include CSS backgrounds/background images — off by
    /// default, matching the common "printer-friendly" convention most
    /// browsers default to for a page's own background colors/images.
    pub print_backgrounds: bool,
}

impl Default for PdfExportSettings {
    fn default() -> Self {
        PdfExportSettings {
            paper: PaperSize::default(),
            orientation: Orientation::default(),
            margins: Margins::default(),
            scale: 1.0,
            print_backgrounds: false,
        }
    }
}

impl PdfExportSettings {
    /// Clamp every field into its valid range; a non-finite `scale` falls
    /// back to `1.0` (100%), the same "no meaningful closest value" reasoning
    /// [`Margins::sanitize`] uses.
    pub fn sanitize(&self) -> PdfExportSettings {
        let scale = if self.scale.is_finite() {
            self.scale.clamp(MIN_SCALE, MAX_SCALE)
        } else {
            1.0
        };
        PdfExportSettings {
            paper: self.paper,
            orientation: self.orientation,
            margins: self.margins.sanitize(),
            scale,
            print_backgrounds: self.print_backgrounds,
        }
    }

    /// `(width, height)` in inches, already swapped for `self.orientation` —
    /// what `ui::webview2_print` feeds straight into `SetPageWidth`/
    /// `SetPageHeight`.
    pub fn page_dimensions_in(&self) -> (f64, f64) {
        let (width, height) = self.paper.portrait_dimensions_in();
        match self.orientation {
            Orientation::Portrait => (width, height),
            Orientation::Landscape => (height, width),
        }
    }
}

/// Suggest a base filename (no directory, `.pdf` already appended) for a
/// page's PDF export, given its tab title (if any has arrived — see
/// `browser::tab::Tab::title`) and current URL. Deliberately **not** run
/// through `browser::downloads::sanitize_filename` here — that is the
/// caller's job (`app::save_active_tab_as_pdf`), mirroring the split
/// `browser::downloads` itself keeps between "what name to suggest" and
/// "is it filesystem-safe" (see that module's doc comment): this function
/// only ever needs to be honest about what page it names.
///
/// - A non-blank title (trimmed) wins.
/// - Otherwise the URL's host, when it parses as one — `example.com`, not
///   the full URL (which would carry a `.pdf`-hostile scheme/path).
/// - Otherwise (an unparseable URL, e.g. `about:blank`, or one with no
///   host) a fixed, generic name.
pub fn suggest_pdf_filename(title: Option<&str>, url: &str) -> String {
    let base = title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .or_else(|| host_of(url))
        .unwrap_or_else(|| "ページ".to_owned());
    format!("{base}.pdf")
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paper_sizes_have_distinct_portrait_dimensions() {
        assert_eq!(PaperSize::A4.portrait_dimensions_in(), (8.27, 11.69));
        assert_eq!(PaperSize::Letter.portrait_dimensions_in(), (8.5, 11.0));
        assert_eq!(PaperSize::Legal.portrait_dimensions_in(), (8.5, 14.0));
    }

    #[test]
    fn page_dimensions_swap_for_landscape() {
        let portrait = PdfExportSettings {
            orientation: Orientation::Portrait,
            paper: PaperSize::Letter,
            ..PdfExportSettings::default()
        };
        let landscape = PdfExportSettings {
            orientation: Orientation::Landscape,
            ..portrait.clone()
        };
        assert_eq!(portrait.page_dimensions_in(), (8.5, 11.0));
        assert_eq!(landscape.page_dimensions_in(), (11.0, 8.5));
    }

    #[test]
    fn default_settings_are_already_sanitized() {
        let default = PdfExportSettings::default();
        assert_eq!(default.sanitize(), default);
    }

    #[test]
    fn margins_sanitize_clamps_out_of_range_values() {
        let margins = Margins {
            top: -5.0,
            bottom: 100.0,
            left: 1.5,
            right: MAX_MARGIN_IN,
        };
        let sanitized = margins.sanitize();
        assert_eq!(sanitized.top, MIN_MARGIN_IN);
        assert_eq!(sanitized.bottom, MAX_MARGIN_IN);
        assert_eq!(sanitized.left, 1.5);
        assert_eq!(sanitized.right, MAX_MARGIN_IN);
    }

    #[test]
    fn margins_sanitize_replaces_non_finite_values_with_the_default() {
        let margins = Margins {
            top: f64::NAN,
            bottom: f64::INFINITY,
            left: f64::NEG_INFINITY,
            right: 0.5,
        };
        let sanitized = margins.sanitize();
        let fallback = Margins::default();
        assert_eq!(sanitized.top, fallback.top);
        assert_eq!(sanitized.bottom, fallback.bottom);
        assert_eq!(sanitized.left, fallback.left);
        assert_eq!(sanitized.right, 0.5);
    }

    #[test]
    fn scale_is_clamped_into_the_valid_range() {
        let too_small = PdfExportSettings {
            scale: 0.0,
            ..PdfExportSettings::default()
        };
        let too_large = PdfExportSettings {
            scale: 9.0,
            ..PdfExportSettings::default()
        };
        assert_eq!(too_small.sanitize().scale, MIN_SCALE);
        assert_eq!(too_large.sanitize().scale, MAX_SCALE);
    }

    #[test]
    fn non_finite_scale_falls_back_to_one_hundred_percent() {
        let settings = PdfExportSettings {
            scale: f64::NAN,
            ..PdfExportSettings::default()
        };
        assert_eq!(settings.sanitize().scale, 1.0);
    }

    #[test]
    fn suggest_pdf_filename_uses_the_title_when_present() {
        assert_eq!(
            suggest_pdf_filename(Some("Example Page"), "https://example.com/"),
            "Example Page.pdf"
        );
    }

    #[test]
    fn suggest_pdf_filename_trims_a_whitespace_only_title() {
        assert_eq!(
            suggest_pdf_filename(Some("   "), "https://example.com/path"),
            "example.com.pdf"
        );
    }

    #[test]
    fn suggest_pdf_filename_falls_back_to_the_host_with_no_title() {
        assert_eq!(
            suggest_pdf_filename(None, "https://example.com/path?x=1"),
            "example.com.pdf"
        );
    }

    #[test]
    fn suggest_pdf_filename_falls_back_to_a_generic_name_for_an_unparseable_url() {
        assert_eq!(suggest_pdf_filename(None, "about:blank"), "ページ.pdf");
        assert_eq!(suggest_pdf_filename(None, "not a url"), "ページ.pdf");
    }
}
