#[cfg(feature = "ffmpeg-codec")]
pub mod mpeg_impl {
    use dcmnorm_dictionary::tags;
    use dcmnorm_object::DefaultDicomObject;

    use ffmpeg_next as ffmpeg;
    use ffmpeg::codec::{self, packet::Packet};
    use ffmpeg::format::Pixel;
    use ffmpeg::frame;
    use ffmpeg::software::scaling;

    /// Splits a concatenated elementary stream into individual access-unit-sized packets using
    /// FFmpeg's own bitstream parser (`av_parser_*`, not exposed by `ffmpeg-next`'s safe API, so
    /// called directly via the `ffi` module it re-exports).
    ///
    /// This is not optional: DICOM PS3.5's video transfer syntaxes require the *entire*
    /// multi-frame bitstream in one PixelData fragment, and `avcodec_send_packet` does not
    /// reliably decode more than the first access unit when handed one packet spanning several -
    /// confirmed empirically (a real multi-frame H.264 round trip decoded frame 0 correctly, then
    /// failed with "decode_slice_header error" on every frame after). Splitting into properly
    /// bounded packets first, exactly as a real demuxer would, is what actually works.
    fn split_into_access_units(
        codec_id: codec::Id,
        decoder_context: *mut ffmpeg::ffi::AVCodecContext,
        stream: &[u8],
    ) -> Result<Vec<Vec<u8>>, String> {
        let raw_id: ffmpeg::ffi::AVCodecID = codec_id.into();
        let parser = unsafe { ffmpeg::ffi::av_parser_init(raw_id as i32) };
        if parser.is_null() {
            return Err(format!("no bitstream parser available for {codec_id:?}"));
        }

        struct ParserGuard(*mut ffmpeg::ffi::AVCodecParserContext);
        impl Drop for ParserGuard {
            fn drop(&mut self) {
                unsafe { ffmpeg::ffi::av_parser_close(self.0) };
            }
        }
        let _guard = ParserGuard(parser);

        // One call to av_parser_parse2: `buf`/`buf_size` of (null, 0) signals EOF, which is
        // required at the very end (not just when input runs out) - byte-stream parsers only
        // recognize an access unit's end once they see the *next* one's start code, so the very
        // last access unit sits in the parser's internal buffer until explicitly flushed. Without
        // this, the last frame of every stream would silently go missing.
        let parse_one = |parser: *mut ffmpeg::ffi::AVCodecParserContext, buf: &[u8]| -> Result<(i32, Vec<u8>), String> {
            let mut out_ptr: *mut u8 = std::ptr::null_mut();
            let mut out_size: std::os::raw::c_int = 0;
            let consumed = unsafe {
                ffmpeg::ffi::av_parser_parse2(
                    parser,
                    decoder_context,
                    &mut out_ptr,
                    &mut out_size,
                    buf.as_ptr(),
                    buf.len() as std::os::raw::c_int,
                    ffmpeg::ffi::AV_NOPTS_VALUE,
                    ffmpeg::ffi::AV_NOPTS_VALUE,
                    0,
                )
            };
            if consumed < 0 {
                return Err("bitstream parser error while splitting video access units".to_owned());
            }
            let packet = if out_size > 0 && !out_ptr.is_null() {
                unsafe { std::slice::from_raw_parts(out_ptr, out_size as usize) }.to_vec()
            } else {
                Vec::new()
            };
            Ok((consumed, packet))
        };

        let mut packets = Vec::new();
        let mut remaining = stream;
        while !remaining.is_empty() {
            let (consumed, packet) = parse_one(parser, remaining)?;
            if !packet.is_empty() {
                packets.push(packet);
            }
            if consumed == 0 {
                // Parser made no progress - stop rather than loop forever on malformed input.
                break;
            }
            remaining = &remaining[consumed as usize..];
        }
        // Flush the last buffered access unit.
        let (_, packet) = parse_one(parser, &[])?;
        if !packet.is_empty() {
            packets.push(packet);
        }

        Ok(packets)
    }

    /// `ffmpeg::init()` is safe to call more than once, but does real work (registering codecs,
    /// setting up the log callback) each time - do it exactly once per process.
    fn ensure_ffmpeg_init() -> Result<(), String> {
        static INIT: std::sync::Once = std::sync::Once::new();
        static INIT_RESULT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        INIT.call_once(|| {
            let _ = INIT_RESULT.set(ffmpeg::init().map_err(|e| format!("ffmpeg::init failed: {e}")));
        });
        INIT_RESULT.get().cloned().unwrap_or(Ok(()))
    }

    fn codec_id_for_transfer_syntax(uid: &str) -> Option<codec::Id> {
        match uid {
            "1.2.840.10008.1.2.4.100" | "1.2.840.10008.1.2.4.101" => Some(codec::Id::MPEG2VIDEO),
            "1.2.840.10008.1.2.4.102"
            | "1.2.840.10008.1.2.4.103"
            | "1.2.840.10008.1.2.4.104"
            | "1.2.840.10008.1.2.4.105"
            | "1.2.840.10008.1.2.4.106" => Some(codec::Id::H264),
            "1.2.840.10008.1.2.4.107" | "1.2.840.10008.1.2.4.108" => Some(codec::Id::HEVC),
            _ => None,
        }
    }

    fn transfer_syntax_uid(object: &DefaultDicomObject) -> String {
        object.meta().transfer_syntax.trim_end_matches(['\0', ' ']).to_owned()
    }

    /// Decode MPEG2/H.264/HEVC-encoded pixel data using FFmpeg's raw codec API (not a
    /// container/demuxer - DICOM stores a bare elementary stream, not an MP4/MPEG-TS wrapper).
    ///
    /// PS3.5's video transfer syntaxes (as opposed to their "Fragmentable" siblings, which this
    /// codebase doesn't register - see entries.rs) require the entire bitstream in a single
    /// PixelData fragment; fragments are still concatenated defensively here in case a peer
    /// split one anyway. The concatenated stream is then split into individual access units via
    /// `split_into_access_units` before being handed to the decoder one packet at a time - see
    /// that function's own doc comment for why a single "whole stream as one packet" send_packet
    /// call isn't reliable for more than the first frame.
    pub fn decode_mpeg_pixel_data(object: &DefaultDicomObject) -> Result<Vec<u8>, String> {
        ensure_ffmpeg_init()?;

        let ts_uid = transfer_syntax_uid(object);
        let codec_id = codec_id_for_transfer_syntax(&ts_uid)
            .ok_or_else(|| format!("unsupported video transfer syntax: {ts_uid}"))?;

        let fragments = object
            .element(tags::PIXEL_DATA)
            .map_err(|e| format!("missing PixelData: {e}"))?
            .fragments()
            .ok_or_else(|| "expected encapsulated video pixel data".to_owned())?;
        if fragments.is_empty() {
            return Err("no video data to decode".to_owned());
        }
        let mut bitstream = Vec::new();
        for fragment in fragments {
            bitstream.extend_from_slice(fragment);
        }

        let rows = object
            .get(tags::ROWS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Rows attribute".to_owned())?;
        let cols = object
            .get(tags::COLUMNS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Columns attribute".to_owned())?;
        let samples_per_pixel =
            object.get(tags::SAMPLES_PER_PIXEL).and_then(|e| e.uint16().ok()).unwrap_or(1);
        let dst_format = if samples_per_pixel == 1 { Pixel::GRAY8 } else { Pixel::RGB24 };

        let decoder_codec = ffmpeg::decoder::find(codec_id)
            .ok_or_else(|| format!("no {codec_id:?} decoder available in this FFmpeg build"))?;
        let mut decoder = ffmpeg::decoder::new()
            .open_as(decoder_codec)
            .map_err(|e| format!("failed to open {codec_id:?} decoder: {e}"))?
            .video()
            .map_err(|e| format!("{codec_id:?} decoder is not a video decoder: {e}"))?;

        let mut scaler: Option<scaling::Context> = None;
        let mut out = Vec::new();
        let mut raw = frame::Video::empty();

        let mut drain_frames = |decoder: &mut ffmpeg::decoder::Video| -> Result<(), String> {
            loop {
                match decoder.receive_frame(&mut raw) {
                    Ok(()) => {
                        if scaler.is_none() {
                            scaler = Some(
                                scaling::Context::get(
                                    decoder.format(),
                                    decoder.width(),
                                    decoder.height(),
                                    dst_format,
                                    u32::from(cols),
                                    u32::from(rows),
                                    scaling::flag::Flags::BILINEAR,
                                )
                                .map_err(|e| format!("failed to create pixel format converter: {e}"))?,
                            );
                        }
                        let mut converted = frame::Video::empty();
                        scaler
                            .as_mut()
                            .unwrap()
                            .run(&raw, &mut converted)
                            .map_err(|e| format!("pixel format conversion failed: {e}"))?;

                        let bytes_per_row = usize::from(cols) * usize::from(samples_per_pixel);
                        let stride = converted.stride(0);
                        let data = converted.data(0);
                        for y in 0..usize::from(rows) {
                            let start = y * stride;
                            out.extend_from_slice(&data[start..start + bytes_per_row]);
                        }
                    }
                    Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => break,
                    Err(ffmpeg::Error::Eof) => break,
                    Err(e) => return Err(format!("video decode failed: {e}")),
                }
            }
            Ok(())
        };

        let decoder_context_ptr = unsafe { decoder.as_mut_ptr() };
        let access_units = split_into_access_units(codec_id, decoder_context_ptr, &bitstream)?;
        if access_units.is_empty() {
            return Err("bitstream parser found no access units to decode".to_owned());
        }
        for access_unit in &access_units {
            decoder
                .send_packet(&Packet::copy(access_unit))
                .map_err(|e| format!("failed to send video data to decoder: {e}"))?;
            drain_frames(&mut decoder)?;
        }
        decoder.send_eof().map_err(|e| format!("failed to flush decoder: {e}"))?;
        drain_frames(&mut decoder)?;

        if out.is_empty() {
            return Err("no frames decoded".to_owned());
        }
        Ok(out)
    }

    /// Frames per second to encode at: `CineRate` (0018,0040) if present, else derived from
    /// `FrameTime` (0018,1063, milliseconds/frame), else a plain default. Real-world cine
    /// loops nearly always carry one of these; the default only matters for synthetic inputs.
    fn frames_per_second(object: &DefaultDicomObject) -> f64 {
        if let Some(rate) =
            object.get(tags::CINE_RATE).and_then(|e| e.to_str().ok()).and_then(|s| s.trim().parse::<f64>().ok())
        {
            if rate > 0.0 {
                return rate;
            }
        }
        if let Some(frame_time_ms) = object
            .get(tags::FRAME_TIME)
            .and_then(|e| e.to_str().ok())
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            if frame_time_ms > 0.0 {
                return 1000.0 / frame_time_ms;
            }
        }
        30.0
    }

    /// Encode raw native pixel data to MPEG2/H.264/HEVC using FFmpeg's raw codec API, producing
    /// a single fragment holding the entire bitstream - the encoded-side mirror of
    /// `decode_mpeg_pixel_data`'s "one fragment, whole stream" convention (see its own doc
    /// comment for why).
    pub fn encode_mpeg_pixel_data(
        object: &DefaultDicomObject,
        target_uid: &str,
    ) -> Result<Vec<Vec<u8>>, String> {
        ensure_ffmpeg_init()?;

        let codec_id = codec_id_for_transfer_syntax(target_uid)
            .ok_or_else(|| format!("unsupported video transfer syntax: {target_uid}"))?;

        let rows = object
            .get(tags::ROWS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Rows attribute".to_owned())?;
        let cols = object
            .get(tags::COLUMNS)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing Columns attribute".to_owned())?;
        let samples_per_pixel =
            object.get(tags::SAMPLES_PER_PIXEL).and_then(|e| e.uint16().ok()).unwrap_or(1);
        let bits_allocated = object
            .get(tags::BITS_ALLOCATED)
            .and_then(|e| e.uint16().ok())
            .ok_or_else(|| "missing BitsAllocated attribute".to_owned())?;
        if bits_allocated != 8 {
            return Err(format!(
                "video encoding only supports 8-bit samples, got BitsAllocated={bits_allocated}"
            ));
        }
        let number_of_frames = object
            .get(tags::NUMBER_OF_FRAMES)
            .and_then(|e| e.to_str().ok())
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);

        let pixel_data = object
            .element(tags::PIXEL_DATA)
            .map_err(|e| format!("missing PixelData: {e}"))?
            .to_bytes()
            .map_err(|e| format!("failed to access pixel data: {e}"))?
            .to_vec();

        let src_format = if samples_per_pixel == 1 { Pixel::GRAY8 } else { Pixel::RGB24 };
        let frame_len = usize::from(rows) * usize::from(cols) * usize::from(samples_per_pixel);
        let expected_len = frame_len * number_of_frames;
        if pixel_data.len() < expected_len {
            return Err(format!(
                "PixelData too short for {number_of_frames} frame(s) of {rows}x{cols}x{samples_per_pixel}: \
                 expected at least {expected_len} bytes, got {}",
                pixel_data.len()
            ));
        }

        let encoder_codec = ffmpeg::encoder::find(codec_id)
            .ok_or_else(|| format!("no {codec_id:?} encoder available in this FFmpeg build"))?;
        let fps = frames_per_second(object);
        let time_base = ffmpeg::Rational::new(1, (fps.round() as i32).max(1));

        let mut encoder = codec::context::Context::new_with_codec(encoder_codec)
            .encoder()
            .video()
            .map_err(|e| format!("failed to create {codec_id:?} encoder context: {e}"))?;
        encoder.set_width(u32::from(cols));
        encoder.set_height(u32::from(rows));
        // YUV 4:2:0 is what every target codec here actually negotiates/decodes correctly in
        // practice (RGB-native encoding profiles exist for some of these codecs but aren't what
        // real-world DICOM viewers expect); the scaler below converts from the source format.
        encoder.set_format(Pixel::YUV420P);
        encoder.set_time_base(time_base);
        encoder.set_frame_rate(Some(time_base.invert()));

        let mut encoder = encoder
            .open()
            .map_err(|e| format!("failed to open {codec_id:?} encoder: {e}"))?;

        let mut scaler = scaling::Context::get(
            src_format,
            u32::from(cols),
            u32::from(rows),
            Pixel::YUV420P,
            u32::from(cols),
            u32::from(rows),
            scaling::flag::Flags::BILINEAR,
        )
        .map_err(|e| format!("failed to create pixel format converter: {e}"))?;

        let mut bitstream = Vec::new();
        let mut encoded = Packet::empty();

        let mut drain_packets = |encoder: &mut ffmpeg::encoder::Video, bitstream: &mut Vec<u8>| -> Result<(), String> {
            loop {
                match encoder.receive_packet(&mut encoded) {
                    Ok(()) => {
                        if let Some(data) = encoded.data() {
                            bitstream.extend_from_slice(data);
                        }
                    }
                    Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => break,
                    Err(ffmpeg::Error::Eof) => break,
                    Err(e) => return Err(format!("video encode failed: {e}")),
                }
            }
            Ok(())
        };

        for frame_index in 0..number_of_frames {
            let start = frame_index * frame_len;
            let frame_bytes = &pixel_data[start..start + frame_len];

            let mut src_frame = frame::Video::new(src_format, u32::from(cols), u32::from(rows));
            let src_bytes_per_row = usize::from(cols) * usize::from(samples_per_pixel);
            let src_stride = src_frame.stride(0);
            {
                let dst = src_frame.data_mut(0);
                for y in 0..usize::from(rows) {
                    let dst_start = y * src_stride;
                    let src_start = y * src_bytes_per_row;
                    dst[dst_start..dst_start + src_bytes_per_row]
                        .copy_from_slice(&frame_bytes[src_start..src_start + src_bytes_per_row]);
                }
            }

            let mut yuv_frame = frame::Video::empty();
            scaler
                .run(&src_frame, &mut yuv_frame)
                .map_err(|e| format!("pixel format conversion failed: {e}"))?;
            yuv_frame.set_pts(Some(frame_index as i64));

            encoder
                .send_frame(&yuv_frame)
                .map_err(|e| format!("failed to send frame {frame_index} to encoder: {e}"))?;
            drain_packets(&mut encoder, &mut bitstream)?;
        }

        encoder.send_eof().map_err(|e| format!("failed to flush encoder: {e}"))?;
        drain_packets(&mut encoder, &mut bitstream)?;

        if bitstream.is_empty() {
            return Err("encoder produced no data".to_owned());
        }
        Ok(vec![bitstream])
    }
}

#[cfg(not(feature = "ffmpeg-codec"))]
pub mod mpeg_impl {
    use dcmnorm_object::DefaultDicomObject;

    pub fn decode_mpeg_pixel_data(_object: &DefaultDicomObject) -> Result<Vec<u8>, String> {
        Err("MPEG codec support requires 'ffmpeg-codec' feature to be enabled".to_owned())
    }

    pub fn encode_mpeg_pixel_data(
        _object: &DefaultDicomObject,
        _target_uid: &str,
    ) -> Result<Vec<Vec<u8>>, String> {
        Err("MPEG codec support requires 'ffmpeg-codec' feature to be enabled".to_owned())
    }
}

pub use mpeg_impl::{decode_mpeg_pixel_data, encode_mpeg_pixel_data};

#[cfg(all(test, feature = "ffmpeg-codec"))]
mod tests {
    use super::{decode_mpeg_pixel_data, encode_mpeg_pixel_data};
    use dcmnorm_core::value::PixelFragmentSequence;
    use dcmnorm_core::{DataElement, PrimitiveValue, VR};
    use dcmnorm_dictionary::tags;
    use dcmnorm_object::{DefaultDicomObject, FileMetaTableBuilder};

    const ROWS: u16 = 120;
    const COLS: u16 = 256;
    const FRAME_LEN: usize = ROWS as usize * COLS as usize * 3;

    /// A red band across the top `count` rows, black elsewhere - deliberately not something a
    /// real X-ray/ultrasound frame would ever look like, so any cross-frame bleed or reordering
    /// bug shows up as an obviously wrong ratio rather than a subtle pixel-value drift.
    fn solid_band_frame(band_rows_from_top: usize, color: [u8; 3]) -> Vec<u8> {
        let mut frame = vec![0u8; FRAME_LEN];
        for y in 0..band_rows_from_top {
            for x in 0..usize::from(COLS) {
                let i = (y * usize::from(COLS) + x) * 3;
                frame[i..i + 3].copy_from_slice(&color);
            }
        }
        frame
    }

    /// Fraction of the top 20 rows that are (approximately) the given color - lossy video
    /// compression means "exactly equal" isn't the right check.
    fn top_band_match_ratio(frame: &[u8], color: [u8; 3]) -> f64 {
        let mut matches = 0usize;
        let checked = 20 * usize::from(COLS);
        for i in 0..checked {
            let px = &frame[i * 3..i * 3 + 3];
            if (0..3).all(|c| (i32::from(px[c]) - i32::from(color[c])).abs() < 40) {
                matches += 1;
            }
        }
        matches as f64 / checked as f64
    }

    fn empty_object_with_transfer_syntax(ts_uid: &str) -> DefaultDicomObject {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(ts_uid)
            .build()
            .expect("minimal meta table should build");
        DefaultDicomObject::new_empty_with_meta(meta)
    }

    fn native_rgb_object(number_of_frames: usize, pixel_data: Vec<u8>) -> DefaultDicomObject {
        // Native (uncompressed) source pixel data - the transfer syntax here doesn't matter to
        // encode_mpeg_pixel_data (it only reads the target UID passed to it separately), so
        // Explicit VR Little Endian just documents "this is native, not encapsulated".
        let mut object = empty_object_with_transfer_syntax("1.2.840.10008.1.2.1");
        for element in [
            DataElement::new(tags::ROWS, VR::US, PrimitiveValue::from(ROWS)),
            DataElement::new(tags::COLUMNS, VR::US, PrimitiveValue::from(COLS)),
            DataElement::new(tags::SAMPLES_PER_PIXEL, VR::US, PrimitiveValue::from(3u16)),
            DataElement::new(tags::BITS_ALLOCATED, VR::US, PrimitiveValue::from(8u16)),
            DataElement::new(tags::PIXEL_REPRESENTATION, VR::US, PrimitiveValue::from(0u16)),
            DataElement::new(tags::PLANAR_CONFIGURATION, VR::US, PrimitiveValue::from(0u16)),
            DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from("RGB".to_owned()),
            ),
            DataElement::new(
                tags::NUMBER_OF_FRAMES,
                VR::IS,
                PrimitiveValue::from(number_of_frames.to_string()),
            ),
            DataElement::new(tags::PIXEL_DATA, VR::OW, PrimitiveValue::from(pixel_data)),
        ] {
            object.put(element);
        }
        object
    }

    /// `decode_mpeg_pixel_data` dispatches on `object.meta().transfer_syntax`, so the returned
    /// object's meta table must actually carry `ts_uid` - unlike the dataset elements, which are
    /// just cloned from `source` unchanged.
    fn encapsulated_video_object(
        source: &DefaultDicomObject,
        ts_uid: &str,
        bitstream: Vec<u8>,
    ) -> DefaultDicomObject {
        let mut object = empty_object_with_transfer_syntax(ts_uid);
        for element in source.iter() {
            object.put(element.clone());
        }
        object.put(DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            PixelFragmentSequence::new(vec![0], vec![bitstream]),
        ));
        object
    }

    /// Three solid-color-band frames, each visually unambiguous, round-tripped through real
    /// libx264 encode and libavcodec decode (not mocked). This is the primary regression test
    /// for `split_into_access_units`: earlier in development, sending the whole 3-frame
    /// bitstream to the decoder as a single packet decoded frame 0 correctly and then failed
    /// with "decode_slice_header error" on every frame after - a bug a single-frame test can't
    /// catch, which is why this uses three.
    #[test]
    fn h264_multi_frame_round_trip_preserves_frame_content_and_order() {
        let frame0 = solid_band_frame(0, [0, 0, 0]);
        let frame1 = solid_band_frame(20, [255, 0, 0]);
        let frame2 = solid_band_frame(20, [0, 255, 0]);
        let mut pixel_data = Vec::with_capacity(FRAME_LEN * 3);
        pixel_data.extend_from_slice(&frame0);
        pixel_data.extend_from_slice(&frame1);
        pixel_data.extend_from_slice(&frame2);

        let source = native_rgb_object(3, pixel_data);
        let fragments = encode_mpeg_pixel_data(&source, "1.2.840.10008.1.2.4.102")
            .expect("H.264 encode should succeed");
        assert_eq!(fragments.len(), 1, "PS3.5 requires the whole bitstream in one fragment");

        let encoded = encapsulated_video_object(&source, "1.2.840.10008.1.2.4.102", fragments[0].clone());
        let decoded = decode_mpeg_pixel_data(&encoded).expect("H.264 decode should succeed");
        assert_eq!(decoded.len(), FRAME_LEN * 3, "expected all 3 frames back");

        let decoded_frames: Vec<&[u8]> = decoded.chunks_exact(FRAME_LEN).collect();
        assert!(top_band_match_ratio(decoded_frames[0], [0, 0, 0]) > 0.9, "frame 0 should stay black");
        assert!(top_band_match_ratio(decoded_frames[1], [255, 0, 0]) > 0.9, "frame 1 should be red");
        assert!(top_band_match_ratio(decoded_frames[2], [0, 255, 0]) > 0.9, "frame 2 should be green");
    }

    /// Same multi-frame round trip for MPEG2 and HEVC, through the same shared
    /// `codec_id_for_transfer_syntax` dispatch H.264 uses above - HEVC in particular is worth
    /// covering separately since x265 (unlike this test's libx264 settings) does use B-frames by
    /// default, exercising the decoder's PTS-based frame reordering rather than just simple
    /// decode-order-equals-display-order sequences.
    #[test]
    fn mpeg2_and_hevc_multi_frame_round_trips_preserve_frame_order() {
        let frame0 = solid_band_frame(0, [0, 0, 0]);
        let frame1 = solid_band_frame(20, [255, 0, 0]);
        let frame2 = solid_band_frame(20, [0, 255, 0]);
        let mut pixel_data = Vec::with_capacity(FRAME_LEN * 3);
        pixel_data.extend_from_slice(&frame0);
        pixel_data.extend_from_slice(&frame1);
        pixel_data.extend_from_slice(&frame2);
        let source = native_rgb_object(3, pixel_data);

        for ts_uid in ["1.2.840.10008.1.2.4.100", "1.2.840.10008.1.2.4.107"] {
            let fragments = encode_mpeg_pixel_data(&source, ts_uid)
                .unwrap_or_else(|e| panic!("encode for {ts_uid} should succeed: {e}"));
            let encoded = encapsulated_video_object(&source, ts_uid, fragments[0].clone());
            let decoded = decode_mpeg_pixel_data(&encoded)
                .unwrap_or_else(|e| panic!("decode for {ts_uid} should succeed: {e}"));
            assert_eq!(decoded.len(), FRAME_LEN * 3, "{ts_uid}: expected all 3 frames back");

            let decoded_frames: Vec<&[u8]> = decoded.chunks_exact(FRAME_LEN).collect();
            assert!(
                top_band_match_ratio(decoded_frames[0], [0, 0, 0]) > 0.85,
                "{ts_uid}: frame 0 should stay black"
            );
            assert!(
                top_band_match_ratio(decoded_frames[1], [255, 0, 0]) > 0.85,
                "{ts_uid}: frame 1 should be red"
            );
            assert!(
                top_band_match_ratio(decoded_frames[2], [0, 255, 0]) > 0.85,
                "{ts_uid}: frame 2 should be green"
            );
        }
    }

    #[test]
    fn encode_rejects_unsupported_transfer_syntax() {
        let source = native_rgb_object(1, vec![0u8; FRAME_LEN]);
        let result = encode_mpeg_pixel_data(&source, "1.2.840.10008.1.2.4.90");
        assert!(result.is_err(), "expected an error, got {result:?}");
    }
}
