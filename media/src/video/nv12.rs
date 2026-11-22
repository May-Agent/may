//! The one CPU pixel path left: packed RGB or BGRA to NV12, for the frames
//! that cannot reach the encoder as surfaces (software AV1, odd-sized
//! frames, hosts without a platform JPEG decoder).

#[cfg(target_os = "macos")]
use yuv::bgra_to_yuv_nv12;
use yuv::{
    BufferStoreMut, YuvBiPlanarImageMut, YuvConversionMode, YuvError, YuvRange, YuvStandardMatrix,
    rgb_to_yuv_nv12,
};

/// Matrix and range of the NV12 this module produces. The encoder is
/// configured with the same description (`VideoColorInfo::default()`:
/// BT.709, limited range), so the signalled colorimetry matches the samples.
const RANGE: YuvRange = YuvRange::Limited;
const MATRIX: YuvStandardMatrix = YuvStandardMatrix::Bt709;
const MODE: YuvConversionMode = YuvConversionMode::Balanced;

/// A tightly packed NV12 frame of even dimensions.
#[derive(Debug)]
pub struct Nv12 {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The packed pixel layouts the sources hand over.
#[derive(Debug, Clone, Copy)]
pub enum Packed {
    Rgb,
    /// Display surfaces, which only macOS captures.
    #[cfg(target_os = "macos")]
    Bgra,
}

/// Converts `width`x`height` packed pixels with `stride` bytes per row.
///
/// An odd dimension loses its last row or column: 4:2:0 needs even sizes,
/// and cropping one edge pixel invents no content, where padding would.
///
/// # Errors
/// Returns the conversion error when `pixels` is shorter than the stride and
/// size require.
pub fn convert(
    layout: Packed,
    pixels: &[u8],
    stride: usize,
    width: u32,
    height: u32,
) -> Result<Nv12, YuvError> {
    let (width, height) = (width & !1, height & !1);
    let luma_len = width as usize * height as usize;
    let mut data = vec![0u8; luma_len + luma_len / 2];
    let (luma, chroma) = data.split_at_mut(luma_len);
    let mut image = YuvBiPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(luma),
        y_stride: width,
        uv_plane: BufferStoreMut::Borrowed(chroma),
        uv_stride: width,
        width,
        height,
    };
    let stride = u32::try_from(stride).expect("a row stride fits u32");
    match layout {
        Packed::Rgb => rgb_to_yuv_nv12(&mut image, pixels, stride, RANGE, MATRIX, MODE)?,
        #[cfg(target_os = "macos")]
        Packed::Bgra => bgra_to_yuv_nv12(&mut image, pixels, stride, RANGE, MATRIX, MODE)?,
    }
    Ok(Nv12 {
        data,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_sizes_lose_the_last_row_and_column() {
        let rgb = vec![128u8; 5 * 3 * 3];
        let frame = convert(Packed::Rgb, &rgb, 5 * 3, 5, 3).expect("convert");
        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(frame.data.len(), 4 * 2 * 3 / 2);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn bgra_honours_the_row_stride() {
        // 4x2 pixels in rows padded to 20 bytes; the padding is poison.
        let mut bgra = vec![0xFFu8; 20 * 2];
        for row in bgra.as_chunks_mut::<20>().0 {
            for px in row[..16].as_chunks_mut::<4>().0 {
                *px = [0, 0, 0, 255];
            }
        }
        let frame = convert(Packed::Bgra, &bgra, 20, 4, 2).expect("convert");
        // Opaque black in limited range is Y 16, neutral chroma 128.
        assert!(frame.data[..8].iter().all(|&y| y == 16));
        assert!(frame.data[8..].iter().all(|&c| c == 128));
    }
}
