//! 二进制块的图片预览：只认 PNG / JPEG（够覆盖最常见的情况，依赖也小）。
//!
//! 解码结果给 egui 上传成纹理；调用方在换选中节点时把它丢掉（及时释放显存）。

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Png,
    Jpeg,
    Other,
}

/// 按魔数判断类型（`@format` 只是提示，最终看字节）。
pub fn sniff(bytes: &[u8]) -> Kind {
    if bytes.len() > 8 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        return Kind::Png;
    }
    if bytes.len() > 3 && bytes[..3] == [0xff, 0xd8, 0xff] {
        return Kind::Jpeg;
    }
    Kind::Other
}

/// 解码成 RGBA8（宽、高、像素）。不认识 / 解不开返回 None。
pub fn decode(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    match sniff(bytes) {
        Kind::Png => decode_png(bytes),
        Kind::Jpeg => decode_jpeg(bytes),
        Kind::Other => None,
    }
}

fn decode_png(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let size = reader.output_buffer_size()?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(w * h * 4);
            for px in buf[..info.buffer_size()].chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(w * h * 4);
            for g in &buf[..info.buffer_size()] {
                out.extend_from_slice(&[*g, *g, *g, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(w * h * 4);
            for px in buf[..info.buffer_size()].chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        _ => return None,
    };
    Some((w, h, rgba))
}

fn decode_jpeg(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(bytes));
    let pixels = decoder.decode().ok()?;
    let (w, h) = decoder.dimensions()?;
    let (w, h) = (w as usize, h as usize);
    let rgba = match pixels.len() {
        n if n == w * h * 4 => pixels,
        n if n == w * h * 3 => {
            let mut out = Vec::with_capacity(w * h * 4);
            for px in pixels.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            let _ = n;
            out
        }
        n if n == w * h => {
            let mut out = Vec::with_capacity(w * h * 4);
            for g in pixels {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            let _ = n;
            out
        }
        _ => return None,
    };
    Some((w, h, rgba))
}
