//! Video encoding for `video`: H.264 MP4 through AVFoundation (VideoToolbox hardware encoder on
//! Apple Silicon, system framework, no extra licence) or animated GIF (pure Rust), plus a
//! contact sheet of key frames for the MCP response.

use std::path::Path;

/// An RGBA8 frame.
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

#[allow(deprecated)]
pub fn write_mp4(path: &Path, frames: &[Frame], fps: u32) -> anyhow::Result<()> {
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_av_foundation::{
        AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus, AVFileTypeMPEG4,
        AVMediaTypeVideo, AVVideoCodecKey, AVVideoCodecTypeH264, AVVideoHeightKey, AVVideoWidthKey,
    };
    use objc2_core_media::CMTime;
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
    };
    use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

    anyhow::ensure!(!frames.is_empty(), "no frames");
    let (w, h) = (frames[0].w, frames[0].h);
    anyhow::ensure!(w % 2 == 0 && h % 2 == 0, "H.264 needs even dimensions ({w}x{h})");
    let _ = std::fs::remove_file(path); // AVAssetWriter refuses to overwrite
    unsafe {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let writer = AVAssetWriter::initWithURL_fileType_error(AVAssetWriter::alloc(), &url, AVFileTypeMPEG4.unwrap())
            .map_err(|e| anyhow::anyhow!("AVAssetWriter: {}", e.localizedDescription()))?;
        let wn = NSNumber::new_u32(w);
        let hn = NSNumber::new_u32(h);
        let keys: [&NSString; 3] = [AVVideoCodecKey.unwrap(), AVVideoWidthKey.unwrap(), AVVideoHeightKey.unwrap()];
        let vals: [&AnyObject; 3] = [AVVideoCodecTypeH264.unwrap().as_ref(), wn.as_ref(), hn.as_ref()];
        let settings: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(&keys, &vals);
        let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(AVMediaTypeVideo.unwrap(), Some(&settings));
        input.setExpectsMediaDataInRealTime(false);
        let adaptor = AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(&input, None);
        anyhow::ensure!(writer.canAddInput(&input), "cannot add video input");
        writer.addInput(&input);
        anyhow::ensure!(writer.startWriting(), "startWriting failed: {:?}", writer.error().map(|e| e.localizedDescription().to_string()));
        writer.startSessionAtSourceTime(CMTime::new(0, fps as i32));
        for (k, f) in frames.iter().enumerate() {
            let mut pb: *mut CVPixelBuffer = std::ptr::null_mut();
            let rc = CVPixelBufferCreate(None, w as usize, h as usize, kCVPixelFormatType_32BGRA, None, std::ptr::NonNull::from(&mut pb));
            anyhow::ensure!(rc == 0 && !pb.is_null(), "CVPixelBufferCreate failed ({rc})");
            let pb_ref = &*pb;
            CVPixelBufferLockBaseAddress(pb_ref, CVPixelBufferLockFlags(0));
            let base = CVPixelBufferGetBaseAddress(pb_ref) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(pb_ref);
            for y in 0..h as usize {
                let row = std::slice::from_raw_parts_mut(base.add(y * stride), w as usize * 4);
                let src = &f.rgba[y * w as usize * 4..(y + 1) * w as usize * 4];
                for (d, s) in row.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = 255;
                }
            }
            CVPixelBufferUnlockBaseAddress(pb_ref, CVPixelBufferLockFlags(0));
            let t0 = std::time::Instant::now();
            while !input.isReadyForMoreMediaData() {
                anyhow::ensure!(t0.elapsed().as_secs() < 30, "encoder stalled");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let ok = adaptor.appendPixelBuffer_withPresentationTime(pb_ref, CMTime::new(k as i64, fps as i32));
            objc2_core_foundation::CFRetained::<CVPixelBuffer>::from_raw(std::ptr::NonNull::new_unchecked(pb));
            anyhow::ensure!(ok, "appending frame {k} failed: {:?}", writer.error().map(|e| e.localizedDescription().to_string()));
        }
        input.markAsFinished();
        anyhow::ensure!(writer.finishWriting(), "finishWriting failed: {:?}", writer.error().map(|e| e.localizedDescription().to_string()));
        anyhow::ensure!(writer.status() == AVAssetWriterStatus::Completed, "writer status {:?}", writer.status());
    }
    Ok(())
}

pub fn write_gif(path: &Path, frames: &[Frame], fps: u32) -> anyhow::Result<()> {
    anyhow::ensure!(!frames.is_empty(), "no frames");
    let (w, h) = (frames[0].w as u16, frames[0].h as u16);
    let mut file = std::fs::File::create(path)?;
    let mut enc = gif::Encoder::new(&mut file, w, h, &[])?;
    enc.set_repeat(gif::Repeat::Infinite)?;
    let delay = (100.0 / fps as f32).round().max(2.0) as u16;
    for f in frames {
        let mut px = f.rgba.clone();
        let mut fr = gif::Frame::from_rgba_speed(w, h, &mut px, 10);
        fr.delay = delay;
        enc.write_frame(&fr)?;
    }
    Ok(())
}

/// Box-filter downscale of an RGBA frame to fit within `max_w` x `max_h`.
pub fn downscale(f: &Frame, max_w: u32, max_h: u32) -> Frame {
    let s = (f.w as f32 / max_w as f32).max(f.h as f32 / max_h as f32).max(1.0);
    let (w, h) = (((f.w as f32 / s) as u32).max(1), ((f.h as f32 / s) as u32).max(1));
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let (y0, y1) = ((y as f32 * s) as u32, (((y + 1) as f32 * s) as u32).min(f.h).max((y as f32 * s) as u32 + 1));
        for x in 0..w {
            let (x0, x1) = ((x as f32 * s) as u32, (((x + 1) as f32 * s) as u32).min(f.w).max((x as f32 * s) as u32 + 1));
            let mut acc = [0u32; 4];
            let mut n = 0;
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let i = ((yy * f.w + xx) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += f.rgba[i + c] as u32;
                    }
                    n += 1;
                }
            }
            let o = ((y * w + x) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n.max(1)) as u8;
            }
        }
    }
    Frame { w, h, rgba: out }
}

/// Tile frames into a sheet (row-major) with 1 px separators.
pub fn sheet(frames: &[Frame], cols: u32) -> Frame {
    let (tw, th) = (frames[0].w, frames[0].h);
    let rows = (frames.len() as u32).div_ceil(cols);
    let (w, h) = (tw * cols + cols - 1, th * rows + rows - 1);
    let mut px = vec![170u8; (w * h * 4) as usize];
    for (k, f) in frames.iter().enumerate() {
        let (ox, oy) = ((k as u32 % cols) * (tw + 1), (k as u32 / cols) * (th + 1));
        for y in 0..f.h.min(th) {
            let src = &f.rgba[(y * f.w * 4) as usize..((y * f.w + f.w.min(tw)) * 4) as usize];
            let o = (((oy + y) * w + ox) * 4) as usize;
            px[o..o + src.len()].copy_from_slice(src);
        }
    }
    Frame { w, h, rgba: px }
}

pub fn encode_png(f: &Frame) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, f.w, f.h);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        e.set_compression(png::Compression::Fast);
        let rgb: Vec<u8> = f.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        e.write_header()?.write_image_data(&rgb)?;
    }
    Ok(out)
}

