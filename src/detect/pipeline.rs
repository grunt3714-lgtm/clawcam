use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::mpsc;

/// A frame captured from the GStreamer pipeline.
pub struct Frame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Build and run a GStreamer pipeline that captures frames for inference.
///
/// `source` is a GStreamer source element string, e.g.:
/// - `libcamerasrc` (Pi Camera Module)
/// - `v4l2src device=/dev/video0` (USB webcam)
/// - `videotestsrc` (for testing)
///
/// Returns a receiver that yields JPEG frames and a pipeline handle.
pub fn create_pipeline(
    source: &str,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(mpsc::Receiver<Frame>, gst::Pipeline)> {
    gst::init().context("failed to initialize GStreamer")?;

    // Build pipeline:
    // source ! videoconvert ! videoscale ! capsfilter ! tee
    //   tee.src_0 ! queue ! jpegenc ! appsink (for snapshots / webhook images)
    //   tee.src_1 ! queue ! videoconvert ! capsfilter(RGB) ! appsink (for inference)
    let pipeline_str = format!(
        "{source} ! videoconvert ! videoscale ! \
         video/x-raw,width={width},height={height},framerate={fps}/1 ! tee name=t \
         t. ! queue ! jpegenc quality=85 ! appsink name=jpeg_sink emit-signals=true max-buffers=2 drop=true \
         t. ! queue ! videoconvert ! video/x-raw,format=RGB ! \
         appsink name=rgb_sink emit-signals=true max-buffers=2 drop=true"
    );

    let pipeline = gst::parse::launch(&pipeline_str)
        .context("failed to parse GStreamer pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("pipeline cast failed"))?;

    let (tx, rx) = mpsc::sync_channel::<Frame>(4);

    // RGB sink for inference frames
    let rgb_sink = pipeline
        .by_name("rgb_sink")
        .context("rgb_sink not found")?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("rgb_sink cast failed"))?;

    let w = width;
    let h = height;
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
