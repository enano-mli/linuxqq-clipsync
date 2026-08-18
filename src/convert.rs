// png ↔ BMP3 转换（纯 Rust，对齐 magick BMP3 的产物：24bpp BI_RGB，无 V4/V5 头）
// wine 把 image/bmp 映射为 CF_DIB（剥掉 14 字节文件头），企微粘贴图片依赖此格式。

use image::codecs::bmp::BmpEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, ImageFormat};
use std::io::Cursor;

// 半透明像素叠白底（magick BMP3 不支持 alpha 的默认行为）
fn composite_white(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 255 * (255 - a as u32) + 127) / 255) as u8
}

pub fn png_to_bmp3(png: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::ImageReader::with_format(Cursor::new(png), ImageFormat::Png)
        .decode()
        .map_err(|e| format!("png 解码失败: {e}"))?;

    let (w, h) = (img.width(), img.height());
    let rgba = img.to_rgba8();

    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for px in rgba.pixels() {
        rgb.push(composite_white(px[0], px[3]));
        rgb.push(composite_white(px[1], px[3]));
        rgb.push(composite_white(px[2], px[3]));
    }

    let mut bmp = Vec::new();
    BmpEncoder::new(&mut bmp)
        .write_image(&rgb, w, h, ExtendedColorType::Rgb8)
        .map_err(|e| format!("bmp 编码失败: {e}"))?;
    Ok(bmp)
}

pub fn bmp_to_png(bmp: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::ImageReader::with_format(Cursor::new(bmp), ImageFormat::Bmp)
        .decode()
        .map_err(|e| format!("bmp 解码失败: {e}"))?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());

    let mut png = Vec::new();
    PngEncoder::new(Cursor::new(&mut png))
        .write_image(rgba.as_raw(), w, h, ExtendedColorType::Rgba8)
        .map_err(|e| format!("png 编码失败: {e}"))?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn make_png(w: u32, h: u32, px: impl Fn(u32, u32) -> Rgba<u8>) -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(w, h, |x, y| px(x, y));
        let mut buf = Vec::new();
        PngEncoder::new(Cursor::new(&mut buf))
            .write_image(img.as_raw(), w, h, ExtendedColorType::Rgba8)
            .unwrap();
        buf
    }

    #[test]
    fn roundtrip_rgb() {
        let png = make_png(16, 8, |x, y| {
            // 红/绿/蓝渐变，含 0 与 255 边界
            Rgba([(x * 16) as u8, (y * 32) as u8, 128, 255])
        });
        let bmp = png_to_bmp3(&png).unwrap();
        // BMP3 头部：'BM' + 14B 文件头 + 40B BITMAPINFOHEADER
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(u32::from_le_bytes(bmp[14..18].try_into().unwrap()), 40);
        let back = bmp_to_png(&bmp).unwrap();
        let decoded = image::load_from_memory(&back).unwrap().to_rgba8();
        let orig = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), orig.dimensions());
        for (a, b) in decoded.pixels().zip(orig.pixels()) {
            assert_eq!(a, b, "往返后像素不一致");
        }
    }

    #[test]
    fn alpha_composites_over_white() {
        // 半透明纯黑 → 应为 ~128 灰；全透明 → 纯白
        let png = make_png(2, 1, |x, _| {
            if x == 0 { Rgba([0, 0, 0, 128]) } else { Rgba([10, 20, 30, 0]) }
        });
        let bmp = png_to_bmp3(&png).unwrap();
        let decoded = image::load_from_memory(&bmp).unwrap().to_rgba8();
        let p0 = decoded.get_pixel(0, 0);
        let p1 = decoded.get_pixel(1, 0);
        assert!((p0[0] as i32 - 127).abs() <= 1, "半透明黑应≈127灰, 实际 {}", p0[0]);
        assert_eq!((p1[0], p1[1], p1[2]), (255, 255, 255), "全透明应为白");
    }

    #[test]
    fn bmp3_is_24bpp_no_palette() {
        let png = make_png(4, 4, |x, _| Rgba([(x * 60) as u8, 99, 200, 255]));
        let bmp = png_to_bmp3(&png).unwrap();
        let bpp = u16::from_le_bytes(bmp[28..30].try_into().unwrap());
        assert_eq!(bpp, 24);
        // 4px 宽 24bpp → 行 12B，补齐到 4 字节倍数无需填充
        let data_offset = u32::from_le_bytes(bmp[10..14].try_into().unwrap()) as usize;
        assert_eq!(data_offset, 54); // 14 + 40，无调色板
    }
}
