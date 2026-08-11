use std::path::Path;

use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Image,
    Video,
}

impl MediaType {
    pub fn from_mime(mime: &str) -> Option<Self> {
        if mime.starts_with("image/") {
            Some(MediaType::Image)
        } else if mime.starts_with("video/") {
            Some(MediaType::Video)
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            MediaType::Image => "image",
            MediaType::Video => "video",
        }
    }
}

#[derive(Debug, Default)]
pub struct ProbeResult {
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_seconds: Option<f64>,
    pub taken_at: Option<String>,
}

#[derive(Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    #[serde(default)]
    format: Option<FfprobeFormat>,
}

#[derive(Deserialize)]
struct FfprobeStream {
    width: Option<i64>,
    height: Option<i64>,
    #[serde(default)]
    tags: Option<std::collections::HashMap<String, String>>,
}

#[derive(Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
    #[serde(default)]
    tags: Option<std::collections::HashMap<String, String>>,
}

const TAKEN_AT_TAG_KEYS: &[&str] = &[
    "creation_time",
    "DateTimeOriginal",
    "date",
    "com.apple.quicktime.creationdate",
];

pub async fn probe(path: &Path) -> anyhow::Result<ProbeResult> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!("ffprobe failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout)?;

    let mut result = ProbeResult::default();

    for stream in &parsed.streams {
        if let (Some(w), Some(h)) = (stream.width, stream.height) {
            result.width = Some(w);
            result.height = Some(h);
        }
        if let Some(tags) = &stream.tags {
            result.taken_at = find_taken_at(tags);
        }
        if result.width.is_some() {
            break;
        }
    }

    if let Some(format) = &parsed.format {
        if let Some(duration) = &format.duration {
            result.duration_seconds = duration.parse::<f64>().ok();
        }
        if result.taken_at.is_none() {
            if let Some(tags) = &format.tags {
                result.taken_at = find_taken_at(tags);
            }
        }
    }

    Ok(result)
}

fn find_taken_at(tags: &std::collections::HashMap<String, String>) -> Option<String> {
    for key in TAKEN_AT_TAG_KEYS {
        if let Some(v) = tags.get(*key) {
            return Some(v.clone());
        }
        // ffprobe tag keys are sometimes lowercased
        if let Some(v) = tags.get(&key.to_lowercase()) {
            return Some(v.clone());
        }
    }
    None
}

/// Generates a single JPEG thumbnail whose largest dimension is at most
/// `max_dimension`, without upscaling smaller sources.
pub async fn generate_thumbnail(
    input_path: &Path,
    output_path: &Path,
    media_type: MediaType,
    max_dimension: u32,
) -> anyhow::Result<()> {
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let scale_filter = format!(
        "scale='min({max_dimension},iw)':'min({max_dimension},ih)':force_original_aspect_ratio=decrease"
    );

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y");
    if media_type == MediaType::Video {
        // seek in a bit so we don't grab a black first frame; ffmpeg clamps
        // automatically for very short videos
        cmd.args(["-ss", "1"]);
    }
    cmd.arg("-i").arg(input_path);
    cmd.args(["-frames:v", "1", "-vf", &scale_filter, "-q:v", "4"]);
    cmd.arg(output_path);

    let output = cmd.output().await?;
    if !output.status.success() {
        anyhow::bail!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}
