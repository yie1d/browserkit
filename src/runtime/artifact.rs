use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use cdpkit::dom::methods::GetBoxModel;
use cdpkit::page::methods::{CaptureScreenshot, GetLayoutMetrics, PrintToPdf};
use cdpkit::page::types::{CaptureScreenshotFormat, Viewport};

use super::{
    ActionCompletion, ArtifactFailure, BrowserError, Frame, Locator, OperationPhase, Page,
    PageSnapshot, SnapshotOptions,
};

const DEFAULT_MAX_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactDimensions {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactMetadata {
    pub encoded_bytes: usize,
    pub css_clip: Option<ArtifactClip>,
    pub full_page: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArtifactClip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Chrome-returned encoded artifact bytes retained by browserkit.
///
/// For screenshots, these are the raw bytes obtained by base64-decoding the
/// `Page.captureScreenshot` response after enforcing the configured encoded
/// byte budget. The MIME type is the requested format confirmed by a
/// recognizable format header. Image dimensions are header-derived only and
/// do not promise that the complete payload can be decoded. PDF artifacts are
/// checked only for the `%PDF-` signature and are not fully parsed.
///
/// The screenshot and PDF `max_bytes` options limit only the encoded
/// `Vec<u8>` retained by browserkit. They do not pre-limit the base64 JSON or
/// transport bytes already received by cdpkit.
#[derive(Clone)]
pub struct ArtifactBytes {
    bytes: Vec<u8>,
    mime_type: &'static str,
    dimensions: Option<ArtifactDimensions>,
    metadata: ArtifactMetadata,
}

impl std::fmt::Debug for ArtifactBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArtifactBytes")
            .field("mime_type", &self.mime_type)
            .field("dimensions", &self.dimensions)
            .field("metadata", &self.metadata)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .finish()
    }
}

impl ArtifactBytes {
    /// Returns Chrome's raw encoded bytes after base64 decoding and the
    /// configured retained-byte check.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
    /// Returns the requested MIME type after a recognizable matching header
    /// was observed. This does not imply complete decoding or parsing.
    pub fn mime_type(&self) -> &str {
        self.mime_type
    }
    /// Returns image dimensions read only from the recognized encoded header.
    ///
    /// `Some` does not guarantee that the complete image payload is decodable.
    pub fn dimensions(&self) -> Option<ArtifactDimensions> {
        self.dimensions
    }
    pub fn metadata(&self) -> &ArtifactMetadata {
        &self.metadata
    }

    /// Atomically makes all bytes returned by Chrome visible by replacing a
    /// regular target file from a temporary file in the selected parent.
    /// This does not add any format-integrity guarantee beyond this artifact's
    /// documented header checks. It is an atomic visibility guarantee, not a
    /// promise of power-loss durability on Windows.
    pub async fn save(&self, path: impl AsRef<Path>) -> Result<PathBuf, BrowserError> {
        save_atomically(
            path.as_ref().to_owned(),
            self.bytes.clone(),
            prepare_atomic_save,
        )
        .await
    }

    fn new(
        bytes: Vec<u8>,
        mime_type: &'static str,
        dimensions: Option<ArtifactDimensions>,
        css_clip: Option<ArtifactClip>,
        full_page: bool,
    ) -> Self {
        Self {
            metadata: ArtifactMetadata {
                encoded_bytes: bytes.len(),
                css_clip,
                full_page,
            },
            bytes,
            mime_type,
            dimensions,
        }
    }
}

async fn save_atomically<Prepare>(
    path: PathBuf,
    bytes: Vec<u8>,
    prepare: Prepare,
) -> Result<PathBuf, BrowserError>
where
    Prepare: FnOnce(PathBuf, Vec<u8>) -> Result<PreparedAtomicSave, BrowserError> + Send + 'static,
{
    let prepared = tokio::task::spawn_blocking(move || prepare(path, bytes))
        .await
        .map_err(|error| {
            BrowserError::operation("save artifact", OperationPhase::Preparation)
                .with_message(format!("artifact save task failed: {error}"))
        })??;
    // No await is allowed after this point: persist is the explicit,
    // non-cancellable atomic commit point.
    commit_atomic_save(prepared)
}

struct PreparedAtomicSave {
    temporary: tempfile::NamedTempFile,
    path: PathBuf,
    parent: PathBuf,
    #[cfg(test)]
    cleanup_probe: Option<SaveCleanupProbe>,
}

#[cfg(test)]
struct SaveCleanupProbe(Option<tokio::sync::oneshot::Sender<()>>);

#[cfg(test)]
impl Drop for SaveCleanupProbe {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

fn prepare_atomic_save(path: PathBuf, bytes: Vec<u8>) -> Result<PreparedAtomicSave, BrowserError> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_owned();
    let file_name = path
        .file_name()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            artifact_error(
                ArtifactFailure::InvalidPath,
                "artifact path must name a file",
            )
        })?
        .to_owned();
    if !parent.is_dir() {
        return Err(artifact_error(
            ArtifactFailure::InvalidPath,
            "artifact parent must already exist and be a directory",
        ));
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(artifact_error(
                ArtifactFailure::InvalidPath,
                "artifact target must be a regular file, not a directory or symbolic link",
            ));
        }
    }

    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(".{}.", file_name.to_string_lossy()))
        .tempfile_in(&parent)
        .map_err(|error| {
            BrowserError::io_operation(
                "create artifact temporary file",
                OperationPhase::Preparation,
                error,
            )
        })?;
    temporary
        .write_all(&bytes)
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| {
            BrowserError::io_operation(
                "write artifact temporary file",
                OperationPhase::Dispatch,
                error,
            )
        })?;
    Ok(PreparedAtomicSave {
        temporary,
        path,
        parent,
        #[cfg(test)]
        cleanup_probe: None,
    })
}

fn commit_atomic_save(prepared: PreparedAtomicSave) -> Result<PathBuf, BrowserError> {
    commit_atomic_save_with(prepared, sync_parent_directory)
}

fn commit_atomic_save_with(
    prepared: PreparedAtomicSave,
    after_persist: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<PathBuf, BrowserError> {
    let PreparedAtomicSave {
        temporary,
        path,
        parent,
        #[cfg(test)]
            cleanup_probe: _cleanup_probe,
    } = prepared;
    temporary.persist(&path).map_err(|error| {
        BrowserError::io_operation(
            "replace artifact file",
            OperationPhase::Dispatch,
            error.error,
        )
        .with_action_completion(ActionCompletion::NotStarted)
    })?;
    after_persist(&parent).map_err(|error| {
        BrowserError::io_operation("sync artifact parent", OperationPhase::Cleanup, error)
            .with_action_completion(ActionCompletion::Completed)
    })?;
    Ok(path)
}

fn sync_parent_directory(parent: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let directory = std::fs::File::open(parent)?;
        directory.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScreenshotFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
}

impl ScreenshotFormat {
    fn cdp(self) -> CaptureScreenshotFormat {
        match self {
            Self::Png => CaptureScreenshotFormat::Png,
            Self::Jpeg => CaptureScreenshotFormat::Jpeg,
            Self::Webp => CaptureScreenshotFormat::Webp,
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScreenshotOptions {
    format: ScreenshotFormat,
    quality: Option<u8>,
    full_page: bool,
    max_bytes: usize,
}

impl Default for ScreenshotOptions {
    fn default() -> Self {
        Self {
            format: ScreenshotFormat::Png,
            quality: None,
            full_page: false,
            max_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
        }
    }
}

impl ScreenshotOptions {
    pub fn format(mut self, format: ScreenshotFormat) -> Self {
        self.format = format;
        self
    }
    pub fn quality(mut self, quality: u8) -> Self {
        self.quality = Some(quality);
        self
    }
    pub fn full_page(mut self, full_page: bool) -> Self {
        self.full_page = full_page;
        self
    }
    /// Sets the maximum encoded-image bytes retained by browserkit.
    ///
    /// This is not a pre-transport limit: cdpkit has already received the
    /// base64 JSON response before browserkit can enforce this budget.
    pub fn max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }
    pub fn max_byte_size(&self) -> usize {
        self.max_bytes
    }
}

#[derive(Clone, Debug)]
pub struct PdfOptions {
    print_background: bool,
    landscape: bool,
    max_bytes: usize,
}
impl Default for PdfOptions {
    fn default() -> Self {
        Self {
            print_background: false,
            landscape: false,
            max_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
        }
    }
}
impl PdfOptions {
    pub fn print_background(mut self, value: bool) -> Self {
        self.print_background = value;
        self
    }
    pub fn landscape(mut self, value: bool) -> Self {
        self.landscape = value;
        self
    }
    /// Sets the maximum PDF format bytes retained by browserkit.
    ///
    /// This is not a pre-transport limit on cdpkit's base64 JSON response.
    pub fn max_bytes(mut self, value: usize) -> Self {
        self.max_bytes = value;
        self
    }
}

#[derive(Clone, Debug)]
pub struct HtmlOptions {
    max_bytes: usize,
}
impl Default for HtmlOptions {
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024,
        }
    }
}
impl HtmlOptions {
    pub fn max_bytes(mut self, value: usize) -> Self {
        self.max_bytes = value;
        self
    }
}

#[derive(Clone, Debug)]
pub struct HtmlArtifact {
    inner: ArtifactBytes,
}
impl HtmlArtifact {
    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(self.inner.as_bytes()).expect("HTML artifact is UTF-8")
    }
    pub fn artifact(&self) -> &ArtifactBytes {
        &self.inner
    }
    pub async fn save(&self, path: impl AsRef<Path>) -> Result<PathBuf, BrowserError> {
        self.inner.save(path).await
    }
}

#[derive(Clone, Debug)]
pub struct AccessibilityArtifact {
    snapshot: PageSnapshot,
}
impl AccessibilityArtifact {
    pub fn snapshot(&self) -> &PageSnapshot {
        &self.snapshot
    }
    pub fn into_snapshot(self) -> PageSnapshot {
        self.snapshot
    }
}

pub(crate) async fn screenshot_page(
    page: &Page,
    options: ScreenshotOptions,
) -> Result<ArtifactBytes, BrowserError> {
    validate_screenshot_options(&options, true)?;
    let _operation = page.admit_operation("capture page screenshot")?;
    let document = page.lifecycle().snapshot();
    let clip = if options.full_page {
        let metrics = GetLayoutMetrics::new()
            .send(page.cdp_session())
            .await
            .map_err(|error| {
                BrowserError::cdp_operation(
                    "read page layout for screenshot",
                    OperationPhase::Observation,
                    error,
                )
            })?;
        let size = metrics.css_content_size;
        Some(checked_clip(size.x, size.y, size.width, size.height)?)
    } else {
        None
    };
    let artifact = capture(page.cdp_session(), &options, clip).await?;
    validate_document(page, document, "capture page screenshot")?;
    Ok(artifact)
}

pub(crate) async fn screenshot_locator(
    locator: &Locator,
    options: ScreenshotOptions,
) -> Result<ArtifactBytes, BrowserError> {
    validate_screenshot_options(&options, false)?;
    let page = locator.page();
    let operation = page.admit_operation("capture locator screenshot")?;
    let resolved = super::action::resolve_locator_after_scroll(locator, &operation).await?;
    let store = page.locator_frame_store(&operation).await?;
    let geometry = super::geometry::Geometry::for_route(page, store, &resolved.route)?;
    let model = GetBoxModel::new()
        .with_backend_node_id(resolved.backend_node_id)
        .send(&resolved.session)
        .await
        .map_err(|error| {
            BrowserError::cdp_operation(
                "read element box for screenshot",
                OperationPhase::Observation,
                error,
            )
        })?
        .model;
    let source = super::geometry::Quad::<super::geometry::SessionViewport>::try_from_slice(
        &model.border,
        "map locator screenshot geometry",
    )
    .map_err(artifact_geometry_error)?;
    let mapped = geometry
        .map_session_quad_to_top_page(source, "map locator screenshot geometry")
        .await
        .map_err(artifact_geometry_error)?;
    mapped
        .fence
        .validate("confirm locator screenshot geometry")
        .await
        .map_err(artifact_geometry_error)?;
    super::geometry::ensure_axis_aligned(mapped.quad, "prepare locator screenshot clip")
        .map_err(artifact_geometry_error)?;
    let bounds = mapped
        .quad
        .bounds("prepare locator screenshot clip")
        .map_err(artifact_geometry_error)?;
    let clip = checked_clip(bounds.x, bounds.y, bounds.width, bounds.height)?;
    let capture_session = page.cdp_session().clone();
    let artifact = capture(&capture_session, &options, Some(clip)).await?;
    mapped
        .fence
        .validate("confirm locator screenshot geometry")
        .await
        .map_err(|error| {
            artifact_geometry_error(error).with_action_completion(ActionCompletion::Completed)
        })?;
    locator
        .validate_scope()
        .await
        .map_err(|error| error.with_action_completion(ActionCompletion::Completed))?;
    Ok(artifact)
}

pub(crate) async fn screenshot_frame(
    frame: &Frame,
    options: ScreenshotOptions,
) -> Result<ArtifactBytes, BrowserError> {
    validate_screenshot_options(&options, false)?;
    let page = frame.page();
    let operation = page.admit_operation("capture frame screenshot")?;
    let geometry = super::geometry::Geometry::for_frame(frame, &operation).await?;
    let mapped = geometry
        .map_frame_viewport_to_top_page("map frame screenshot geometry")
        .await
        .map_err(artifact_geometry_error)?;

    mapped
        .fence
        .validate("confirm frame screenshot geometry")
        .await
        .map_err(artifact_geometry_error)?;
    super::geometry::ensure_axis_aligned(mapped.quad, "prepare frame screenshot clip")
        .map_err(artifact_geometry_error)?;
    let bounds = mapped
        .quad
        .bounds("prepare frame screenshot clip")
        .map_err(artifact_geometry_error)?;
    let clip = checked_clip(bounds.x, bounds.y, bounds.width, bounds.height)?;
    let capture_session = page.cdp_session().clone();
    let artifact = capture(&capture_session, &options, Some(clip)).await?;
    mapped
        .fence
        .validate("confirm frame screenshot geometry")
        .await
        .map_err(|error| {
            artifact_geometry_error(error).with_action_completion(ActionCompletion::Completed)
        })?;
    frame
        .validate_locator_scope()
        .await
        .map_err(|error| error.with_action_completion(ActionCompletion::Completed))?;
    Ok(artifact)
}

async fn capture(
    session: &cdpkit::Session,
    options: &ScreenshotOptions,
    clip: Option<ArtifactClip>,
) -> Result<ArtifactBytes, BrowserError> {
    let mut method = CaptureScreenshot::new()
        .with_format(options.format.cdp())
        .with_from_surface(true);
    if let Some(quality) = options.quality {
        method = method.with_quality(i64::from(quality));
    }
    if let Some(value) = clip {
        method = method
            .with_clip(Viewport {
                x: value.x,
                y: value.y,
                width: value.width,
                height: value.height,
                scale: 1.0,
            })
            .with_capture_beyond_viewport(true);
    }
    let response = method.send(session).await.map_err(|error| {
        BrowserError::cdp_operation("capture screenshot", OperationPhase::Dispatch, error)
            .with_action_completion(ActionCompletion::Unknown)
    })?;
    let bytes = decode_bounded(&response.data, options.max_bytes, "screenshot")
        .map_err(|error| error.with_action_completion(ActionCompletion::Completed))?;
    let dimensions = image_dimensions(&bytes, options.format).ok_or_else(|| {
        artifact_error(
            ArtifactFailure::InvalidData,
            "Chrome returned an unrecognized or mismatched screenshot header",
        )
        .with_action_completion(ActionCompletion::Completed)
    })?;
    Ok(ArtifactBytes::new(
        bytes,
        options.format.mime(),
        Some(dimensions),
        clip,
        options.full_page,
    ))
}

pub(crate) async fn pdf(page: &Page, options: PdfOptions) -> Result<ArtifactBytes, BrowserError> {
    if options.max_bytes == 0 {
        return Err(artifact_error(
            ArtifactFailure::InvalidOptions,
            "PDF max_bytes must be greater than zero",
        ));
    }
    let status = *page.capabilities().status(super::Capability::Pdf);
    if status.availability() == super::CapabilityAvailability::Unavailable {
        let reason = status
            .reason()
            .expect("an unavailable capability must include a typed reason");
        return Err(BrowserError::configuration(
            "print page PDF",
            super::ConfigurationFailure::UnsupportedCapability {
                capability: status.capability(),
                reason,
            },
        ));
    }
    let _operation = page.admit_operation("print page PDF")?;
    let document = page.lifecycle().snapshot();
    let response = PrintToPdf::new().with_print_background(options.print_background).with_landscape(options.landscape).send(page.cdp_session()).await.map_err(|error| BrowserError::cdp_operation("print page PDF (requires a target where Chrome supports Page.printToPDF, normally headless Chrome)", OperationPhase::Dispatch, error).with_action_completion(ActionCompletion::Unknown))?;
    let bytes = decode_bounded(&response.data, options.max_bytes, "PDF")
        .map_err(|error| error.with_action_completion(ActionCompletion::Completed))?;
    if !bytes.starts_with(b"%PDF-") {
        return Err(artifact_error(
            ArtifactFailure::InvalidData,
            "Chrome returned an unrecognized or mismatched PDF header",
        )
        .with_action_completion(ActionCompletion::Completed));
    }
    validate_document(page, document, "print page PDF")?;
    Ok(ArtifactBytes::new(
        bytes,
        "application/pdf",
        None,
        None,
        true,
    ))
}

pub(crate) async fn page_html(
    page: &Page,
    options: HtmlOptions,
) -> Result<HtmlArtifact, BrowserError> {
    validate_html_options(&options)?;
    let _operation = page.admit_operation("capture page HTML")?;
    let document = page.lifecycle().snapshot();
    let html: String = page
        .evaluate("document.documentElement ? document.documentElement.outerHTML : ''")
        .await?;
    validate_document(page, document, "capture page HTML")?;
    html_artifact(html, options)
}

pub(crate) async fn frame_html(
    frame: &Frame,
    options: HtmlOptions,
) -> Result<HtmlArtifact, BrowserError> {
    validate_html_options(&options)?;
    frame.validate_locator_scope().await?;
    let html: String = frame
        .evaluate("document.documentElement ? document.documentElement.outerHTML : ''")
        .await?;
    frame
        .validate_locator_scope()
        .await
        .map_err(|error| error.with_action_completion(ActionCompletion::Completed))?;
    html_artifact(html, options)
}

fn html_artifact(html: String, options: HtmlOptions) -> Result<HtmlArtifact, BrowserError> {
    validate_html_options(&options)?;
    if html.len() > options.max_bytes {
        return Err(artifact_error(
            ArtifactFailure::TooLarge {
                max_bytes: options.max_bytes,
                observed_bytes: html.len(),
            },
            "HTML artifact exceeds max_bytes",
        )
        .with_action_completion(ActionCompletion::Completed));
    }
    Ok(HtmlArtifact {
        inner: ArtifactBytes::new(
            html.into_bytes(),
            "text/html; charset=utf-8",
            None,
            None,
            false,
        ),
    })
}

fn validate_html_options(options: &HtmlOptions) -> Result<(), BrowserError> {
    if options.max_bytes == 0 {
        return Err(artifact_error(
            ArtifactFailure::InvalidOptions,
            "HTML max_bytes must be greater than zero",
        ));
    }
    Ok(())
}

pub(crate) async fn accessibility(
    page: &Page,
    options: SnapshotOptions,
) -> Result<AccessibilityArtifact, BrowserError> {
    Ok(accessibility_from_snapshot(page.snapshot(options).await?))
}

fn accessibility_from_snapshot(snapshot: PageSnapshot) -> AccessibilityArtifact {
    AccessibilityArtifact { snapshot }
}

fn validate_screenshot_options(
    options: &ScreenshotOptions,
    allow_full_page: bool,
) -> Result<(), BrowserError> {
    if options.max_bytes == 0 {
        return Err(artifact_error(
            ArtifactFailure::InvalidOptions,
            "screenshot max_bytes must be greater than zero",
        ));
    }
    if options.full_page && !allow_full_page {
        return Err(artifact_error(
            ArtifactFailure::InvalidOptions,
            "full_page is only valid for Page screenshots",
        ));
    }
    if options.format == ScreenshotFormat::Png && options.quality.is_some() {
        return Err(artifact_error(
            ArtifactFailure::InvalidOptions,
            "quality is only valid for JPEG and WebP screenshots",
        ));
    }
    if options.quality.is_some_and(|quality| quality > 100) {
        return Err(artifact_error(
            ArtifactFailure::InvalidOptions,
            "screenshot quality must be between 0 and 100",
        ));
    }
    Ok(())
}

fn checked_clip(x: f64, y: f64, width: f64, height: f64) -> Result<ArtifactClip, BrowserError> {
    if ![x, y, width, height].into_iter().all(f64::is_finite) || width <= 0.0 || height <= 0.0 {
        return Err(artifact_error(
            ArtifactFailure::EmptyRegion,
            "screenshot region is empty or non-finite",
        ));
    }
    if width * height > 268_435_456.0 {
        return Err(artifact_error(
            ArtifactFailure::RegionTooLarge,
            "screenshot region exceeds 268,435,456 CSS pixels",
        ));
    }
    Ok(ArtifactClip {
        x,
        y,
        width,
        height,
    })
}

fn standard_base64_decoded_len(encoded: &str) -> Option<usize> {
    fn sextet(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes = encoded.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let padding = if bytes.ends_with(b"==") {
        2
    } else if bytes.ends_with(b"=") {
        1
    } else {
        0
    };
    let data_len = bytes.len().checked_sub(padding)?;
    if bytes[..data_len].iter().any(|byte| sextet(*byte).is_none())
        || bytes[data_len..].iter().any(|byte| *byte != b'=')
    {
        return None;
    }
    let canonical_tail = match padding {
        0 => true,
        1 => sextet(*bytes.get(bytes.len().checked_sub(2)?)?)? & 0b11 == 0,
        2 => sextet(*bytes.get(bytes.len().checked_sub(3)?)?)? & 0b1111 == 0,
        _ => false,
    };
    if !canonical_tail {
        return None;
    }
    bytes
        .len()
        .checked_div(4)?
        .checked_mul(3)?
        .checked_sub(padding)
}

fn decode_bounded(
    encoded: &str,
    max_bytes: usize,
    kind: &'static str,
) -> Result<Vec<u8>, BrowserError> {
    if let Some(decoded_len) = standard_base64_decoded_len(encoded) {
        if decoded_len > max_bytes {
            return Err(artifact_error(
                ArtifactFailure::TooLarge {
                    max_bytes,
                    observed_bytes: decoded_len,
                },
                format!("{kind} base64 payload exceeds max_bytes"),
            ));
        }
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| {
            artifact_error(
                ArtifactFailure::InvalidData,
                format!("Chrome returned invalid base64 for {kind}"),
            )
        })?;
    if bytes.len() > max_bytes {
        return Err(artifact_error(
            ArtifactFailure::TooLarge {
                max_bytes,
                observed_bytes: bytes.len(),
            },
            format!("{kind} exceeds max_bytes"),
        ));
    }
    Ok(bytes)
}

fn artifact_geometry_error(error: BrowserError) -> BrowserError {
    let failure = if error.to_string().contains("outside")
        || error.to_string().contains("non-positive")
        || error.to_string().contains("degenerate")
    {
        ArtifactFailure::EmptyRegion
    } else {
        ArtifactFailure::InvalidData
    };
    error.with_artifact_failure(failure)
}

fn artifact_error(failure: ArtifactFailure, message: impl Into<String>) -> BrowserError {
    BrowserError::operation("produce artifact", OperationPhase::Observation)
        .with_message(message)
        .with_artifact_failure(failure)
}

fn validate_document(
    page: &Page,
    expected: super::LifecycleSnapshot,
    operation: &'static str,
) -> Result<(), BrowserError> {
    page.lifecycle()
        .validate_document(expected)
        .map_err(|reason| {
            BrowserError::operation(operation, OperationPhase::Confirmation)
                .with_message(format!(
                    "artifact document changed during capture: {reason:?}"
                ))
                .with_action_completion(ActionCompletion::Completed)
        })
}

fn image_dimensions(bytes: &[u8], format: ScreenshotFormat) -> Option<ArtifactDimensions> {
    match format {
        ScreenshotFormat::Png => png_dimensions(bytes),
        ScreenshotFormat::Jpeg => jpeg_dimensions(bytes),
        ScreenshotFormat::Webp => webp_dimensions(bytes),
    }
}

fn png_dimensions(bytes: &[u8]) -> Option<ArtifactDimensions> {
    if bytes.get(..8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let length = u32::from_be_bytes(bytes.get(8..12)?.try_into().ok()?) as usize;
    if length != 13 || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    let data = bytes.get(16..29)?;
    let expected_crc = u32::from_be_bytes(bytes.get(29..33)?.try_into().ok()?);
    if png_crc32(b"IHDR", data) != expected_crc {
        return None;
    }
    let width = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?);
    let height = u32::from_be_bytes(data.get(4..8)?.try_into().ok()?);
    let bit_depth = *data.get(8)?;
    let color_type = *data.get(9)?;
    let valid_depth = match color_type {
        0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
        2 | 4 | 6 => matches!(bit_depth, 8 | 16),
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        _ => false,
    };
    if width == 0
        || height == 0
        || !valid_depth
        || data.get(10) != Some(&0)
        || data.get(11) != Some(&0)
        || !matches!(data.get(12), Some(0 | 1))
    {
        return None;
    }
    Some(ArtifactDimensions { width, height })
}

fn png_crc32(chunk_type: &[u8], data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in chunk_type.iter().chain(data) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<ArtifactDimensions> {
    if bytes.get(..2)? != [0xff, 0xd8] {
        return None;
    }
    let mut offset = 2_usize;
    while offset < bytes.len() {
        if *bytes.get(offset)? != 0xff {
            return None;
        }
        while bytes.get(offset) == Some(&0xff) {
            offset = offset.checked_add(1)?;
        }
        let marker = *bytes.get(offset)?;
        offset = offset.checked_add(1)?;
        if marker == 0xd9 {
            return None;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if marker == 0x00 || marker == 0xd8 {
            return None;
        }
        let length = u16::from_be_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?)
            as usize;
        if length < 2 {
            return None;
        }
        let segment_end = offset.checked_add(length)?;
        if segment_end > bytes.len() {
            return None;
        }
        if is_jpeg_sof(marker) {
            if length < 8 {
                return None;
            }
            let data = bytes.get(offset..segment_end)?;
            let components = *data.get(7)? as usize;
            let expected_length = 8_usize.checked_add(components.checked_mul(3)?)?;
            let precision = *data.get(2)?;
            if components == 0 || length != expected_length || !matches!(precision, 8 | 12) {
                return None;
            }
            let height = u16::from_be_bytes([*data.get(3)?, *data.get(4)?]) as u32;
            let width = u16::from_be_bytes([*data.get(5)?, *data.get(6)?]) as u32;
            return (width != 0 && height != 0).then_some(ArtifactDimensions { width, height });
        }
        if marker == 0xda {
            return None;
        }
        offset = segment_end;
    }
    None
}

fn is_jpeg_sof(marker: u8) -> bool {
    matches!(
        marker,
        0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf
    )
}

fn webp_dimensions(bytes: &[u8]) -> Option<ArtifactDimensions> {
    if bytes.get(..4)? != b"RIFF" || bytes.get(8..12)? != b"WEBP" {
        return None;
    }
    let declared_end =
        (u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?) as usize).checked_add(8)?;
    if declared_end < 20 {
        return None;
    }
    let mut offset = 12_usize;
    loop {
        let chunk_type_end = offset.checked_add(4)?;
        let chunk_header_end = offset.checked_add(8)?;
        if chunk_header_end > bytes.len() || chunk_header_end > declared_end {
            return None;
        }
        let chunk_type = bytes.get(offset..chunk_type_end)?;
        let length = u32::from_le_bytes(
            bytes
                .get(chunk_type_end..chunk_header_end)?
                .try_into()
                .ok()?,
        ) as usize;
        let data_offset = chunk_header_end;
        let data_end = data_offset.checked_add(length)?;
        let declared_next = data_end.checked_add(length & 1)?;
        if declared_next > declared_end {
            return None;
        }
        let required = match chunk_type {
            b"VP8X" => 10,
            b"VP8 " => 10,
            b"VP8L" => 5,
            _ => 0,
        };
        if required != 0 {
            if length < required {
                return None;
            }
            let header_end = data_offset.checked_add(required)?;
            let header = bytes.get(data_offset..header_end)?;
            return webp_image_dimensions(chunk_type, header);
        }
        if declared_next > bytes.len() {
            return None;
        }
        offset = declared_next;
    }
}

fn read_le24(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes([
        *bytes.first()?,
        *bytes.get(1)?,
        *bytes.get(2)?,
        0,
    ]))
}

fn webp_image_dimensions(chunk_type: &[u8], header: &[u8]) -> Option<ArtifactDimensions> {
    let dimensions = match chunk_type {
        b"VP8X" if header.len() >= 10 && header[0] & 0xc1 == 0 && header[1..4] == [0, 0, 0] => {
            ArtifactDimensions {
                width: read_le24(header.get(4..7)?)?.checked_add(1)?,
                height: read_le24(header.get(7..10)?)?.checked_add(1)?,
            }
        }
        b"VP8 " if header.len() >= 10 && header[3..6] == [0x9d, 0x01, 0x2a] => {
            let frame_tag =
                u32::from(header[0]) | (u32::from(header[1]) << 8) | (u32::from(header[2]) << 16);
            if frame_tag & 1 != 0 {
                return None;
            }
            ArtifactDimensions {
                width: u16::from_le_bytes([header[6], header[7]]) as u32 & 0x3fff,
                height: u16::from_le_bytes([header[8], header[9]]) as u32 & 0x3fff,
            }
        }
        b"VP8L" if header.len() >= 5 && header[0] == 0x2f => {
            let bits = u32::from_le_bytes(header[1..5].try_into().ok()?);
            if bits >> 29 != 0 {
                return None;
            }
            ArtifactDimensions {
                width: (bits & 0x3fff) + 1,
                height: ((bits >> 14) & 0x3fff) + 1,
            }
        }
        _ => return None,
    };
    (dimensions.width != 0 && dimensions.height != 0).then_some(dimensions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::sync::{Arc, Weak};
    use tokio_tungstenite::tungstenite::Message;

    fn encoded_test_png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&vec![0; width as usize * height as usize * 3])
            .unwrap();
        writer.finish().unwrap();
        bytes
    }

    fn riff_chunk(chunk_type: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(chunk_type);
        chunk.extend_from_slice(&(data.len() as u32).to_le_bytes());
        chunk.extend_from_slice(data);
        if data.len() & 1 != 0 {
            chunk.push(0);
        }
        chunk
    }

    fn webp_container(chunks: &[Vec<u8>]) -> Vec<u8> {
        let payload_length = 4_usize + chunks.iter().map(Vec::len).sum::<usize>();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(payload_length as u32).to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        for chunk in chunks {
            bytes.extend_from_slice(chunk);
        }
        bytes
    }

    fn encoded_test_webp() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode("UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==")
            .unwrap()
    }

    fn structural_jpeg(entropy: &[u8]) -> Vec<u8> {
        let mut bytes = vec![
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x02, 0x00, 0x03, 0x01, 0x01, 0x11,
            0x00, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00,
        ];
        bytes.extend_from_slice(entropy);
        bytes.extend_from_slice(&[0xff, 0xd9]);
        bytes
    }

    #[test]
    fn bounded_base64_uses_exact_decoded_lengths_for_every_padding_shape() {
        for (encoded, decoded) in [("QUJD", b"ABC".as_slice()), ("QUI=", b"AB"), ("QQ==", b"A")] {
            assert_eq!(standard_base64_decoded_len(encoded), Some(decoded.len()));
            assert_eq!(
                decode_bounded(encoded, decoded.len(), "test").unwrap(),
                decoded
            );

            let max_bytes = decoded.len() - 1;
            let error = decode_bounded(encoded, max_bytes, "test").unwrap_err();
            assert_eq!(
                error.artifact_failure(),
                Some(&ArtifactFailure::TooLarge {
                    max_bytes,
                    observed_bytes: decoded.len(),
                })
            );
        }
    }

    #[test]
    fn malformed_base64_is_reported_as_invalid_data_before_any_size_result() {
        for encoded in ["AAA", "A=AA", "AAA===", "AB==", "AA/="] {
            assert_eq!(standard_base64_decoded_len(encoded), None, "{encoded}");
            let error = decode_bounded(encoded, 0, "test").unwrap_err();
            assert_eq!(
                error.artifact_failure(),
                Some(&ArtifactFailure::InvalidData),
                "{encoded}"
            );
        }
    }

    #[test]
    fn screenshot_quality_boundaries_and_empty_budgets_are_validated() {
        let error = validate_screenshot_options(&ScreenshotOptions::default().quality(50), true)
            .unwrap_err();
        assert_eq!(
            error.artifact_failure(),
            Some(&ArtifactFailure::InvalidOptions)
        );
        let error = validate_screenshot_options(&ScreenshotOptions::default().max_bytes(0), true)
            .unwrap_err();
        assert_eq!(
            error.artifact_failure(),
            Some(&ArtifactFailure::InvalidOptions)
        );
        for format in [ScreenshotFormat::Jpeg, ScreenshotFormat::Webp] {
            for quality in [0, 100] {
                validate_screenshot_options(
                    &ScreenshotOptions::default().format(format).quality(quality),
                    true,
                )
                .unwrap();
            }
            for quality in [101, u8::MAX] {
                let error = validate_screenshot_options(
                    &ScreenshotOptions::default().format(format).quality(quality),
                    true,
                )
                .unwrap_err();
                assert_eq!(
                    error.artifact_failure(),
                    Some(&ArtifactFailure::InvalidOptions)
                );
            }
        }
    }

    #[test]
    fn html_zero_budget_is_invalid_but_oversize_is_a_completed_capture() {
        let zero = html_artifact(String::new(), HtmlOptions::default().max_bytes(0)).unwrap_err();
        assert_eq!(
            zero.artifact_failure(),
            Some(&ArtifactFailure::InvalidOptions)
        );
        assert_eq!(zero.action_completed(), ActionCompletion::NotStarted);

        let oversize =
            html_artifact("too large".to_owned(), HtmlOptions::default().max_bytes(3)).unwrap_err();
        assert_eq!(
            oversize.artifact_failure(),
            Some(&ArtifactFailure::TooLarge {
                max_bytes: 3,
                observed_bytes: 9,
            })
        );
        assert_eq!(oversize.action_completed(), ActionCompletion::Completed);
    }

    #[tokio::test]
    async fn save_replaces_files_and_never_accepts_a_directory() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("artifact.bin");
        std::fs::write(&target, b"old").unwrap();
        let artifact = ArtifactBytes::new(
            b"new".to_vec(),
            "application/octet-stream",
            None,
            None,
            false,
        );
        artifact.save(&target).await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let error = artifact.save(directory.path()).await.unwrap_err();
        assert_eq!(
            error.artifact_failure(),
            Some(&ArtifactFailure::InvalidPath)
        );
    }

    #[tokio::test]
    async fn cancelling_save_before_persist_preserves_destination_and_cleans_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("artifact.bin");
        std::fs::write(&target, b"old").unwrap();
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
        let save_target = target.clone();
        let save = tokio::spawn(async move {
            save_atomically(save_target, b"new".to_vec(), move |path, bytes| {
                let mut prepared = prepare_atomic_save(path, bytes)?;
                let temporary_path = prepared.temporary.path().to_owned();
                prepared.cleanup_probe = Some(SaveCleanupProbe(Some(cleaned_tx)));
                prepared_tx.send(temporary_path).unwrap();
                release_rx.recv().unwrap();
                Ok(prepared)
            })
            .await
        });

        let temporary_path = prepared_rx.await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(temporary_path.exists());
        save.abort();
        assert!(save.await.unwrap_err().is_cancelled());
        release_tx.send(()).unwrap();
        cleaned_rx.await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!temporary_path.exists());
    }

    #[test]
    fn persist_failure_is_not_started_but_post_persist_failure_is_completed() {
        let not_started_directory = tempfile::tempdir().unwrap();
        let not_started_target = not_started_directory.path().join("occupied");
        let not_started_prepared =
            prepare_atomic_save(not_started_target.clone(), b"new".to_vec()).unwrap();
        let not_started_temporary = not_started_prepared.temporary.path().to_owned();
        std::fs::create_dir(&not_started_target).unwrap();
        let not_started = commit_atomic_save(not_started_prepared).unwrap_err();
        assert_eq!(not_started.action_completed(), ActionCompletion::NotStarted);
        assert!(not_started_target.is_dir());
        assert!(!not_started_temporary.exists());

        let completed_directory = tempfile::tempdir().unwrap();
        let completed_target = completed_directory.path().join("artifact.bin");
        std::fs::write(&completed_target, b"old").unwrap();
        let completed_prepared =
            prepare_atomic_save(completed_target.clone(), b"new".to_vec()).unwrap();
        let completed = commit_atomic_save_with(completed_prepared, |_| {
            Err(std::io::Error::other("injected directory sync failure"))
        })
        .unwrap_err();
        assert_eq!(completed.action_completed(), ActionCompletion::Completed);
        assert_eq!(std::fs::read(completed_target).unwrap(), b"new");
    }

    #[test]
    fn image_header_recognition_rejects_wrong_or_truncated_headers_and_zero_dimensions() {
        let png = encoded_test_png(3, 2);
        let mut bad_ihdr_crc = png.clone();
        bad_ihdr_crc[29] ^= 0x01;
        assert_eq!(png_dimensions(&bad_ihdr_crc), None);
        assert_eq!(png_dimensions(&png[..32]), None, "truncated IHDR");
        assert_eq!(
            png_dimensions(&png[..33]),
            Some(ArtifactDimensions {
                width: 3,
                height: 2
            }),
            "bytes after a recognized IHDR are intentionally not decoded"
        );
        let mut zero_width_png = png[..33].to_vec();
        zero_width_png[16..20].copy_from_slice(&0_u32.to_be_bytes());
        let crc = png_crc32(&zero_width_png[12..16], &zero_width_png[16..29]);
        zero_width_png[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(png_dimensions(&zero_width_png), None);

        let jpeg = structural_jpeg(&[1]);
        assert_eq!(
            jpeg_dimensions(&jpeg[..15]),
            Some(ArtifactDimensions {
                width: 3,
                height: 2
            }),
            "SOF dimensions do not imply a complete JPEG scan"
        );
        assert_eq!(jpeg_dimensions(&jpeg[..10]), None, "truncated SOF");
        let mut zero_width_jpeg = jpeg[..15].to_vec();
        zero_width_jpeg[9] = 0;
        zero_width_jpeg[10] = 0;
        assert_eq!(jpeg_dimensions(&zero_width_jpeg), None);

        let vp8x = webp_container(&[riff_chunk(b"VP8X", &[0, 0, 0, 0, 2, 0, 0, 1, 0, 0])]);
        assert_eq!(
            webp_dimensions(&vp8x),
            Some(ArtifactDimensions {
                width: 3,
                height: 2
            })
        );
        assert_eq!(webp_dimensions(&vp8x[..21]), None, "truncated VP8X header");
        let vp8x_reserved_flags =
            webp_container(&[riff_chunk(b"VP8X", &[0x80, 0, 0, 0, 2, 0, 0, 1, 0, 0])]);
        assert_eq!(
            webp_dimensions(&vp8x_reserved_flags),
            None,
            "VP8X reserved flag bits must be zero"
        );

        let vp8 = webp_container(&[riff_chunk(
            b"VP8 ",
            &[0, 0, 0, 0x9d, 0x01, 0x2a, 3, 0, 2, 0],
        )]);
        assert_eq!(
            webp_dimensions(&vp8),
            Some(ArtifactDimensions {
                width: 3,
                height: 2
            })
        );
        assert_eq!(webp_dimensions(&vp8[..21]), None, "truncated VP8 header");
        let zero_width_vp8 = webp_container(&[riff_chunk(
            b"VP8 ",
            &[0, 0, 0, 0x9d, 0x01, 0x2a, 0, 0, 2, 0],
        )]);
        assert_eq!(webp_dimensions(&zero_width_vp8), None);

        let vp8l_header = [0x2f, 0x02, 0x40, 0, 0];
        let vp8l = webp_container(&[riff_chunk(b"JUNK", &[1]), riff_chunk(b"VP8L", &vp8l_header)]);
        assert_eq!(
            webp_dimensions(&vp8l),
            Some(ArtifactDimensions {
                width: 3,
                height: 2
            }),
            "unknown chunks and their padding are walked before VP8L"
        );
        assert_eq!(
            webp_dimensions(&vp8l[..vp8l.len() - 2]),
            None,
            "truncated VP8L header"
        );

        let mut undersized_riff = vp8x.clone();
        undersized_riff[4..8].copy_from_slice(&11_u32.to_le_bytes());
        assert_eq!(webp_dimensions(&undersized_riff), None);
        assert_eq!(webp_dimensions(b"not a webp header"), None);
    }

    #[test]
    fn requested_format_must_match_a_recognizable_dimension_header() {
        let png = encoded_test_png(320, 200);
        let jpeg = structural_jpeg(&[1]);
        let webp = encoded_test_webp();
        assert_eq!(
            image_dimensions(&png, ScreenshotFormat::Png),
            Some(ArtifactDimensions {
                width: 320,
                height: 200,
            })
        );
        assert_eq!(
            image_dimensions(&jpeg, ScreenshotFormat::Jpeg),
            Some(ArtifactDimensions {
                width: 3,
                height: 2
            })
        );
        assert_eq!(
            image_dimensions(&webp, ScreenshotFormat::Webp),
            Some(ArtifactDimensions {
                width: 1,
                height: 1
            })
        );
        assert_eq!(image_dimensions(&png, ScreenshotFormat::Jpeg), None);
        assert_eq!(image_dimensions(&jpeg, ScreenshotFormat::Webp), None);
        assert_eq!(image_dimensions(&webp, ScreenshotFormat::Png), None);
    }

    #[test]
    fn accessibility_artifact_reuses_the_structured_page_snapshot() {
        let snapshot = PageSnapshot {
            main_frame_id: "main".to_owned(),
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
            load_state: super::super::DocumentLoadState::Complete,
            visible_text: "ready".to_owned(),
            elements: Vec::new(),
            focus: None,
            viewport: super::super::ViewportSnapshot {
                width: 800.0,
                height: 600.0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                document_width: 800.0,
                document_height: 600.0,
            },
            frames: Vec::new(),
            truncation: super::super::SnapshotTruncation::default(),
        };
        let artifact = accessibility_from_snapshot(snapshot.clone());
        assert_eq!(artifact.snapshot(), &snapshot);
        assert_eq!(artifact.into_snapshot(), snapshot);
    }

    async fn fake_artifact_page() -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        fake_artifact_page_with_pdf(b"%PDF-1.7\n%%EOF".to_vec()).await
    }

    async fn fake_artifact_page_with_pdf(
        pdf_payload: Vec<u8>,
    ) -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        fake_artifact_page_with_payloads(pdf_payload, encoded_test_png(320, 200)).await
    }

    async fn fake_artifact_page_with_payloads(
        pdf_payload: Vec<u8>,
        screenshot_payload: Vec<u8>,
    ) -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        use crate::runtime::{BrowserRuntime, BrowserSessionId, PageOwnership};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let commands = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_commands = Arc::clone(&commands);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                server_commands.lock().push(command.clone());
                let id = command["id"].as_u64().unwrap();
                let result = match command["method"].as_str().unwrap() {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                    "Page.getLayoutMetrics" => json!({
                        "layoutViewport": {"pageX": 0, "pageY": 0, "clientWidth": 80, "clientHeight": 60},
                        "visualViewport": {"offsetX": 0.0, "offsetY": 0.0, "pageX": 0.0, "pageY": 0.0, "clientWidth": 80.0, "clientHeight": 60.0, "scale": 1.0},
                        "contentSize": {"x": 0.0, "y": 0.0, "width": 320.0, "height": 200.0},
                        "cssLayoutViewport": {"pageX": 0, "pageY": 0, "clientWidth": 80, "clientHeight": 60},
                        "cssVisualViewport": {"offsetX": 0.0, "offsetY": 0.0, "pageX": 0.0, "pageY": 0.0, "clientWidth": 80.0, "clientHeight": 60.0, "scale": 1.0},
                        "cssContentSize": {"x": 0.0, "y": 0.0, "width": 320.0, "height": 200.0}
                    }),
                    "Page.captureScreenshot" => {
                        json!({"data": base64::engine::general_purpose::STANDARD.encode(&screenshot_payload)})
                    }
                    "Page.printToPDF" => {
                        json!({"data": base64::engine::general_purpose::STANDARD.encode(&pdf_payload)})
                    }
                    "Page.enable" | "Target.setAutoAttach" => json!({}),
                    "Page.getFrameTree" => json!({
                        "frameTree": {
                            "frame": {
                                "id": "main", "loaderId": "loader-main", "url": "about:blank",
                                "domainAndRegistry": "", "securityOrigin": "null", "mimeType": "text/html",
                                "secureContextType": "InsecureScheme",
                                "crossOriginIsolatedContextType": "NotIsolated", "gatedAPIFeatures": []
                            }
                        }
                    }),
                    other => panic!("unexpected artifact command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let runtime = BrowserRuntime::connect(format!("ws://{address}"))
            .await
            .unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner"),
            Weak::new(),
            "target-artifact".into(),
            PageOwnership::Attached,
            runtime.cdp().session("artifact-session"),
        );
        (page, commands)
    }

    #[tokio::test]
    async fn full_page_screenshot_uses_css_content_clip_without_mutating_viewport() {
        let (page, commands) = fake_artifact_page().await;
        commands.lock().clear();
        let artifact = page
            .screenshot(ScreenshotOptions::default().full_page(true))
            .await
            .unwrap();
        assert_eq!(artifact.mime_type(), "image/png");
        assert_eq!(
            artifact.dimensions(),
            Some(ArtifactDimensions {
                width: 320,
                height: 200
            })
        );
        let commands = commands.lock();
        let capture = commands
            .iter()
            .find(|command| command["method"] == "Page.captureScreenshot")
            .unwrap();
        assert_eq!(capture["params"]["clip"]["width"], 320.0);
        assert_eq!(capture["params"]["clip"]["height"], 200.0);
        assert_eq!(capture["params"]["captureBeyondViewport"], true);
        assert_eq!(
            commands
                .iter()
                .map(|command| command["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["Page.getLayoutMetrics", "Page.captureScreenshot"]
        );
    }

    #[tokio::test]
    async fn frame_and_locator_full_page_fail_before_artifact_dispatch() {
        let (page, commands) = fake_artifact_page().await;
        let frame = page.main_frame().await.unwrap();
        commands.lock().clear();

        let frame_error = frame
            .screenshot(ScreenshotOptions::default().full_page(true))
            .await
            .unwrap_err();
        assert_eq!(
            frame_error.artifact_failure(),
            Some(&ArtifactFailure::InvalidOptions)
        );
        assert!(commands.lock().is_empty());

        let locator_error = page
            .locator("html")
            .screenshot(ScreenshotOptions::default().full_page(true))
            .await
            .unwrap_err();
        assert_eq!(
            locator_error.artifact_failure(),
            Some(&ArtifactFailure::InvalidOptions)
        );
        assert!(commands.lock().is_empty());
    }

    #[tokio::test]
    async fn pdf_reports_signature_and_respects_byte_limit() {
        let (page, _) = fake_artifact_page().await;
        let artifact = page.pdf(PdfOptions::default()).await.unwrap();
        assert_eq!(artifact.mime_type(), "application/pdf");
        assert!(artifact.as_bytes().starts_with(b"%PDF-"));
        let error = page
            .pdf(PdfOptions::default().max_bytes(4))
            .await
            .unwrap_err();
        assert!(matches!(
            error.artifact_failure(),
            Some(ArtifactFailure::TooLarge { max_bytes: 4, .. })
        ));
    }

    #[tokio::test]
    async fn invalid_quality_and_zero_html_budget_fail_before_dispatch() {
        let (page, commands) = fake_artifact_page().await;
        commands.lock().clear();
        let quality = page
            .screenshot(
                ScreenshotOptions::default()
                    .format(ScreenshotFormat::Jpeg)
                    .quality(101),
            )
            .await
            .unwrap_err();
        assert_eq!(
            quality.artifact_failure(),
            Some(&ArtifactFailure::InvalidOptions)
        );
        let html = page
            .html(HtmlOptions::default().max_bytes(0))
            .await
            .unwrap_err();
        assert_eq!(
            html.artifact_failure(),
            Some(&ArtifactFailure::InvalidOptions)
        );
        assert!(commands.lock().is_empty());
    }

    #[tokio::test]
    async fn post_response_document_fence_is_completed() {
        let (page, _) = fake_artifact_page().await;
        let document = page.lifecycle().snapshot();
        page.lifecycle().commit_new_document();

        let error = validate_document(&page, document, "capture page screenshot").unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
    }

    #[tokio::test]
    async fn screenshot_rejects_invalid_payload_as_completed_after_dispatch() {
        let mut pseudo_png = vec![0; 24];
        pseudo_png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        pseudo_png[16..20].copy_from_slice(&320_u32.to_be_bytes());
        pseudo_png[20..24].copy_from_slice(&200_u32.to_be_bytes());
        let (page, commands) =
            fake_artifact_page_with_payloads(b"%PDF-1.7\n%%EOF".to_vec(), pseudo_png).await;
        commands.lock().clear();

        let error = page
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap_err();

        assert_eq!(
            error.artifact_failure(),
            Some(&ArtifactFailure::InvalidData)
        );
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Page.captureScreenshot")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn pdf_rejects_invalid_magic_after_dispatch() {
        let (page, commands) = fake_artifact_page_with_pdf(b"not a PDF".to_vec()).await;
        commands.lock().clear();
        let error = page.pdf(PdfOptions::default()).await.unwrap_err();
        assert_eq!(
            error.artifact_failure(),
            Some(&ArtifactFailure::InvalidData)
        );
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert_eq!(
            commands
                .lock()
                .iter()
                .map(|command| command["method"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>(),
            vec!["Page.printToPDF"]
        );
    }

    async fn fake_oopif_artifact_page() -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        fake_oopif_artifact_page_with_route_stale(false, false, false, false, false, false).await
    }

    async fn fake_oopif_artifact_page_with_stale_after_capture(
        stale_after_capture: bool,
    ) -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        fake_oopif_artifact_page_with_route_stale(
            false,
            stale_after_capture,
            false,
            false,
            false,
            false,
        )
        .await
    }

    async fn fake_oopif_artifact_page_with_transformed_owner(
    ) -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        fake_oopif_artifact_page_with_route_stale(false, false, true, false, false, false).await
    }

    async fn fake_oopif_artifact_page_with_invisible_locator(
    ) -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        fake_oopif_artifact_page_with_route_stale(false, false, false, true, false, false).await
    }

    async fn fake_oopif_artifact_page_with_lineage_change(
        before_capture: bool,
        after_capture: bool,
    ) -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        fake_oopif_artifact_page_with_route_stale(
            false,
            false,
            false,
            false,
            before_capture,
            after_capture,
        )
        .await
    }

    async fn fake_oopif_artifact_page_with_route_stale(
        stale_before_capture: bool,
        stale_after_capture: bool,
        transformed_top_owner: bool,
        remain_offscreen_after_scroll: bool,
        lineage_stale_before_capture: bool,
        lineage_stale_after_capture: bool,
    ) -> (Page, Arc<parking_lot::Mutex<Vec<Value>>>) {
        use crate::runtime::{BrowserRuntime, BrowserSessionId, PageOwnership};

        fn frame(id: &str, loader_id: &str, parent_id: Option<&str>) -> Value {
            let mut value = json!({
                "id": id,
                "loaderId": loader_id,
                "url": format!("https://{id}.test/"),
                "domainAndRegistry": "test",
                "securityOrigin": format!("https://{id}.test"),
                "mimeType": "text/html",
                "secureContextType": "Secure",
                "crossOriginIsolatedContextType": "NotIsolated",
                "gatedAPIFeatures": []
            });
            if let Some(parent_id) = parent_id {
                value["parentId"] = json!(parent_id);
            }
            value
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let commands = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server_commands = Arc::clone(&commands);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let websocket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut write, mut read) = websocket.split();
            let mut outer_attached = false;
            let mut inner_attached = false;
            let mut capture_sent = false;
            let mut frame_geometry_read = false;
            let mut locator_scrolled = false;
            while let Some(Ok(Message::Text(text))) = read.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                server_commands.lock().push(command.clone());
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                let session_id = command.get("sessionId").and_then(Value::as_str);
                let (layout_width, layout_height, visual_width, visual_height) =
                    if session_id == Some("oopif-inner") {
                        (40, 30, 40.0, 30.0)
                    } else if session_id == Some("artifact-top") {
                        (500, 400, 400.0, 300.0)
                    } else if session_id == Some("oopif-child") {
                        // A child target can report the outer renderer viewport rather than
                        // the iframe's 80x60 content aperture. Geometry must use facts from
                        // the child's exact main-world execution context instead.
                        (800, 600, 800.0, 600.0)
                    } else {
                        (80, 60, 80.0, 60.0)
                    };
                let (page_x, page_y) = if session_id == Some("artifact-top") {
                    (7.0, 11.0)
                } else {
                    (0.0, 0.0)
                };
                let result = match method {
                    "Browser.getVersion" => crate::runtime::test_browser_version_result(),
                    "Target.getBrowserContexts" => json!({"browserContextIds": []}),
                    "Page.enable"
                    | "Target.setAutoAttach"
                    | "Runtime.enable"
                    | "Runtime.setAsyncCallStackDepth" => json!({}),
                    "Page.getFrameTree" if session_id == Some("oopif-inner") => json!({
                        "frameTree": {"frame": frame("inner", "loader-inner", Some("same"))}
                    }),
                    "Page.getFrameTree" if session_id == Some("oopif-child") => json!({
                        "frameTree": {
                            "frame": frame(
                                "child",
                                if (stale_before_capture && frame_geometry_read)
                                    || (stale_after_capture && capture_sent)
                                {
                                    "loader-child-replaced"
                                } else {
                                    "loader-child"
                                },
                                if (lineage_stale_before_capture && frame_geometry_read)
                                    || (lineage_stale_after_capture && capture_sent)
                                {
                                    Some("unrelated-parent")
                                } else {
                                    Some("main")
                                }
                            ),
                            "childFrames": [{
                                "frame": frame("same", "loader-same", Some("child")),
                                "childFrames": [{"frame": frame("inner", "loader-inner", Some("same"))}]
                            }]
                        }
                    }),
                    "Page.getFrameTree" => json!({
                        "frameTree": {
                            "frame": frame("main", "loader-main", None)
                        }
                    }),
                    "Page.createIsolatedWorld" => json!({"executionContextId": 91}),
                    "Runtime.evaluate" if command["params"].get("uniqueContextId").is_some() => {
                        let (inner_width, inner_height) = if session_id == Some("oopif-inner") {
                            (40.0, 30.0)
                        } else if session_id == Some("oopif-child") {
                            (80.0, 60.0)
                        } else {
                            (400.0, 300.0)
                        };
                        json!({
                            "result": {"type": "object", "value": {
                                "innerWidth": inner_width,
                                "innerHeight": inner_height,
                                "scrollX": 0.0,
                                "scrollY": if locator_scrolled { 120.0 } else { 0.0 },
                                "visualOffsetLeft": 0.0,
                                "visualOffsetTop": 0.0,
                                "visualPageLeft": 0.0,
                                "visualPageTop": if locator_scrolled { 120.0 } else { 0.0 },
                                "visualWidth": inner_width,
                                "visualHeight": inner_height,
                                "visualScale": 1.0
                            }}
                        })
                    }
                    "Runtime.evaluate" => json!({
                        "result": {
                            "type": "object", "subtype": "node",
                            "className": "HTMLDivElement", "description": "div#blue",
                            "objectId": "element-1"
                        }
                    }),
                    "Runtime.callFunctionOn" => json!({
                        "result": {"type": "object", "value": {
                            "attached": true, "visible": true, "enabled": true,
                            "stable": true, "obscured": false
                        }}
                    }),
                    "Runtime.releaseObjectGroup" => json!({}),
                    "DOM.scrollIntoViewIfNeeded" => {
                        locator_scrolled = true;
                        json!({})
                    }
                    "DOM.describeNode" => json!({
                        "node": {
                            "nodeId": 7, "backendNodeId": 41, "nodeType": 1,
                            "nodeName": "DIV", "localName": "div", "nodeValue": ""
                        }
                    }),
                    "DOM.getFrameOwner" if command["params"]["frameId"] == "same" => {
                        json!({"backendNodeId": 103})
                    }
                    "DOM.getFrameOwner" if command["params"]["frameId"] == "inner" => {
                        json!({"backendNodeId": 102})
                    }
                    "DOM.getFrameOwner" => json!({"backendNodeId": 101}),
                    "DOM.getBoxModel" if command["params"]["backendNodeId"] == 103 => json!({
                        "model": {
                            "content": [0, 0, 80, 0, 80, 60, 0, 60],
                            "padding": [0, 0, 80, 0, 80, 60, 0, 60],
                            "border": [0, 0, 80, 0, 80, 60, 0, 60],
                            "margin": [0, 0, 80, 0, 80, 60, 0, 60],
                            "width": 80, "height": 60
                        }
                    }),
                    "DOM.getBoxModel" if command["params"]["backendNodeId"] == 102 => json!({
                        "model": {
                            "content": [30, 10, 70, 10, 70, 40, 30, 40],
                            "padding": [30, 10, 70, 10, 70, 40, 30, 40],
                            "border": [30, 10, 70, 10, 70, 40, 30, 40],
                            "margin": [30, 10, 70, 10, 70, 40, 30, 40],
                            "width": 40, "height": 30
                        }
                    }),
                    "DOM.getBoxModel" if command["params"]["backendNodeId"] == 101 => {
                        frame_geometry_read = true;
                        let content = if transformed_top_owner {
                            vec![200, 20, 280, 30, 270, 90, 190, 80]
                        } else {
                            vec![200, 20, 280, 20, 280, 80, 200, 80]
                        };
                        json!({
                            "model": {
                                "content": content,
                                "padding": [200, 20, 280, 20, 280, 80, 200, 80],
                                "border": [200, 20, 280, 20, 280, 80, 200, 80],
                                "margin": [200, 20, 280, 20, 280, 80, 200, 80],
                                "width": 80, "height": 60
                            }
                        })
                    }
                    "DOM.getBoxModel"
                        if command["params"]["backendNodeId"] == 41
                            && (!locator_scrolled || remain_offscreen_after_scroll) =>
                    {
                        json!({
                            "model": {
                                "content": [500, 400, 520, 400, 520, 410, 500, 410],
                                "padding": [500, 400, 520, 400, 520, 410, 500, 410],
                                "border": [500, 400, 520, 400, 520, 410, 500, 410],
                                "margin": [500, 400, 520, 400, 520, 410, 500, 410],
                                "width": 20, "height": 10
                            }
                        })
                    }
                    "DOM.getBoxModel" if session_id == Some("oopif-inner") => json!({
                        "model": {
                            "content": [2, 3, 12, 3, 12, 8, 2, 8],
                            "padding": [2, 3, 12, 3, 12, 8, 2, 8],
                            "border": [2, 3, 12, 3, 12, 8, 2, 8],
                            "margin": [2, 3, 12, 3, 12, 8, 2, 8],
                            "width": 10, "height": 5
                        }
                    }),
                    "DOM.getBoxModel" => json!({
                        "model": {
                            "content": [5, 6, 39, 6, 39, 28, 5, 28],
                            "padding": [5, 6, 39, 6, 39, 28, 5, 28],
                            "border": [5, 6, 39, 6, 39, 28, 5, 28],
                            "margin": [5, 6, 39, 6, 39, 28, 5, 28],
                            "width": 34, "height": 22
                        }
                    }),
                    "Page.getLayoutMetrics" => json!({
                        "layoutViewport": {"pageX": 0, "pageY": 0, "clientWidth": layout_width, "clientHeight": layout_height},
                        "visualViewport": {"offsetX": 13.0, "offsetY": 17.0, "pageX": page_x, "pageY": page_y, "clientWidth": visual_width, "clientHeight": visual_height, "scale": 1.0},
                        "contentSize": {"x": 0.0, "y": 0.0, "width": 640.0, "height": 480.0},
                        "cssLayoutViewport": {"pageX": 0, "pageY": 0, "clientWidth": layout_width, "clientHeight": layout_height},
                        "cssVisualViewport": {"offsetX": 13.0, "offsetY": 17.0, "pageX": page_x, "pageY": page_y, "clientWidth": visual_width, "clientHeight": visual_height, "scale": 1.0},
                        "cssContentSize": {"x": 0.0, "y": 0.0, "width": 640.0, "height": 480.0}
                    }),
                    "Page.captureScreenshot" => {
                        capture_sent = true;
                        let width = command["params"]["clip"]["width"]
                            .as_f64()
                            .expect("capture clip width")
                            .round() as u32;
                        let height = command["params"]["clip"]["height"]
                            .as_f64()
                            .expect("capture clip height")
                            .round() as u32;
                        let png = encoded_test_png(width, height);
                        json!({"data": base64::engine::general_purpose::STANDARD.encode(png)})
                    }
                    other => panic!("unexpected OOPIF artifact command: {other}"),
                };
                let mut response = json!({"id": id, "result": result});
                if let Some(session_id) = command.get("sessionId") {
                    response["sessionId"] = session_id.clone();
                }
                write
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();

                if method == "Runtime.enable" {
                    let (context_id, unique_id, frame_id) = match session_id {
                        Some("oopif-inner") => (303, "context-inner", "inner"),
                        Some("oopif-child") => (202, "context-child", "child"),
                        _ => (101, "context-main", "main"),
                    };
                    write
                        .send(Message::Text(
                            json!({
                                "method": "Runtime.executionContextCreated",
                                "params": {"context": {
                                    "id": context_id,
                                    "uniqueId": unique_id,
                                    "origin": "https://fixture.test",
                                    "name": "",
                                    "auxData": {"isDefault": true, "frameId": frame_id}
                                }},
                                "sessionId": session_id
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                }

                if method == "Target.setAutoAttach"
                    && session_id == Some("artifact-top")
                    && !outer_attached
                {
                    outer_attached = true;
                    write
                        .send(Message::Text(
                            json!({
                                "method": "Target.attachedToTarget",
                                "params": {
                                    "sessionId": "oopif-child",
                                    "targetInfo": {
                                        "targetId": "child", "type": "iframe", "title": "",
                                        "url": "https://child.test/", "attached": true,
                                        "canAccessOpener": false, "parentFrameId": "main"
                                    },
                                    "waitingForDebugger": false
                                },
                                "sessionId": "artifact-top"
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                } else if method == "Target.setAutoAttach"
                    && session_id == Some("oopif-child")
                    && !inner_attached
                {
                    inner_attached = true;
                    write
                        .send(Message::Text(
                            json!({
                                "method": "Target.attachedToTarget",
                                "params": {
                                    "sessionId": "oopif-inner",
                                    "targetInfo": {
                                        "targetId": "inner", "type": "iframe", "title": "",
                                        "url": "https://inner.test/", "attached": true,
                                        "canAccessOpener": false, "parentFrameId": "same"
                                    },
                                    "waitingForDebugger": false
                                },
                                "sessionId": "oopif-child"
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                }
            }
        });

        let runtime = BrowserRuntime::connect(format!("ws://{address}"))
            .await
            .unwrap();
        let page = Page::new(
            runtime.clone(),
            BrowserSessionId::new("owner"),
            Weak::new(),
            "target-artifact".into(),
            PageOwnership::Attached,
            runtime.cdp().session("artifact-top"),
        );
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(frame) = page
                    .frames()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|frame| frame.id().as_str() == "child")
                {
                    if frame.cdp_session().await.unwrap().id() == "oopif-child" {
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        frame.expect("fake OOPIF route was not established");
        (page, commands)
    }

    fn assert_oopif_capture_routing_and_clip(commands: &[Value]) {
        let element_box = commands
            .iter()
            .find(|command| {
                command["method"] == "DOM.getBoxModel" && command["params"]["backendNodeId"] == 41
            })
            .expect("element box command");
        assert_eq!(element_box["sessionId"], "oopif-child");
        let capture = commands
            .iter()
            .find(|command| command["method"] == "Page.captureScreenshot")
            .expect("capture command");
        assert_eq!(capture["sessionId"], "artifact-top");
        assert_eq!(capture["params"]["clip"]["x"], 212.0);
        assert_eq!(capture["params"]["clip"]["y"], 37.0);
        assert_eq!(capture["params"]["clip"]["width"], 34.0);
        assert_eq!(capture["params"]["clip"]["height"], 22.0);
    }

    #[tokio::test]
    async fn oopif_locator_screenshot_routes_dom_to_child_and_capture_to_top_level() {
        let (page, commands) = fake_oopif_artifact_page().await;
        let child = page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .find(|frame| frame.id().as_str() == "child")
            .unwrap();
        commands.lock().clear();
        child
            .locator("#blue")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        let commands = commands.lock();
        assert_oopif_capture_routing_and_clip(&commands);
        let scroll_index = commands
            .iter()
            .position(|command| command["method"] == "DOM.scrollIntoViewIfNeeded")
            .expect("locator is scrolled before capture");
        let resolves = commands
            .iter()
            .enumerate()
            .filter(|(_, command)| {
                command["method"] == "Runtime.evaluate"
                    && command["params"].get("uniqueContextId").is_none()
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(
            resolves.len(),
            2,
            "locator must be re-resolved after scroll"
        );
        assert!(resolves[0] < scroll_index && scroll_index < resolves[1]);
    }

    #[tokio::test]
    async fn locator_still_outside_frame_aperture_after_scroll_fails_without_capture() {
        let (page, commands) = fake_oopif_artifact_page_with_invisible_locator().await;
        let child = page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .find(|frame| frame.id().as_str() == "child")
            .unwrap();
        commands.lock().clear();

        let error = child
            .locator("#blue")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap_err();

        assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
        assert_eq!(
            error.artifact_failure(),
            Some(&ArtifactFailure::EmptyRegion)
        );
        let commands = commands.lock();
        assert!(commands
            .iter()
            .any(|command| command["method"] == "DOM.scrollIntoViewIfNeeded"));
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Page.captureScreenshot")
                .count(),
            0,
            "an aperture failure must never return unrelated top-level pixels"
        );
    }

    #[tokio::test]
    async fn cross_session_one_way_lineage_uses_child_parent_id_as_authority() {
        let (page, commands) = fake_oopif_artifact_page().await;
        let child = page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .find(|frame| frame.id().as_str() == "child")
            .unwrap();
        commands.lock().clear();
        let artifact = child
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();

        assert_eq!(
            artifact.dimensions(),
            Some(ArtifactDimensions {
                width: 80,
                height: 60,
            })
        );
        let commands = commands.lock();
        assert!(!commands.iter().any(|command| matches!(
            command["method"].as_str(),
            Some("Runtime.evaluate" | "DOM.describeNode")
        )));
        let owner = commands
            .iter()
            .find(|command| {
                command["method"] == "DOM.getFrameOwner" && command["params"]["frameId"] == "child"
            })
            .expect("OOPIF owner command");
        assert_eq!(owner["sessionId"], "artifact-top");
        let capture = commands
            .iter()
            .find(|command| command["method"] == "Page.captureScreenshot")
            .expect("capture command");
        assert_eq!(capture["sessionId"], "artifact-top");
        assert_eq!(capture["params"]["clip"]["x"], 207.0);
        assert_eq!(capture["params"]["clip"]["y"], 31.0);
        assert_eq!(capture["params"]["clip"]["width"], 80.0);
        assert_eq!(capture["params"]["clip"]["height"], 60.0);
    }

    #[tokio::test]
    async fn same_session_reciprocal_lineage_remains_valid_inside_oopif() {
        let (page, commands) = fake_oopif_artifact_page().await;
        let same = page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .find(|frame| frame.id().as_str() == "same")
            .unwrap();
        commands.lock().clear();

        let artifact = same.screenshot(ScreenshotOptions::default()).await.unwrap();

        assert_eq!(
            artifact.dimensions(),
            Some(ArtifactDimensions {
                width: 80,
                height: 60,
            })
        );
        let commands = commands.lock();
        assert!(!commands
            .iter()
            .any(|command| command["method"] == "DOM.describeNode"));
        let viewport_facts = commands
            .iter()
            .filter(|command| {
                command["method"] == "Runtime.evaluate"
                    && command["params"].get("uniqueContextId").is_some()
            })
            .collect::<Vec<_>>();
        assert_eq!(viewport_facts.len(), 3);
        assert!(viewport_facts.iter().all(|command| {
            command["sessionId"] == "oopif-child"
                && command["params"]["uniqueContextId"] == "context-child"
        }));
        let owner = commands
            .iter()
            .find(|command| {
                command["method"] == "DOM.getFrameOwner" && command["params"]["frameId"] == "same"
            })
            .expect("same-process frame owner command");
        assert_eq!(owner["sessionId"], "oopif-child");
        let owner_box = commands
            .iter()
            .find(|command| {
                command["method"] == "DOM.getBoxModel" && command["params"]["backendNodeId"] == 103
            })
            .expect("same-process frame owner box command");
        assert_eq!(owner_box["sessionId"], "oopif-child");
        let capture = commands
            .iter()
            .find(|command| command["method"] == "Page.captureScreenshot")
            .expect("capture command");
        assert_eq!(capture["sessionId"], "artifact-top");
        assert_eq!(capture["params"]["clip"]["x"], 207.0);
        assert_eq!(capture["params"]["clip"]["y"], 31.0);
        assert_eq!(capture["params"]["clip"]["width"], 80.0);
        assert_eq!(capture["params"]["clip"]["height"], 60.0);
    }

    #[tokio::test]
    async fn nested_oopif_locator_scrolls_and_stays_inside_every_aperture() {
        let (page, commands) = fake_oopif_artifact_page().await;
        let inner = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(frame) = page
                    .frames()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|frame| frame.id().as_str() == "inner")
                {
                    if frame.cdp_session().await.unwrap().id() == "oopif-inner" {
                        break frame;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("nested fake OOPIF route was not established");
        commands.lock().clear();

        let artifact = inner
            .locator("#nested")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();

        assert_eq!(
            artifact.dimensions(),
            Some(ArtifactDimensions {
                width: 10,
                height: 5,
            })
        );
        let commands = commands.lock();
        assert!(commands
            .iter()
            .any(|command| command["method"] == "DOM.scrollIntoViewIfNeeded"));
        let capture = commands
            .iter()
            .find(|command| command["method"] == "Page.captureScreenshot")
            .unwrap();
        assert_eq!(capture["params"]["clip"]["x"], 239.0);
        assert_eq!(capture["params"]["clip"]["y"], 44.0);
        assert_eq!(capture["params"]["clip"]["width"], 10.0);
        assert_eq!(capture["params"]["clip"]["height"], 5.0);
    }

    #[tokio::test]
    async fn nested_oopif_frame_screenshot_maps_every_session_boundary_from_inner_to_outer() {
        let (page, commands) = fake_oopif_artifact_page().await;
        let inner = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(frame) = page
                    .frames()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|frame| frame.id().as_str() == "inner")
                {
                    if frame.cdp_session().await.unwrap().id() == "oopif-inner" {
                        break frame;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("nested fake OOPIF route was not established");
        commands.lock().clear();

        let artifact = inner
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();

        assert_eq!(
            artifact.dimensions(),
            Some(ArtifactDimensions {
                width: 40,
                height: 30,
            })
        );
        let commands = commands.lock();
        let owner_frames = commands
            .iter()
            .filter(|command| command["method"] == "DOM.getFrameOwner")
            .map(|command| {
                (
                    command["sessionId"].as_str().unwrap(),
                    command["params"]["frameId"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let expected = [
            ("oopif-child", "inner"),
            ("oopif-child", "same"),
            ("artifact-top", "child"),
        ];
        assert_eq!(owner_frames.len(), expected.len() * 3);
        assert!(owner_frames
            .chunks_exact(expected.len())
            .all(|chunk| chunk == expected));
        let capture = commands
            .iter()
            .find(|command| command["method"] == "Page.captureScreenshot")
            .unwrap();
        assert_eq!(capture["sessionId"], "artifact-top");
        assert_eq!(capture["params"]["clip"]["x"], 237.0);
        assert_eq!(capture["params"]["clip"]["y"], 41.0);
        assert_eq!(capture["params"]["clip"]["width"], 40.0);
        assert_eq!(capture["params"]["clip"]["height"], 30.0);
    }

    #[tokio::test]
    async fn transformed_owner_that_cannot_be_an_axis_aligned_clip_fails_closed() {
        let (page, commands) = fake_oopif_artifact_page_with_transformed_owner().await;
        let inner = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(frame) = page
                    .frames()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|frame| frame.id().as_str() == "inner")
                {
                    if frame.cdp_session().await.unwrap().id() == "oopif-inner" {
                        break frame;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("nested fake OOPIF route was not established");
        commands.lock().clear();

        let error = inner
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap_err();

        assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
        assert_eq!(
            error.artifact_failure(),
            Some(&ArtifactFailure::InvalidData)
        );
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Page.captureScreenshot")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn same_process_frame_inside_oopif_does_not_apply_an_extra_owner_offset() {
        let (page, commands) = fake_oopif_artifact_page().await;
        let same = page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .find(|frame| frame.id().as_str() == "same")
            .unwrap();
        assert_eq!(same.cdp_session().await.unwrap().id(), "oopif-child");
        commands.lock().clear();

        same.locator("#same")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();

        let commands = commands.lock();
        let owners = commands
            .iter()
            .filter(|command| command["method"] == "DOM.getFrameOwner")
            .collect::<Vec<_>>();
        assert_eq!(owners.len(), 6);
        assert!(owners.chunks_exact(2).all(|sample| {
            sample[0]["params"]["frameId"] == "same"
                && sample[0]["sessionId"] == "oopif-child"
                && sample[1]["params"]["frameId"] == "child"
                && sample[1]["sessionId"] == "artifact-top"
        }));
        let capture = commands
            .iter()
            .find(|command| command["method"] == "Page.captureScreenshot")
            .unwrap();
        assert_eq!(capture["params"]["clip"]["x"], 212.0);
        assert_eq!(capture["params"]["clip"]["y"], 37.0);
    }

    #[tokio::test]
    async fn main_frame_screenshot_uses_css_visual_viewport_without_scaling() {
        let (page, commands) = fake_oopif_artifact_page().await;
        let main = page.main_frame().await.unwrap();
        commands.lock().clear();
        let artifact = main.screenshot(ScreenshotOptions::default()).await.unwrap();

        assert_eq!(
            artifact.dimensions(),
            Some(ArtifactDimensions {
                width: 400,
                height: 300,
            })
        );
        let commands = commands.lock();
        assert!(!commands.iter().any(|command| matches!(
            command["method"].as_str(),
            Some("DOM.getFrameOwner" | "DOM.getBoxModel" | "DOM.describeNode" | "Runtime.evaluate")
        )));
        let capture = commands
            .iter()
            .find(|command| command["method"] == "Page.captureScreenshot")
            .unwrap();
        assert_eq!(capture["sessionId"], "artifact-top");
        assert_eq!(capture["params"]["clip"]["x"], 7.0);
        assert_eq!(capture["params"]["clip"]["y"], 11.0);
        assert_eq!(capture["params"]["clip"]["width"], 400.0);
        assert_eq!(capture["params"]["clip"]["height"], 300.0);
    }

    #[tokio::test]
    async fn frame_lineage_change_before_capture_fails_without_dispatch() {
        let (page, commands) = fake_oopif_artifact_page_with_lineage_change(true, false).await;
        let child = page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .find(|frame| frame.id().as_str() == "child")
            .unwrap();
        commands.lock().clear();

        let error = child
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap_err();

        assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Page.captureScreenshot")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn frame_lineage_change_after_capture_is_completed_once() {
        let (page, commands) = fake_oopif_artifact_page_with_lineage_change(false, true).await;
        let child = page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .find(|frame| frame.id().as_str() == "child")
            .unwrap();
        commands.lock().clear();

        let error = child
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap_err();

        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        assert_eq!(
            commands
                .lock()
                .iter()
                .filter(|command| command["method"] == "Page.captureScreenshot")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn frame_route_change_before_capture_fails_without_dispatch() {
        let (page, commands) =
            fake_oopif_artifact_page_with_route_stale(true, false, false, false, false, false)
                .await;
        let child = page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .find(|frame| frame.id().as_str() == "child")
            .unwrap();
        commands.lock().clear();

        let error = child
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap_err();

        assert_eq!(error.action_completed(), ActionCompletion::NotStarted);
        let commands = commands.lock();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Page.captureScreenshot")
                .count(),
            0
        );
        assert!(commands
            .iter()
            .any(|command| command["method"] == "Page.getFrameTree"));
    }

    #[tokio::test]
    async fn frame_route_change_after_capture_is_completed_and_never_retried() {
        let (page, commands) = fake_oopif_artifact_page_with_stale_after_capture(true).await;
        let child = page
            .frames()
            .await
            .unwrap()
            .into_iter()
            .find(|frame| frame.id().as_str() == "child")
            .unwrap();
        commands.lock().clear();

        let error = child
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.action_completed(), ActionCompletion::Completed);
        let commands = commands.lock();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Page.captureScreenshot")
                .count(),
            1
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command["method"] == "Runtime.evaluate")
                .count(),
            0
        );
        let capture_index = commands
            .iter()
            .position(|command| command["method"] == "Page.captureScreenshot")
            .unwrap();
        assert!(commands[..capture_index]
            .iter()
            .any(|command| command["method"] == "Page.getFrameTree"));
        assert!(commands[capture_index + 1..]
            .iter()
            .any(|command| command["method"] == "Page.getFrameTree"));
    }

    async fn serve_artifact_fixture(listener: tokio::net::TcpListener, root: String, same: String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let root = root.clone();
            let same = same.clone();
            tokio::spawn(async move {
                let mut request = vec![0_u8; 8192];
                let Ok(count) = stream.read(&mut request).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&request[..count]);
                let body = if request.starts_with("GET /same ") {
                    same
                } else {
                    root
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }

    fn png_center_rgb(bytes: &[u8]) -> (u8, u8, u8) {
        let decoder = png::Decoder::new(bytes);
        let mut reader = decoder.read_info().unwrap();
        let mut buffer = vec![0; reader.output_buffer_size()];
        let output = reader.next_frame(&mut buffer).unwrap();
        let channels = match output.color_type {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            other => panic!("unexpected screenshot PNG color type: {other:?}"),
        };
        let x = output.width as usize / 2;
        let y = output.height as usize / 2;
        let offset = (y * output.width as usize + x) * channels;
        (buffer[offset], buffer[offset + 1], buffer[offset + 2])
    }

    fn assert_dominant(actual: (u8, u8, u8), channel: usize) {
        let values = [actual.0, actual.1, actual.2];
        assert!(
            values[channel] > 180,
            "expected dominant channel {channel}, got {actual:?}"
        );
        assert!(
            values
                .iter()
                .enumerate()
                .all(|(index, value)| index == channel || *value < 100),
            "expected a clear color sample, got {actual:?}"
        );
    }

    fn explicitly_allowed_ports_arg(ports: impl IntoIterator<Item = u16>) -> String {
        let ports = ports
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|port| port.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!("--explicitly-allowed-ports={ports}")
    }

    #[test]
    fn explicitly_allowed_ports_are_sorted_deduplicated_decimal_u16_values() {
        assert_eq!(
            explicitly_allowed_ports_arg([40_000, 12_345]),
            "--explicitly-allowed-ports=12345,40000"
        );
        assert_eq!(
            explicitly_allowed_ports_arg([40_000, 12_345, 40_000]),
            "--explicitly-allowed-ports=12345,40000"
        );
        assert_eq!(
            explicitly_allowed_ports_arg([u16::MIN, u16::MAX]),
            "--explicitly-allowed-ports=0,65535"
        );
    }

    struct LiveArtifactFixture {
        runtime: crate::runtime::BrowserRuntime,
        page: Page,
        parent_server: tokio::task::JoinHandle<()>,
        child_server: tokio::task::JoinHandle<()>,
    }

    impl LiveArtifactFixture {
        async fn launch() -> Self {
            use crate::runtime::{BrowserRuntime, LaunchOptions};

            let child_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let child_address = child_listener.local_addr().unwrap();
            let child_port = child_address.port();
            let parent_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let parent_address = parent_listener.local_addr().unwrap();
            let parent_port = parent_address.port();
            let same_body = "<!doctype html><style>html,body{margin:0;background:#00ff00}#green{width:30px;height:20px;background:#00ff00}</style><div id=green></div>".to_owned();
            let child_body = "<!doctype html><style>html,body{margin:0;background:#0000ff}#blue{width:34px;height:22px;background:#0000ff}iframe{position:absolute;left:25px;top:0;border:0;width:50px;height:40px}</style><div id=blue></div><iframe id=nested src=/same></iframe>".to_owned();
            let nested_body = "<!doctype html><style>html,body{margin:0;background:#ffff00}#yellow{width:26px;height:18px;background:#ffff00}</style><div id=yellow></div>".to_owned();
            let parent_body = format!(
                r#"<!doctype html><style>html,body{{margin:0;background:white}}#red{{width:40px;height:30px;background:#ff0000}}iframe{{border:0;width:80px;height:60px}}#same{{position:absolute;left:100px;top:20px}}#cross{{position:absolute;left:200px;top:20px}}</style><div id=red></div><div id=host></div><script>const root=document.querySelector('#host').attachShadow({{mode:'open'}});root.innerHTML='<div id=purple style=\"width:36px;height:24px;background:#ff00ff\"></div>';</script><iframe id=same src=/same></iframe><iframe id=cross src="http://child.test:{child_port}/"></iframe>"#
            );
            let parent_server = tokio::spawn(serve_artifact_fixture(
                parent_listener,
                parent_body,
                same_body,
            ));
            let child_server = tokio::spawn(serve_artifact_fixture(
                child_listener,
                child_body,
                nested_body,
            ));
            let runtime = BrowserRuntime::launch(
                LaunchOptions::default()
                    .headless(true)
                    .arg("--site-per-process")
                    .arg("--host-resolver-rules=MAP *.test 127.0.0.1")
                    .arg(explicitly_allowed_ports_arg([
                        parent_address.port(),
                        child_address.port(),
                    ])),
            )
            .await
            .unwrap();
            let session = runtime.default_session().await.unwrap();
            let page = session.new_page("about:blank").await.unwrap();
            let initial_main = page.main_frame().await.unwrap();
            let initial_epoch = initial_main.document_epoch();
            let initial_generation = page.generation();
            let mut events = page.subscribe_events().await.unwrap();
            let target_url = format!("http://parent.test:{parent_port}/");
            let navigation = page.goto(target_url.as_str()).await.unwrap();
            assert_eq!(navigation.final_url(), target_url);
            let committed = tokio::time::timeout(std::time::Duration::from_secs(8), async {
                loop {
                    let event = events
                        .next()
                        .await
                        .expect("page event stream closed before initial navigation commit")
                        .expect("page event stream failed before initial navigation commit");
                    if matches!(
                        event.event(),
                        crate::runtime::PageEvent::FrameNavigated {
                            frame_id,
                            url,
                            loader_id: Some(_),
                            same_document: false,
                        } if frame_id == initial_main.id() && url == &target_url
                    ) {
                        return event;
                    }
                }
            })
            .await
            .expect("initial navigation was not reduced before the artifact deadline");
            assert_eq!(
                committed.metadata().identity().page_generation(),
                Some(initial_generation)
            );
            let ready_main = live_frame_with_selector(&page, "#red").await;
            assert_eq!(ready_main.id(), initial_main.id());
            assert_ne!(ready_main.document_epoch(), initial_epoch);
            let ready_state: String = ready_main.evaluate("document.readyState").await.unwrap();
            assert_eq!(ready_state, "complete");
            Self {
                runtime,
                page,
                parent_server,
                child_server,
            }
        }

        async fn close(self) {
            assert!(self.runtime.close().await.is_complete());
            self.parent_server.abort();
            self.child_server.abort();
        }
    }

    async fn live_frame_with_selector(page: &Page, selector: &str) -> Frame {
        use crate::runtime::{Evaluation, EvaluationArgument, PageEvent, PageEventStream};
        use futures::stream::FuturesUnordered;

        async fn next_frame_change(events: &mut PageEventStream) {
            loop {
                let event = events
                    .next()
                    .await
                    .expect("page event stream closed before frame selector readiness")
                    .expect("page event stream failed before frame selector readiness");
                if matches!(
                    event.event(),
                    PageEvent::FrameAttached { .. }
                        | PageEvent::FrameDetached { .. }
                        | PageEvent::FrameNavigated { .. }
                        | PageEvent::FrameRouteChanged { .. }
                ) {
                    return;
                }
            }
        }

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        // Subscribe before the first frame snapshot so a concurrent attach,
        // navigation, or OOPIF route change cannot be lost between the two.
        let mut events = page.subscribe_events().await.unwrap();
        tokio::time::timeout_at(deadline, async {
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let mut checks = page
                    .frames()
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|frame| {
                        let selector = EvaluationArgument::json(selector).unwrap();
                        let browser_timeout = EvaluationArgument::json(
                            u64::try_from(remaining.as_millis())
                                .unwrap_or(u64::MAX)
                                .max(1),
                        )
                        .unwrap();
                        async move {
                            let ready = frame
                                .evaluate::<bool>(
                                    Evaluation::function(
                                        "(selector, timeoutMilliseconds) => new Promise((resolve) => {\n\
                                         let observer;\n\
                                         let timeout;\n\
                                         const finish = (found) => {\n\
                                             if (observer) observer.disconnect();\n\
                                             if (timeout !== undefined) clearTimeout(timeout);\n\
                                             resolve(found);\n\
                                         };\n\
                                         if (document.querySelector(selector)) {\n\
                                             finish(true);\n\
                                             return;\n\
                                         }\n\
                                         observer = new MutationObserver(() => {\n\
                                             if (document.querySelector(selector)) finish(true);\n\
                                         });\n\
                                         observer.observe(document, { childList: true, subtree: true });\n\
                                         timeout = setTimeout(() => finish(false), timeoutMilliseconds);\n\
                                     })",
                                    )
                                    .argument(selector)
                                    .argument(browser_timeout)
                                    .deadline(remaining),
                                )
                                .await;
                            (frame, ready)
                        }
                    })
                    .collect::<FuturesUnordered<_>>();

                if checks.is_empty() {
                    next_frame_change(&mut events).await;
                    continue;
                }

                tokio::select! {
                    ready = async {
                        while let Some((frame, ready)) = checks.next().await {
                            if ready.unwrap_or(false) {
                                return Some(frame);
                            }
                        }
                        None
                    } => match ready {
                        Some(frame) => {
                            let operation = page
                                .admit_operation("assert live artifact locator resolution")
                                .unwrap();
                            frame
                                .locator(selector)
                                .resolve_admitted(&operation)
                                .await
                                .expect("ready live artifact selector must resolve normally");
                            return frame;
                        }
                        None => next_frame_change(&mut events).await,
                    },
                    () = next_frame_change(&mut events) => {}
                }
            }
        })
        .await
        .expect("frame selector did not become available")
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_artifact_formats_full_page_pdf_html_and_main_viewport() {
        let fixture = LiveArtifactFixture::launch().await;
        let page = &fixture.page;
        let main = page.main_frame().await.unwrap();
        let viewport_before: Vec<f64> = page
            .evaluate("[scrollX, scrollY, innerWidth, innerHeight]")
            .await
            .unwrap();
        let full_page = page
            .screenshot(ScreenshotOptions::default().full_page(true))
            .await
            .unwrap();
        let viewport_after: Vec<f64> = page
            .evaluate("[scrollX, scrollY, innerWidth, innerHeight]")
            .await
            .unwrap();
        assert_eq!(viewport_after, viewport_before);
        assert!(full_page.metadata().full_page);
        let main_frame = main.screenshot(ScreenshotOptions::default()).await.unwrap();
        let page_viewport = page.screenshot(ScreenshotOptions::default()).await.unwrap();
        assert_eq!(main_frame.dimensions(), page_viewport.dimensions());
        png_center_rgb(page_viewport.as_bytes());
        for (format, mime_type, magic) in [
            (ScreenshotFormat::Jpeg, "image/jpeg", &b"\xff\xd8"[..]),
            (ScreenshotFormat::Webp, "image/webp", &b"RIFF"[..]),
        ] {
            let artifact = page
                .screenshot(ScreenshotOptions::default().format(format).quality(75))
                .await
                .unwrap();
            assert_eq!(artifact.mime_type(), mime_type);
            assert!(artifact.as_bytes().starts_with(magic));
            assert_eq!(artifact.dimensions(), page_viewport.dimensions());
        }
        let pdf = page
            .pdf(PdfOptions::default().print_background(true))
            .await
            .unwrap();
        assert!(pdf.as_bytes().starts_with(b"%PDF-"));
        assert!(page
            .html(HtmlOptions::default())
            .await
            .unwrap()
            .as_str()
            .contains("id=\"red\""));
        fixture.close().await;
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_artifact_scope_routing_pixels_and_offscreen_reresolve() {
        let fixture = LiveArtifactFixture::launch().await;
        let runtime = fixture.runtime.clone();
        let page = fixture.page.clone();
        let parent_server = fixture.parent_server;
        let child_server = fixture.child_server;
        let main = page.main_frame().await.unwrap();

        let main_session = main.cdp_session().await.unwrap().id().to_owned();
        let same = live_frame_with_selector(&page, "#green").await;
        let oopif = live_frame_with_selector(&page, "#blue").await;
        let nested = live_frame_with_selector(&page, "#yellow").await;
        let same_session = same.cdp_session().await.unwrap().id().to_owned();
        let oopif_session = oopif.cdp_session().await.unwrap().id().to_owned();
        let nested_session = nested.cdp_session().await.unwrap().id().to_owned();
        assert_eq!(same_session, main_session);
        assert_ne!(oopif_session, main_session);
        assert_eq!(nested_session, oopif_session);

        let red = page
            .locator("#red")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(
            red.dimensions(),
            Some(ArtifactDimensions {
                width: 40,
                height: 30
            })
        );
        assert_dominant(png_center_rgb(red.as_bytes()), 0);
        let purple = page
            .locator("#purple")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(
            purple.dimensions(),
            Some(ArtifactDimensions {
                width: 36,
                height: 24
            })
        );
        let purple_rgb = png_center_rgb(purple.as_bytes());
        assert!(
            purple_rgb.0 > 180 && purple_rgb.2 > 180 && purple_rgb.1 < 100,
            "shadow sample was {purple_rgb:?}"
        );
        let green = same
            .locator("#green")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(
            green.dimensions(),
            Some(ArtifactDimensions {
                width: 30,
                height: 20
            })
        );
        assert_dominant(png_center_rgb(green.as_bytes()), 1);
        let blue = oopif
            .locator("#blue")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(
            blue.dimensions(),
            Some(ArtifactDimensions {
                width: 34,
                height: 22
            })
        );
        assert_dominant(png_center_rgb(blue.as_bytes()), 2);
        let yellow = nested
            .locator("#yellow")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(
            yellow.dimensions(),
            Some(ArtifactDimensions {
                width: 26,
                height: 18
            })
        );
        let yellow_rgb = png_center_rgb(yellow.as_bytes());
        assert!(
            yellow_rgb.0 > 180 && yellow_rgb.1 > 180 && yellow_rgb.2 < 100,
            "nested same-process-in-OOPIF sample was {yellow_rgb:?}"
        );
        let same_frame = same.screenshot(ScreenshotOptions::default()).await.unwrap();
        assert_eq!(
            same_frame.dimensions(),
            Some(ArtifactDimensions {
                width: 80,
                height: 60
            })
        );
        assert_dominant(png_center_rgb(same_frame.as_bytes()), 1);
        let oopif_frame = oopif
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(
            oopif_frame.dimensions(),
            Some(ArtifactDimensions {
                width: 80,
                height: 60
            })
        );
        png_center_rgb(oopif_frame.as_bytes());
        let nested_frame = nested
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(
            nested_frame.dimensions(),
            Some(ArtifactDimensions {
                width: 50,
                height: 40
            })
        );
        let nested_frame_rgb = png_center_rgb(nested_frame.as_bytes());
        assert!(
            nested_frame_rgb.0 > 180 && nested_frame_rgb.1 > 180 && nested_frame_rgb.2 < 100,
            "nested frame sample was {nested_frame_rgb:?}"
        );

        let same_before: Vec<f64> = same
            .evaluate(
                "(() => { document.body.style.background = '#ff00ff'; document.body.style.minHeight = '500px'; const element = document.querySelector('#green'); Object.assign(element.style, { position: 'absolute', left: '7px', top: '240px' }); scrollTo(0, 0); const rect = element.getBoundingClientRect(); return [scrollY, rect.top, rect.bottom, innerHeight]; })()",
            )
            .await
            .unwrap();
        assert_eq!(same_before[0], 0.0);
        assert!(
            same_before[1] >= same_before[3],
            "same-process element must begin below its frame aperture: {same_before:?}"
        );
        let same_offscreen = same
            .locator("#green")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(
            same_offscreen.dimensions(),
            Some(ArtifactDimensions {
                width: 30,
                height: 20
            })
        );
        assert_dominant(png_center_rgb(same_offscreen.as_bytes()), 1);
        let same_after: Vec<f64> = same
            .evaluate("(() => { const rect = document.querySelector('#green').getBoundingClientRect(); return [scrollY, rect.top, rect.bottom, innerHeight]; })()")
            .await
            .unwrap();
        assert!(
            same_after[0] > 0.0,
            "same-process frame did not scroll: {same_after:?}"
        );
        assert!(
            same_after[1] >= 0.0 && same_after[2] <= same_after[3],
            "same-process element was not re-resolved inside its aperture: {same_after:?}"
        );

        let oopif_before: Vec<f64> = oopif
            .evaluate(
                "(() => { document.body.style.background = '#ffff00'; document.body.style.minHeight = '500px'; const element = document.querySelector('#blue'); Object.assign(element.style, { position: 'absolute', left: '9px', top: '260px' }); scrollTo(0, 0); const rect = element.getBoundingClientRect(); return [scrollY, rect.top, rect.bottom, innerHeight]; })()",
            )
            .await
            .unwrap();
        assert_eq!(oopif_before[0], 0.0);
        assert!(
            oopif_before[1] >= oopif_before[3],
            "OOPIF element must begin below its frame aperture: {oopif_before:?}"
        );
        let oopif_offscreen = oopif
            .locator("#blue")
            .screenshot(ScreenshotOptions::default())
            .await
            .unwrap();
        assert_eq!(
            oopif_offscreen.dimensions(),
            Some(ArtifactDimensions {
                width: 34,
                height: 22
            })
        );
        assert_dominant(png_center_rgb(oopif_offscreen.as_bytes()), 2);
        let oopif_after: Vec<f64> = oopif
            .evaluate("(() => { const rect = document.querySelector('#blue').getBoundingClientRect(); return [scrollY, rect.top, rect.bottom, innerHeight]; })()")
            .await
            .unwrap();
        assert!(
            oopif_after[0] > 0.0,
            "OOPIF did not scroll: {oopif_after:?}"
        );
        assert!(
            oopif_after[1] >= 0.0 && oopif_after[2] <= oopif_after[3],
            "OOPIF element was not re-resolved inside its aperture: {oopif_after:?}"
        );
        assert!(runtime.close().await.is_complete());
        parent_server.abort();
        child_server.abort();
    }

    #[tokio::test]
    #[ignore = "requires installed Chrome and loopback sockets"]
    async fn live_chrome_diagnostics_future_only_bundle_and_save() {
        use crate::runtime::{DiagnosticBundleOptions, DiagnosticCollectorOptions};
        use std::time::Duration;

        let fixture = LiveArtifactFixture::launch().await;
        let page = &fixture.page;
        page.evaluate_value("console.log('artifact-before-collector')")
            .await
            .unwrap();
        let mut marker_events = page.subscribe_events().await.unwrap();
        let mut collector = page
            .start_diagnostic_collector(
                DiagnosticCollectorOptions::default().max_duration(Duration::from_secs(2)),
            )
            .await
            .unwrap();
        page.evaluate_value(
            "console.log('artifact-diagnostic-marker'); new Promise((resolve) => {\n\
             addEventListener('error', (event) => {\n\
                 if (event.message.includes('artifact-error-marker')) resolve(true);\n\
             }, { once: true });\n\
             queueMicrotask(() => { throw new Error('artifact-error-marker'); });\n\
         })",
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = marker_events
                    .next()
                    .await
                    .expect("page event stream closed before diagnostic marker")
                    .expect("page event stream failed before diagnostic marker");
                if matches!(
                    event.event(),
                    crate::runtime::PageEvent::JavaScriptError(error)
                        if error.exception_description.as_deref().is_some_and(|description| description.contains("artifact-error-marker"))
                ) {
                    break;
                }
            }
        })
        .await
        .expect("diagnostic page-error marker was not observed");
        collector
            .wait_for_observed_events(2, Duration::from_secs(2))
            .await;
        let events = collector.finish().await;
        assert!(events.console().iter().any(|message| {
            message
                .arguments
                .iter()
                .any(|argument| argument.value == Some(json!("artifact-diagnostic-marker")))
        }));
        assert!(events.console().iter().all(|message| {
            message
                .arguments
                .iter()
                .all(|argument| argument.value != Some(json!("artifact-before-collector")))
        }));
        assert!(events.page_errors().iter().any(|error| {
            error
                .exception_description
                .as_deref()
                .is_some_and(|description| description.contains("artifact-error-marker"))
        }));
        let bundle = page
            .diagnostic_bundle(
                DiagnosticBundleOptions::default().include_screenshot(true),
                events,
            )
            .await
            .unwrap();
        assert!(bundle.snapshot().is_available());
        assert!(bundle.screenshot().unwrap().is_available());

        let screenshot = page.screenshot(ScreenshotOptions::default()).await.unwrap();
        let output = tempfile::tempdir().unwrap();
        let target = output.path().join("saved.png");
        screenshot.save(&target).await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), screenshot.as_bytes());
        fixture.close().await;
    }
}
