//! Vision executor: screenshot → model proposal → viewport point.
//!
//! It owns the geometry, not the model. The adapter returns a box normalized
//! to the image; this module is responsible for proving that the image really
//! is the CSS viewport at the current scale, then for converting the box into
//! the coordinates `Input.dispatchMouseEvent` expects.
//!
//! Why normalized boxes: device scale factor, browser zoom and the screenshot
//! size all multiply `/ divide` into each other. A box expressed as a fraction
//! of the picture cancels all three, provided the picture is exactly the
//! viewport. That last condition is checked rather than assumed.

use std::sync::Arc;

use bh_browser::Session;
use bh_core::{BhError, Point, Rect, Result, Step};
use bh_model::{ImageData, ModelRequest, ModelRouter};

pub struct Vision {
    router: ModelRouter,
}

#[derive(Debug, Clone)]
pub struct VisualHit {
    /// Box in CSS pixels, relative to the viewport.
    pub bbox: Rect,
    pub point: Point,
    pub confidence: f64,
    pub model: String,
    pub image_w: u32,
    pub image_h: u32,
    pub viewport: (f64, f64, f64),
}

impl Vision {
    pub fn new(router: ModelRouter) -> Self {
        Self { router }
    }

    /// Always constructible: without a configured adapter a visual target
    /// fails with "no model adapter configured" instead of being skipped.
    pub fn from_env(max_calls: u32) -> Arc<Self> {
        Arc::new(Self::new(ModelRouter::from_env(max_calls)))
    }

    pub fn calls(&self) -> u32 {
        self.router.calls()
    }

    /// The task shares one router across visual and semantic work, so both
    /// spend the same `max_model_calls` budget and land in one counter.
    pub fn router(&self) -> &ModelRouter {
        &self.router
    }

    /// Second opinion on the current page state, with an optional screenshot.
    pub async fn semantic_verify(
        &self,
        goal: &str,
        state: &str,
        threshold: f64,
        image: Option<ImageData>,
    ) -> Result<bh_model::VerifyReport> {
        bh_model::verify(&self.router, goal, state, threshold, image).await
    }

    /// A PNG of the viewport with its dimensions, ready for a vision model.
    /// No geometry check here: the verifier judges the picture, it does not
    /// translate coordinates, so a non-viewport capture is odd but not unsafe.
    pub async fn screenshot_image(&self, session: &Session) -> Result<ImageData> {
        let png = session.screenshot_png().await?;
        let (width, height) = png_dimensions(&png)?;
        Ok(ImageData { media_type: "image/png", bytes: png, width, height })
    }

    /// Ask for a corrected step. The runner enforces the safety envelope
    /// (same id, no higher side effect, allowed hosts) before executing it.
    pub async fn repair(
        &self,
        goal: &str,
        state: &str,
        failed: &Step,
        error: &str,
    ) -> Result<bh_model::RepairReport> {
        bh_model::repair(&self.router, goal, state, failed, error).await
    }

    pub async fn locate(&self, session: &Session, description: &str, goal: &str) -> Result<VisualHit> {
        let vp = session.viewport().await?;
        let png = session.screenshot_png().await?;
        let (image_w, image_h) = png_dimensions(&png)?;

        // The screenshot must be the viewport, scaled by DPR. If it is not
        // (full-page capture, zoom, a mismatched emulation setting), a
        // normalized box would map to the wrong place and the click would land
        // blind. Refuse instead.
        let expected_w = vp.w * vp.dpr;
        let expected_h = vp.h * vp.dpr;
        let tol_w = (expected_w * 0.02).max(2.0);
        let tol_h = (expected_h * 0.02).max(2.0);
        if (f64::from(image_w) - expected_w).abs() > tol_w
            || (f64::from(image_h) - expected_h).abs() > tol_h
        {
            return Err(BhError::Other(format!(
                "screenshot is {image_w}x{image_h} but the viewport is {}x{} at dpr {} \
                 (expected {expected_w:.0}x{expected_h:.0}); refusing to translate coordinates",
                vp.w, vp.h, vp.dpr
            )));
        }

        let mut req = ModelRequest::visual(description, goal);
        req.image = Some(ImageData {
            media_type: "image/png",
            bytes: png,
            width: image_w,
            height: image_h,
        });
        let proposal = self.router.propose(&req).await?;
        let b = proposal
            .bbox_norm
            .ok_or_else(|| BhError::Other("visual proposal has no box".into()))?;

        let bbox = Rect {
            x: b[0] * vp.w,
            y: b[1] * vp.h,
            w: b[2] * vp.w,
            h: b[3] * vp.h,
        };
        if bbox.w < 1.0 || bbox.h < 1.0 {
            return Err(BhError::Other(format!(
                "model returned a {:.2}x{:.2} px box; too small to click reliably",
                bbox.w, bbox.h
            )));
        }
        let point = bbox.center();
        if point.x < 0.0 || point.y < 0.0 || point.x > vp.w || point.y > vp.h {
            return Err(BhError::Other(format!(
                "model box {bbox:?} maps outside the {}x{} viewport",
                vp.w, vp.h
            )));
        }

        tracing::info!(
            target = description,
            point = ?point,
            confidence = proposal.confidence,
            model = %proposal.model,
            "visual locate"
        );

        Ok(VisualHit {
            bbox,
            point,
            confidence: proposal.confidence,
            model: proposal.model,
            image_w,
            image_h,
            viewport: (vp.w, vp.h, vp.dpr),
        })
    }
}

/// PNG IHDR: width and height are big-endian u32 at bytes 16..24. Parsing the
/// header avoids carrying a full image decoder just to sanity-check geometry.
pub fn png_dimensions(png: &[u8]) -> Result<(u32, u32)> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if png.len() < 24 || png[..8] != SIG || &png[12..16] != b"IHDR" {
        return Err(BhError::Other("screenshot is not a PNG".into()));
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    if w == 0 || h == 0 {
        return Err(BhError::Other("screenshot has zero dimensions".into()));
    }
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_png_dimensions() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&13u32.to_be_bytes());
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&640u32.to_be_bytes());
        ihdr.extend_from_slice(&360u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        assert_eq!(png_dimensions(&png).unwrap(), (640, 360));
    }

    #[test]
    fn rejects_a_jpeg() {
        assert!(png_dimensions(&[0xff, 0xd8, 0xff, 0xe0]).is_err());
    }
}
