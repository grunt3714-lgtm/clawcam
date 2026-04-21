use anyhow::{Context, Result};
use ndarray::{Array, ArrayView2, s};
use ort::session::Session;
use ort::value::Tensor;

use crate::webhook::Detection;

const INPUT_SIZE: u32 = 640;
const DEFAULT_CONF_THRESHOLD: f32 = 0.6;
const IOU_THRESHOLD: f32 = 0.45;

// COCO class names
const CLASS_NAMES: &[&str] = &[
    "person", "bicycle", "car", "motorcycle", "airplane", "bus", "train", "truck",
    "boat", "traffic light", "fire hydrant", "stop sign", "parking meter", "bench",
    "bird", "cat", "dog", "horse", "sheep", "cow", "elephant", "bear", "zebra",
    "giraffe", "backpack", "umbrella", "handbag", "tie", "suitcase", "frisbee",
    "skis", "snowboard", "sports ball", "kite", "baseball bat", "baseball glove",
    "skateboard", "surfboard", "tennis racket", "bottle", "wine glass", "cup",
    "fork", "knife", "spoon", "bowl", "banana", "apple", "sandwich", "orange",
    "broccoli", "carrot", "hot dog", "pizza", "donut", "cake", "chair", "couch",
    "potted plant", "bed", "dining table", "toilet", "tv", "laptop", "mouse",
    "remote", "keyboard", "cell phone", "microwave", "oven", "toaster", "sink",
    "refrigerator", "book", "clock", "vase", "scissors", "teddy bear",
    "hair drier", "toothbrush",
];

pub struct YoloDetector {
    session: Session,
    conf_threshold: f32,
    class_allow: Option<std::collections::HashSet<String>>,
}

impl YoloDetector {
    pub fn load(model_path: &str) -> Result<Self> {
        let conf_threshold = std::env::var("CLAWCAM_CONF_THRESHOLD")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(DEFAULT_CONF_THRESHOLD)
            .clamp(0.1, 1.0);

        let class_allow = std::env::var("CLAWCAM_CLASSES").ok().map(|s| {
            s.split(',')
                .map(|x| x.trim().to_lowercase())
                .filter(|x| !x.is_empty())
                .collect::<std::collections::HashSet<_>>()
        });

        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("failed to create session builder: {e}"))?
            .with_intra_threads(4)
            .map_err(|e| anyhow::anyhow!("failed to set threads: {e}"))?
            .commit_from_file(model_path)
            .map_err(|e| anyhow::anyhow!("failed to load model from {model_path}: {e}"))?;

        tracing::info!(
            "confidence threshold: {conf_threshold}; class allowlist: {}",
            class_allow
                .as_ref()
                .map(|s| s.iter().cloned().collect::<Vec<_>>().join(","))
                .unwrap_or_else(|| "(all)".into())
        );
        Ok(Self { session, conf_threshold, class_allow })
    }

    /// Run inference on an RGB frame. Returns detections scaled to original image dimensions.
    pub fn detect(
        &mut self,
        rgb_data: &[u8],
        img_width: u32,
        img_height: u32,
    ) -> Result<Vec<Detection>> {
        let input = preprocess(rgb_data, img_width, img_height)?;

        let input_tensor = Tensor::from_array(input)
            .map_err(|e| anyhow::anyhow!("failed to create input tensor: {e}"))?;
        let outputs = self.session.run(ort::inputs![input_tensor])
            .map_err(|e| anyhow::anyhow!("inference failed: {e}"))?;

        // Extract output tensor — YOLOv8 output shape: [1, 84, 8400]
        let output_value = &outputs[0];
        let (shape, data) = output_value
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to extract output: {e}"))?;

        // Shape is [1, 84, 8400] — we want [84, 8400]
        let rows = shape[1] as usize;
        let cols = shape[2] as usize;

        // Skip the batch dimension
        let batch_offset = rows * cols;
        let slice = &data[..batch_offset];

        let output_2d = ArrayView2::from_shape((rows, cols), slice)
            .context("shape mismatch on output tensor")?;

        let mut detections = postprocess(output_2d, img_width, img_height, self.conf_threshold);
        if let Some(allow) = &self.class_allow {
            detections.retain(|d| allow.contains(&d.class.to_lowercase()));
        }
        Ok(detections)
    }
}

/// Preprocess RGB image to NCHW float32 tensor normalized to [0, 1].
fn preprocess(rgb_data: &[u8], width: u32, height: u32) -> Result<Array<f32, ndarray::Ix4>> {
    let img = image::RgbImage::from_raw(width, height, rgb_data.to_vec())
        .context("invalid RGB data dimensions")?;

    let resized = image::imageops::resize(
        &img,
        INPUT_SIZE,
        INPUT_SIZE,
        image::imageops::FilterType::Triangle,
    );

    let mut input = Array::zeros((1, 3, INPUT_SIZE as usize, INPUT_SIZE as usize));
    for y in 0..INPUT_SIZE as usize {
        for x in 0..INPUT_SIZE as usize {
            let pixel = resized.get_pixel(x as u32, y as u32);
            input[[0, 0, y, x]] = pixel[0] as f32 / 255.0;
            input[[0, 1, y, x]] = pixel[1] as f32 / 255.0;
            input[[0, 2, y, x]] = pixel[2] as f32 / 255.0;
        }
    }

    Ok(input)
}

/// Parse YOLOv8 output: shape [84, 8400] → detections.
/// Rows 0-3: cx, cy, w, h. Rows 4-83: class scores.
fn postprocess(
    output: ArrayView2<f32>,
    img_width: u32,
    img_height: u32,
    conf_threshold: f32,
) -> Vec<Detection> {
    // YOLOv8 output: [84, 8400] — transpose to [8400, 84]
    let output = output.t();
    let num_boxes = output.nrows();

    let scale_x = img_width as f32 / INPUT_SIZE as f32;
    let scale_y = img_height as f32 / INPUT_SIZE as f32;

    let mut candidates: Vec<Detection> = Vec::new();

    for i in 0..num_boxes {
        let cx = output[[i, 0]];
        let cy = output[[i, 1]];
        let w = output[[i, 2]];
        let h = output[[i, 3]];

        // Find best class
        let class_scores = output.slice(s![i, 4..]);
        let (class_id, &score) = class_scores
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();

        if score < conf_threshold {
            continue;
        }

        let left = ((cx - w / 2.0) * scale_x).max(0.0) as u32;
        let top = ((cy - h / 2.0) * scale_y).max(0.0) as u32;
        let right = ((cx + w / 2.0) * scale_x).min(img_width as f32) as u32;
        let bottom = ((cy + h / 2.0) * scale_y).min(img_height as f32) as u32;

        let class_name = if class_id < CLASS_NAMES.len() {
            CLASS_NAMES[class_id].to_string()
        } else {
            format!("class_{class_id}")
        };

        candidates.push(Detection {
            class: class_name,
            class_id: class_id as u32,
            score,
            left,
            top,
            right,
            bottom,
        });
    }

    // NMS
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    let mut kept: Vec<Detection> = Vec::new();
    for det in candidates {
        let dominated = kept.iter().any(|k| {
            k.class_id == det.class_id && iou(k, &det) > IOU_THRESHOLD
        });
        if !dominated {
            kept.push(det);
        }
    }

    kept
}

fn iou(a: &Detection, b: &Detection) -> f32 {
    let x1 = a.left.max(b.left) as f32;
    let y1 = a.top.max(b.top) as f32;
    let x2 = (a.right.min(b.right)) as f32;
    let y2 = (a.bottom.min(b.bottom)) as f32;

    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let area_a = (a.right - a.left) as f32 * (a.bottom - a.top) as f32;
    let area_b = (b.right - b.left) as f32 * (b.bottom - b.top) as f32;
    let union = area_a + area_b - inter;

    if union <= 0.0 { 0.0 } else { inter / union }
}
