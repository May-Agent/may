//! One-shot stills of a session desktop for the agent: `CGDisplayCreateImage`
//! needs no running stream — it composites the display's current framebuffer.

use std::ffi::c_void;

use super::DisplayError;

type CGImageRef = *mut c_void;
type CGDataProviderRef = *mut c_void;
type CFDataRef = *const c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGDisplayCreateImage(display: u32) -> CGImageRef;
    fn CGImageGetDataProvider(image: CGImageRef) -> CGDataProviderRef;
    fn CGImageGetWidth(image: CGImageRef) -> usize;
    fn CGImageGetHeight(image: CGImageRef) -> usize;
    fn CGImageGetBytesPerRow(image: CGImageRef) -> usize;
    fn CGDataProviderCopyData(provider: CGDataProviderRef) -> CFDataRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    fn CFDataGetLength(data: CFDataRef) -> isize;
    fn CFRelease(cf: *const c_void);
}

/// A PNG of the display's current contents.
///
/// # Errors
/// Returns [`DisplayError`] when the display cannot be snapshotted or
/// the pixels cannot be encoded.
pub fn screenshot_png(display_id: u32) -> Result<Vec<u8>, DisplayError> {
    let image = unsafe { CGDisplayCreateImage(display_id) };
    if image.is_null() {
        return Err(DisplayError("the display produced no image".to_owned()));
    }
    let (width, height, stride) = unsafe {
        (
            CGImageGetWidth(image),
            CGImageGetHeight(image),
            CGImageGetBytesPerRow(image),
        )
    };
    let provider = unsafe { CGImageGetDataProvider(image) };
    let data = unsafe { CGDataProviderCopyData(provider) };
    if data.is_null() {
        unsafe { CFRelease(image) };
        return Err(DisplayError(
            "the display image carried no pixels".to_owned(),
        ));
    }
    let raw = unsafe {
        let len = CFDataGetLength(data);
        std::slice::from_raw_parts(CFDataGetBytePtr(data), usize::try_from(len).unwrap_or(0))
            .to_vec()
    };
    unsafe {
        CFRelease(data);
        CFRelease(image);
    }

    // CGDisplayCreateImage surfaces are BGRA8; expand to RGB for PNG.
    let mut rgb = image::RgbImage::new(
        u32::try_from(width).unwrap_or(u32::MAX),
        u32::try_from(height).unwrap_or(u32::MAX),
    );
    for (y, row) in raw.chunks_exact(stride).take(height).enumerate() {
        for (x, px) in row.as_chunks::<4>().0.iter().take(width).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            rgb.put_pixel(x as u32, y as u32, image::Rgb([px[2], px[1], px[0]]));
        }
    }
    let mut png = std::io::Cursor::new(Vec::new());
    rgb.write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| DisplayError(format!("encode the screenshot: {e}")))?;
    Ok(png.into_inner())
}
