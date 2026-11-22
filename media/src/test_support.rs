//! Fixture images shared by the workspace's `#[cfg(test)]` code.
//!
//! `may-media`'s own tests see this module through `cfg(test)`; dependent
//! crates enable the `testing` feature in `[dev-dependencies]` to reach it.

use std::io::Cursor;

use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};

/// A photo-like PNG with zero text features: a smooth sky/hill gradient.
///
/// # Panics
/// Panics when the image cannot be encoded — impossible for a valid RGB
/// buffer, so a panic here is a bug.
#[must_use]
pub fn make_photo_png(width: u32, height: u32) -> Vec<u8> {
    let mut buf: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(width, height);
    for (x, y, pixel) in buf.enumerate_pixels_mut() {
        // Smooth sunset-like sky/hill gradient with zero text features.
        #[allow(clippy::cast_possible_truncation)]
        let r = ((x * 255) / width.max(1)) as u8;
        #[allow(clippy::cast_possible_truncation)]
        let g = ((y * 180) / height.max(1)) as u8;
        let horizon = height * 2 / 3;
        let b = if y < horizon { 210 } else { 70 };
        *pixel = Rgb([r, g, b]);
    }
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(buf)
        .write_to(&mut out, ImageFormat::Png)
        .expect("encode photo png");
    out.into_inner()
}

/// A PNG of `lines` drawn through `AppKit`: the text-dense screenshot OCR
/// tests recognize.
///
/// # Panics
/// Panics when the bitmap context cannot be allocated or the PNG cannot be
/// encoded — impossible off an `AppKit` session, so a panic here is a bug.
#[cfg(all(feature = "vision-ocr", target_os = "macos"))]
#[must_use]
pub fn render_multilingual_text_png(lines: &[&str], width: usize, height: usize) -> Vec<u8> {
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{
        NSBezierPath, NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace,
        NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSGraphicsContext,
        NSStringDrawing,
    };
    use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

    let rep: Retained<NSBitmapImageRep> = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            width.cast_signed(),
            height.cast_signed(),
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
        .expect("allocate NSBitmapImageRep")
    };
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)
        .expect("bitmap graphics context");
    ctx.saveGraphicsState();
    NSGraphicsContext::setCurrentContext(Some(&ctx));

    NSColor::whiteColor().setFill();
    let canvas = NSRect::new(
        NSPoint::new(0.0, 0.0),
        #[allow(clippy::cast_precision_loss)]
        NSSize::new(width as f64, height as f64),
    );
    NSBezierPath::fillRect(canvas);

    let font = NSFont::systemFontOfSize(34.0);
    let color = NSColor::blackColor();
    let attrs: Retained<NSDictionary<objc2_foundation::NSAttributedStringKey, AnyObject>> = unsafe {
        let keys = [NSFontAttributeName, NSForegroundColorAttributeName];
        let objs: [&AnyObject; 2] = [(*font).as_ref(), (*color).as_ref()];
        NSDictionary::from_slices(&keys, &objs)
    };

    for (idx, line) in lines.iter().enumerate() {
        let text = NSString::from_str(line);
        #[allow(clippy::cast_precision_loss)]
        let y = (idx as f64).mul_add(-64.0, (height as f64) - 96.0);
        let point = NSPoint::new(48.0, y);
        unsafe {
            text.drawAtPoint_withAttributes(point, Some(&attrs));
        }
    }

    ctx.flushGraphics();
    NSGraphicsContext::setCurrentContext(None);
    ctx.restoreGraphicsState();
    let empty = NSDictionary::new();
    let png_data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &empty)
            .expect("encode png representation")
    };
    png_data.to_vec()
}
