//! H.264 level selection from a stream's size and frame rate.

use mediax_codec::H264Level;

/// Limits of one level from table A-1 of ITU-T H.264: the macroblock rate
/// and the frame size in macroblocks.
struct LevelLimits {
    level: H264Level,
    max_macroblocks_per_second: u64,
    max_frame_macroblocks: u64,
}

/// Levels 3.0 through 5.2, the range the encoder takes, lowest first.
const LEVELS: [LevelLimits; 9] = [
    LevelLimits {
        level: H264Level::L3_0,
        max_macroblocks_per_second: 40_500,
        max_frame_macroblocks: 1_620,
    },
    LevelLimits {
        level: H264Level::L3_1,
        max_macroblocks_per_second: 108_000,
        max_frame_macroblocks: 3_600,
    },
    LevelLimits {
        level: H264Level::L3_2,
        max_macroblocks_per_second: 216_000,
        max_frame_macroblocks: 5_120,
    },
    LevelLimits {
        level: H264Level::L4_0,
        max_macroblocks_per_second: 245_760,
        max_frame_macroblocks: 8_192,
    },
    LevelLimits {
        level: H264Level::L4_1,
        max_macroblocks_per_second: 245_760,
        max_frame_macroblocks: 8_192,
    },
    LevelLimits {
        level: H264Level::L4_2,
        max_macroblocks_per_second: 522_240,
        max_frame_macroblocks: 8_704,
    },
    LevelLimits {
        level: H264Level::L5_0,
        max_macroblocks_per_second: 589_824,
        max_frame_macroblocks: 22_080,
    },
    LevelLimits {
        level: H264Level::L5_1,
        max_macroblocks_per_second: 983_040,
        max_frame_macroblocks: 36_864,
    },
    LevelLimits {
        level: H264Level::L5_2,
        max_macroblocks_per_second: 2_073_600,
        max_frame_macroblocks: 36_864,
    },
];

/// The lowest level that admits `width`x`height` pixels at `frames_per_second`,
/// or `None` when even level 5.2 does not.
///
/// A level also bounds each frame dimension to `sqrt(8 * MaxFS)` macroblocks,
/// so a very wide or tall stream can exceed a level its area would fit.
pub fn h264_level(width: u32, height: u32, frames_per_second: u32) -> Option<H264Level> {
    let columns = u64::from(width.div_ceil(16));
    let rows = u64::from(height.div_ceil(16));
    let frame_macroblocks = columns * rows;
    let macroblocks_per_second = frame_macroblocks * u64::from(frames_per_second);
    LEVELS
        .iter()
        .find(|limits| {
            frame_macroblocks <= limits.max_frame_macroblocks
                && macroblocks_per_second <= limits.max_macroblocks_per_second
                && columns * columns <= 8 * limits.max_frame_macroblocks
                && rows * rows <= 8 * limits.max_frame_macroblocks
        })
        .map(|limits| limits.level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_lowest_admitting_level() {
        assert_eq!(h264_level(640, 480, 30), Some(H264Level::L3_0));
        assert_eq!(h264_level(1280, 720, 30), Some(H264Level::L3_1));
        assert_eq!(h264_level(1920, 1080, 30), Some(H264Level::L4_0));
        assert_eq!(h264_level(1920, 1200, 30), Some(H264Level::L5_0));
        assert_eq!(h264_level(3840, 2160, 30), Some(H264Level::L5_1));
        assert_eq!(h264_level(3840, 2160, 60), Some(H264Level::L5_2));
    }

    #[test]
    fn a_stream_beyond_level_5_2_has_none() {
        assert_eq!(h264_level(8192, 4320, 30), None);
    }
}
