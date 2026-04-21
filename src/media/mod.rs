pub mod snap;
pub mod clip;
pub mod audio;
pub mod ptz;

/// Detect the GStreamer camera source to use.
/// Checks CLAWCAM_CAMERA_SOURCE env var first, then probes GStreamer
/// for available sources (libcamerasrc for Pi Camera, v4l2src for USB).
pub fn detect_source() -> String {
    if let Ok(source) = std::env::var("CLAWCAM_CAMERA_SOURCE") {
        return source;
    }

    // Probe GStreamer for available source elements
    gstreamer::init().ok();

    // Try libcamerasrc first (Pi Camera Module)
    if gstreamer::ElementFactory::find("libcamerasrc").is_some() {
        return "libcamerasrc".to_string();
    }

    // Fall back to v4l2src (USB webcams)
    "v4l2src".to_string()
}
