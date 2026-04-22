use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::mpsc;
use tracing::{info, warn};

/// A frame captured from the GStreamer pipeline.
pub struct Frame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Build a GStreamer pipeline that captures frames for inference, JPEG snapshots,
/// and optionally publishes H.264 RTSP to an external relay (e.g. MediaMTX).
///
/// Pipeline layout:
///   source → convert → scale → capsfilter → tee
///     tee → queue → jpegenc → jpeg_sink     (snapshots / webhook images)
///     tee → queue → videorate → convert → capsfilter(RGB,2fps) → rgb_sink  (YOLO)
///     tee → queue → convert → capsfilter(NV12) → v4l2h264enc → h264parse → rtspclientsink
///         (stream branch, only present when stream_url is Some)
pub fn create_pipeline(
    source_name: &str,
    width: u32,
    height: u32,
    fps: u32,
    stream_url: Option<&str>,
) -> Result<(mpsc::Receiver<Frame>, gst::Pipeline)> {
    gst::init().context("failed to initialize GStreamer")?;

    let pipeline = gst::Pipeline::default();

    let source = gst::ElementFactory::make(source_name)
        .build()
        .context(format!("failed to create {source_name}"))?;

    // For USB webcams (v4l2src), the sensor emits MJPEG — not the NV12 raw our
    // capsfilter below demands — so we need jpegdec + videoconvert between the
    // source and the NV12 capsfilter. libcamerasrc natively delivers NV12 and
    // can skip these.
    let needs_jpeg_decode = source_name == "v4l2src";
    let src_caps = if needs_jpeg_decode {
        Some(
            gst::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    gst::Caps::builder("image/jpeg")
                        .field("width", width as i32)
                        .field("height", height as i32)
                        .field("framerate", gst::Fraction::new(fps as i32, 1))
                        .build(),
                )
                .build()?,
        )
    } else {
        None
    };
    // Prefer the Pi's VideoCore hardware MJPEG decoder (`v4l2jpegdec`) when
    // available. USB MJPEG cams produce compressed frames that the ARM has to
    // decompress — at 720p30 software `jpegdec` burns 70%+ of a core. Moving
    // the decode onto `bcm2835-codec` drops CPU dramatically.
    //
    // Caveat: `v4l2jpegdec` outputs DMABuf video, and the GStreamer build on
    // Raspberry Pi OS Trixie has `videoflip` that refuses DMABuf-derived
    // buffers (negotiation silently stalls). We skip HW decode when the
    // pipeline also needs videoflip; when rotation is identity we can drop
    // videoflip entirely and get the HW win. Set CLAWCAM_HW_DECODE=0 to
    // force SW if a particular cam negotiates poorly.
    let flip_method = rotate_method(std::env::var("CLAWCAM_ROTATE").ok().as_deref());
    let needs_videoflip = flip_method != "identity";
    let prefer_hw_decode = needs_jpeg_decode
        && !needs_videoflip
        && std::env::var("CLAWCAM_HW_DECODE").ok().as_deref() != Some("0");
    let jpegdec = if needs_jpeg_decode {
        let (elem, kind) = if prefer_hw_decode {
            match gst::ElementFactory::make("v4l2jpegdec").build() {
                Ok(e) => (e, "v4l2jpegdec (hw)"),
                Err(_) => (gst::ElementFactory::make("jpegdec").build()?, "jpegdec (sw)"),
            }
        } else {
            let reason = if needs_videoflip { " (videoflip active — HW decode blocked)" } else { "" };
            (gst::ElementFactory::make("jpegdec").build()?, Box::leak(format!("jpegdec (sw){reason}").into_boxed_str()) as &'static str)
        };
        tracing::info!("MJPEG decoder: {kind}");
        Some(elem)
    } else {
        None
    };
    let src_convert = if needs_jpeg_decode {
        Some(gst::ElementFactory::make("videoconvert").build()?)
    } else {
        None
    };

    // `videoflip` is only added to the pipeline when rotation is actually
    // requested. When it's identity, skipping the element both saves a small
    // amount of CPU (the no-op copy is still ~5-10% of a core at 720p30) and
    // lets us use HW MJPEG decode upstream (see jpegdec block above).
    let flip = if needs_videoflip {
        Some(
            gst::ElementFactory::make("videoflip")
                .property_from_str("video-direction", flip_method)
                .build()?,
        )
    } else {
        None
    };
    // IMPORTANT: format=NV12 + interlace-mode=progressive are required for
    // v4l2h264enc on Pi (bcm2835-codec) to negotiate correctly.
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", width as i32)
                .field("height", height as i32)
                .field("format", "NV12")
                .field("interlace-mode", "progressive")
                .field("framerate", gst::Fraction::new(fps as i32, 1))
                .build(),
        )
        .build()?;
    let tee = gst::ElementFactory::make("tee")
        .property("allow-not-linked", true)
        .build()?;

    // JPEG branch (for snapshots + webhook images)
    let jpeg_queue = gst::ElementFactory::make("queue").build()?;
    let jpegenc = gst::ElementFactory::make("jpegenc")
        .property("quality", 85i32)
        .build()?;
    let jpeg_sink = gst_app::AppSink::builder()
        .name("jpeg_sink")
        .max_buffers(2)
        .drop(true)
        .build();

    // RGB branch — downsampled for YOLO; queue's leaky-downstream + appsink drop
    // give us effective rate-limiting without videorate's negotiation quirks.
    let rgb_queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 1u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()?;
    let rgb_scale = gst::ElementFactory::make("videoscale").build()?;
    let rgb_convert = gst::ElementFactory::make("videoconvert").build()?;

    let is_rot90 = matches!(flip_method, "90r" | "90l");
    let (post_w, post_h) = if is_rot90 { (height, width) } else { (width, height) };
    let yolo_scale_factor: u32 = std::env::var("CLAWCAM_YOLO_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
        .max(1);
    let yolo_w = (post_w / yolo_scale_factor).max(160);
    let yolo_h = (post_h / yolo_scale_factor).max(90);
    tracing::info!("YOLO branch: {yolo_w}x{yolo_h} (1/{yolo_scale_factor} scale)");

    let rgb_caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "RGB")
                .field("width", yolo_w as i32)
                .field("height", yolo_h as i32)
                .build(),
        )
        .build()?;
    let rgb_sink = gst_app::AppSink::builder()
        .name("rgb_sink")
        .max_buffers(2)
        .drop(true)
        .build();

    pipeline.add_many([
        &source, &caps, &tee,
        &jpeg_queue, &jpegenc, jpeg_sink.upcast_ref(),
        &rgb_queue, &rgb_scale, &rgb_convert, &rgb_caps, rgb_sink.upcast_ref(),
    ])?;
    if let Some(f) = &flip {
        pipeline.add(f)?;
    }
    if let (Some(c), Some(d), Some(v)) = (&src_caps, &jpegdec, &src_convert) {
        pipeline.add_many([c, d, v])?;
    }

    // Assemble the pre-tee segment. Order matters here because GStreamer will
    // reject a link if upstream caps don't fit downstream. The final sink of
    // this segment is always `tee`; everything else depends on the cam type
    // and whether rotation is needed.
    let pre_tee: Vec<&gst::Element> = {
        let mut v: Vec<&gst::Element> = Vec::new();
        v.push(&source);
        if let (Some(c), Some(d), Some(conv)) = (&src_caps, &jpegdec, &src_convert) {
            v.push(c);
            v.push(d);
            v.push(conv);
        }
        v.push(&caps);
        if let Some(f) = &flip {
            v.push(f);
        }
        v.push(&tee);
        v
    };
    gst::Element::link_many(pre_tee.as_slice())?;

    gst::Element::link_many([&jpeg_queue, &jpegenc, jpeg_sink.upcast_ref()])?;
    tee.link_pads(None, &jpeg_queue, None)?;

    gst::Element::link_many([&rgb_queue, &rgb_scale, &rgb_convert, &rgb_caps, rgb_sink.upcast_ref()])?;
    tee.link_pads(None, &rgb_queue, None)?;

    // Optional H.264 + RTSP publish branch
    if let Some(url) = stream_url {
        match build_stream_branch(&pipeline, &tee, url) {
            Ok(()) => info!("RTSP stream branch active → {url}"),
            Err(e) => warn!("RTSP stream branch disabled: {e:#}"),
        }
    }

    // Capture RGB frames for the monitor loop (at the downsampled YOLO resolution)
    let (tx, rx) = mpsc::sync_channel::<Frame>(4);
    let w = yolo_w;
    let h = yolo_h;
    rgb_sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let _ = tx.try_send(Frame {
                    data: map.to_vec(),
                    width: w,
                    height: h,
                });
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    Ok((rx, pipeline))
}

fn build_stream_branch(pipeline: &gst::Pipeline, tee: &gst::Element, url: &str) -> Result<()> {
    let queue = gst::ElementFactory::make("queue")
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 4u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()?;

    // Pi HW H.264 encoder. `repeat_sequence_header=1` makes the encoder emit
    // SPS/PPS with every IDR so late RTSP readers can sync. The explicit
    // output caps with `level=4` is required for negotiation with rtspclientsink.
    let idr_period: i32 = std::env::var("CLAWCAM_STREAM_GOP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let encoder = gst::ElementFactory::make("v4l2h264enc")
        .property(
            "extra-controls",
            gst::Structure::builder("controls")
                .field("repeat_sequence_header", 1i32)
                .field("h264_i_frame_period", idr_period)
                .build(),
        )
        .build()
        .context("v4l2h264enc not available — install gstreamer1.0-plugins-good")?;

    let enc_caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-h264")
                .field("level", "4")
                .build(),
        )
        .build()?;

    let parse = gst::ElementFactory::make("h264parse")
        .property("config-interval", -1i32)
        .build()?;

    // Force TCP to avoid UDP packet loss under high bitrates.
    let rtsp_sink = gst::ElementFactory::make("rtspclientsink")
        .property("location", url)
        .property("latency", 100u32)
        .property_from_str("protocols", "tcp")
        .build()
        .context("rtspclientsink not available — install gstreamer1.0-rtsp")?;

    pipeline.add_many([&queue, &encoder, &enc_caps, &parse, &rtsp_sink])?;
    gst::Element::link_many([&queue, &encoder, &enc_caps, &parse, &rtsp_sink])?;
    tee.link_pads(None, &queue, None)?;
    Ok(())
}

/// Map CLAWCAM_ROTATE value → videoflip's video-direction enum.
/// Accepts: "0","90","180","-90","270","cw","ccw","flip-h","flip-v","none" (case-insensitive)
fn rotate_method(v: Option<&str>) -> &'static str {
    match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("0") | Some("none") | Some("identity") => "identity",
        Some("90") | Some("cw") | Some("clockwise") => "90r",
        Some("180") | Some("rotate-180") => "180",
        Some("-90") | Some("270") | Some("ccw") | Some("counterclockwise") => "90l",
        Some("flip-h") | Some("horizontal-flip") | Some("horiz") => "horiz",
        Some("flip-v") | Some("vertical-flip") | Some("vert") => "vert",
        Some(other) => {
            tracing::warn!("CLAWCAM_ROTATE unrecognized value {other:?}, using identity");
            "identity"
        }
    }
}

#[allow(dead_code)]
fn make_encoder() -> Result<(gst::Element, Option<gst::Element>)> {
    // Allow override; default to x264 because Pi V4L2 encoder is fragile
    // (needs gpu_mem bump + driver tweaks) and we value reliability.
    let prefer_hw = std::env::var("CLAWCAM_STREAM_HW")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);

    if prefer_hw {
        if let Ok(enc) = gst::ElementFactory::make("v4l2h264enc").build() {
            let caps = gst::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("format", "NV12")
                        .build(),
                )
                .build()?;
            return Ok((enc, Some(caps)));
        }
    }

    if let Ok(enc) = gst::ElementFactory::make("x264enc")
        .property_from_str("tune", "zerolatency")
        .property_from_str("speed-preset", "ultrafast")
        .property("bitrate", 3000u32)
        .property("key-int-max", 30u32)
        .property("bframes", 0u32)
        .build()
    {
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "I420")
                    .build(),
            )
            .build()?;
        return Ok((enc, Some(caps)));
    }

    if let Ok(enc) = gst::ElementFactory::make("v4l2h264enc").build() {
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "NV12")
                    .build(),
            )
            .build()?;
        return Ok((enc, Some(caps)));
    }

    anyhow::bail!("no H.264 encoder available (need x264enc or v4l2h264enc)")
}

/// Grab a single JPEG from the pipeline's jpeg_sink.
pub fn grab_jpeg(pipeline: &gst::Pipeline) -> Result<Vec<u8>> {
    let jpeg_sink = pipeline
        .by_name("jpeg_sink")
        .context("jpeg_sink not found")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("jpeg_sink cast failed"))?;

    let sample = jpeg_sink
        .pull_sample()
        .map_err(|_| anyhow::anyhow!("failed to pull JPEG sample"))?;
    let buffer = sample.buffer().context("no buffer in sample")?;
    let map = buffer.map_readable()?;
    Ok(map.to_vec())
}
