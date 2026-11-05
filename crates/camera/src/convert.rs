//! Camera pixel formats to I420 (limited-range BT.601).

use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::Frame;

/// A JPEG from the camera. Decoded to full-range YCbCr (what JFIF carries), then squeezed into
/// limited range and 4:2:0. `None` for a frame that does not decode.
pub fn mjpeg_to_i420(data: &[u8]) -> Option<Frame> {
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::YCbCr);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(data), opts);
    let pixels = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (usize::from(info.width), usize::from(info.height));
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || pixels.len() < w * h * 3 {
        return None;
    }
    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; w * h / 4];
    let mut v = vec![0u8; w * h / 4];
    for row in 0..h {
        for col in 0..w {
            y[row * w + col] = full_to_limited_y(pixels[(row * w + col) * 3]);
        }
    }
    // Chroma: the mean of each 2x2 block.
    for row in 0..h / 2 {
        for col in 0..w / 2 {
            let (mut cb, mut cr) = (0u32, 0u32);
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let i = ((row * 2 + dy) * w + col * 2 + dx) * 3;
                cb += u32::from(pixels[i + 1]);
                cr += u32::from(pixels[i + 2]);
            }
            u[row * (w / 2) + col] = full_to_limited_c(((cb + 2) / 4) as u8);
            v[row * (w / 2) + col] = full_to_limited_c(((cr + 2) / 4) as u8);
        }
    }
    Some(Frame {
        width: w as u32,
        height: h as u32,
        y,
        u,
        v,
    })
}

/// Packed YUYV (4:2:2, already limited range from a webcam) to planar 4:2:0.
pub fn yuyv_to_i420(data: &[u8], width: u32, height: u32) -> Option<Frame> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || data.len() < w * h * 2 {
        return None;
    }
    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; w * h / 4];
    let mut v = vec![0u8; w * h / 4];
    for row in 0..h {
        for col in 0..w / 2 {
            let i = (row * w + col * 2) * 2;
            y[row * w + col * 2] = data[i];
            y[row * w + col * 2 + 1] = data[i + 2];
            if row % 2 == 0 {
                // Chroma of an even row and the odd row below it are averaged.
                let j = i + w * 2;
                let (cb0, cr0) = (u16::from(data[i + 1]), u16::from(data[i + 3]));
                let (cb1, cr1) = (u16::from(data[j + 1]), u16::from(data[j + 3]));
                u[(row / 2) * (w / 2) + col] = (cb0 + cb1).div_ceil(2) as u8;
                v[(row / 2) * (w / 2) + col] = (cr0 + cr1).div_ceil(2) as u8;
            }
        }
    }
    Some(Frame {
        width,
        height,
        y,
        u,
        v,
    })
}

fn full_to_limited_y(y: u8) -> u8 {
    (16 + (u32::from(y) * 219 + 127) / 255) as u8
}

fn full_to_limited_c(c: u8) -> u8 {
    (128 + ((i32::from(c) - 128) * 224 + if c >= 128 { 127 } else { -127 }) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_range_maps_into_limited_range() {
        assert_eq!(full_to_limited_y(0), 16);
        assert_eq!(full_to_limited_y(255), 235);
        assert_eq!(full_to_limited_c(128), 128);
        assert_eq!(full_to_limited_c(0), 16);
        assert_eq!(full_to_limited_c(255), 240);
    }

    #[test]
    fn yuyv_is_split_into_planes() {
        // 4x2: row 0 Y 10 11 12 13, row 1 Y 20 21 22 23; chroma pairs (cb, cr).
        let data = [
            10, 100, 11, 200, 12, 110, 13, 210, // row 0
            20, 102, 21, 202, 22, 112, 23, 212, // row 1
        ];
        let f = yuyv_to_i420(&data, 4, 2).unwrap();
        assert_eq!(f.y, [10, 11, 12, 13, 20, 21, 22, 23]);
        assert_eq!(f.u, [101, 111]);
        assert_eq!(f.v, [201, 211]);
        assert!(yuyv_to_i420(&data, 4, 4).is_none());
        assert!(yuyv_to_i420(&data, 3, 2).is_none());
    }

    /// SMPTE colour bars, 64x48: from the left the seven bars are 75% white, yellow, cyan, green,
    /// magenta, red and blue. Luma of 75% white is about 180 in limited range, and the bottom
    /// strip of the pattern holds black near the left.
    #[test]
    fn a_known_jpeg_decodes_to_the_right_planes() {
        let f = mjpeg_to_i420(include_bytes!("../tests/data/bars-64x48.jpg")).expect("decodes");
        assert_eq!((f.width, f.height), (64, 48));
        assert_eq!(f.y.len(), 64 * 48);
        assert_eq!((f.u.len(), f.v.len()), (32 * 24, 32 * 24));
        let luma = |x: usize, y: usize| i32::from(f.y[y * 64 + x]);
        // Bar 0 (white, about 75%) and bar 6 (blue) near the top.
        assert!(
            (luma(4, 4) - 180).abs() <= 12,
            "white bar luma {}",
            luma(4, 4)
        );
        assert!(luma(60, 4) < 60, "blue bar luma {}", luma(60, 4));
        assert!(luma(4, 4) > luma(60, 4) + 80);
        // Chroma: the grey/white bar is neutral, the blue bar has Cb well above 128 and Cr below.
        let cb = |x: usize, y: usize| i32::from(f.u[y * 32 + x]);
        let cr = |x: usize, y: usize| i32::from(f.v[y * 32 + x]);
        assert!(
            (cb(2, 2) - 128).abs() <= 6 && (cr(2, 2) - 128).abs() <= 6,
            "white bar is not neutral"
        );
        // Inside the red bar Cr is high and Cb low, inside the blue bar the reverse. (Chroma blurs
        // across bar edges, so only interior points are checked.)
        assert!(
            cr(26, 2) > 170 && cb(26, 2) < 140,
            "red bar ({}, {})",
            cb(26, 2),
            cr(26, 2)
        );
        assert!(
            cb(30, 2) > 170 && cr(30, 2) < 140,
            "blue bar ({}, {})",
            cb(30, 2),
            cr(30, 2)
        );
        assert!(f.y.iter().all(|&b| (16..=235).contains(&b)));
    }

    #[test]
    fn junk_is_not_a_jpeg() {
        assert!(mjpeg_to_i420(b"not a jpeg").is_none());
        assert!(mjpeg_to_i420(&[]).is_none());
    }
}
