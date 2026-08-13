use std::collections::HashMap;
use std::path::{Path as StdPath, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use uuid::Uuid;

use crate::error::AppError;
use crate::ingest::{ingest_bytes, status_for, FileResult, ImportSummary};
use crate::AppState;

/// In-memory job registry, shared across the app via `AppState`. Jobs are
/// intentionally not persisted: they exist only to let the web app poll
/// progress of a running downloader script, and are lost on server restart
/// like any other transient request state.
pub type JobStore = Arc<Mutex<HashMap<String, Job>>>;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub script: String,
    pub urls: Vec<String>,
    /// Index into `urls` of the one currently being processed; `None` once
    /// every url has been run (or before the first one has started).
    pub current_index: Option<usize>,
    pub status: JobStatus,
    pub log: Vec<String>,
    pub results: Vec<FileResult>,
    pub summary: ImportSummary,
    pub started_at: String,
    pub finished_at: Option<String>,
}

#[derive(Serialize)]
pub struct DownloaderInfo {
    pub name: String,
}

/// Lists executable files directly inside `downloaders_path`, sorted by name.
/// Returns an empty list (rather than an error) when no `downloaders_path` is
/// configured, since the feature is optional.
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<DownloaderInfo>>, AppError> {
    let Some(downloaders_path) = state.config.downloaders_path.clone() else {
        return Ok(Json(Vec::new()));
    };

    let names = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&downloaders_path)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if entry.metadata()?.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    })
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, anyhow::anyhow!(e)))??;

    Ok(Json(names.into_iter().map(|name| DownloaderInfo { name }).collect()))
}

/// Resolves a script `name` to an absolute path inside the configured
/// `downloaders_path`, rejecting anything that isn't a direct child of it
/// (no path separators, no `.`/`..`) so a script name can never be used to
/// escape the allowlisted directory.
fn resolve_script(state: &AppState, name: &str) -> Result<PathBuf, AppError> {
    let downloaders_path = state
        .config
        .downloaders_path
        .as_ref()
        .ok_or_else(|| AppError::bad_request("no downloaders_path configured"))?;

    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(AppError::bad_request("invalid script name"));
    }

    let candidate = downloaders_path.join(name);
    let canonical = candidate.canonicalize().map_err(|_| AppError::not_found("script not found"))?;

    if canonical.parent() != Some(downloaders_path.as_path()) || !canonical.is_file() {
        return Err(AppError::bad_request("invalid script name"));
    }

    Ok(canonical)
}

#[derive(Deserialize)]
pub struct RunBody {
    urls: Vec<String>,
}

#[derive(Serialize)]
pub struct RunResponse {
    job_id: String,
}

/// Kicks off a downloader script as a detached background job, once per url
/// in sequence (never in parallel — each run gets its own staging directory
/// and must finish before the next starts), and returns immediately with the
/// job's id; the web app polls `job_status` for progress. The script is
/// invoked via argv (never through a shell), with the url as its only
/// argument and a fresh staging directory it should write files into.
pub async fn run(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<RunBody>,
) -> Result<Json<RunResponse>, AppError> {
    let script = resolve_script(&state, &name)?;
    let urls: Vec<String> = body.urls.iter().map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).collect();
    if urls.is_empty() {
        return Err(AppError::bad_request("at least one url is required"));
    }

    let job_id = Uuid::new_v4().to_string();
    let staging_root = std::env::temp_dir().join(format!("phoserv-dl-{job_id}"));
    tokio::fs::create_dir_all(&staging_root).await?;

    let job = Job {
        id: job_id.clone(),
        script: name,
        urls: urls.clone(),
        current_index: Some(0),
        status: JobStatus::Running,
        log: Vec::new(),
        results: Vec::new(),
        summary: ImportSummary::default(),
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: None,
    };
    state.jobs.lock().unwrap().insert(job_id.clone(), job);

    let task_state = state.clone();
    let task_job_id = job_id.clone();
    tokio::spawn(run_job(task_state, task_job_id, script, urls, staging_root));

    Ok(Json(RunResponse { job_id }))
}

pub async fn job_status(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Job>, AppError> {
    let jobs = state.jobs.lock().unwrap();
    match jobs.get(&id) {
        Some(job) => Ok(Json(job.clone())),
        None => Err(AppError::not_found("job not found")),
    }
}

fn append_log(state: &AppState, job_id: &str, line: String) {
    if let Some(job) = state.jobs.lock().unwrap().get_mut(job_id) {
        job.log.push(line);
    }
}

fn record_result(state: &AppState, job_id: &str, result: FileResult) {
    if let Some(job) = state.jobs.lock().unwrap().get_mut(job_id) {
        job.summary.record(result.status);
        job.results.push(result);
    }
}

fn set_current_index(state: &AppState, job_id: &str, index: Option<usize>) {
    if let Some(job) = state.jobs.lock().unwrap().get_mut(job_id) {
        job.current_index = index;
    }
}

fn finish_job(state: &AppState, job_id: &str, status: JobStatus) {
    if let Some(job) = state.jobs.lock().unwrap().get_mut(job_id) {
        job.status = status;
        job.finished_at = Some(chrono::Utc::now().to_rfc3339());
    }
}

/// One line of the manifest a downloader script emits on stdout to report a
/// downloaded file: `{"file": "<path relative to the staging dir>", "tags": [...]}`.
/// Any stdout line that isn't valid JSON in this shape is treated as plain
/// log output rather than an error, so scripts are free to print progress.
#[derive(Deserialize)]
struct ManifestLine {
    file: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// Runs the downloader script once per url, strictly in sequence (each run
/// must finish before the next starts, since scripts write into a shared-name
/// staging subdirectory and log to the same job). The job is marked failed if
/// any url's run fails, but every url still gets a run regardless of earlier
/// failures. Removes the whole staging tree once every url has been
/// processed.
async fn run_job(state: AppState, job_id: String, script: PathBuf, urls: Vec<String>, staging_root: PathBuf) {
    let total = urls.len();
    let mut any_failed = false;

    for (index, url) in urls.iter().enumerate() {
        set_current_index(&state, &job_id, Some(index));
        append_log(&state, &job_id, format!("=== [{}/{total}] {url} ===", index + 1));

        let staging_dir = staging_root.join(index.to_string());
        if let Err(e) = tokio::fs::create_dir_all(&staging_dir).await {
            append_log(&state, &job_id, format!("failed to create staging dir: {e}"));
            any_failed = true;
            continue;
        }

        let ok = run_one(&state, &job_id, &script, url, &staging_dir).await;
        any_failed |= !ok;

        if let Err(e) = tokio::fs::remove_dir_all(&staging_dir).await {
            tracing::warn!("failed to clean up downloader staging dir {}: {e}", staging_dir.display());
        }
    }

    set_current_index(&state, &job_id, None);
    finish_job(&state, &job_id, if any_failed { JobStatus::Failed } else { JobStatus::Completed });
    if let Err(e) = tokio::fs::remove_dir_all(&staging_root).await {
        tracing::warn!("failed to clean up downloader staging dir {}: {e}", staging_root.display());
    }
}

/// Runs the downloader script for a single url to completion, streaming its
/// stdout (parsing manifest lines and ingesting the files they reference) and
/// stderr into the job log. Returns whether the script started and exited
/// successfully.
async fn run_one(state: &AppState, job_id: &str, script: &StdPath, url: &str, staging_dir: &StdPath) -> bool {
    let mut cmd = Command::new(script);
    cmd.arg(url)
        .env("PHOSERV_STAGING_DIR", staging_dir)
        .current_dir(staging_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            append_log(state, job_id, format!("failed to start script: {e}"));
            return false;
        }
    };

    let stdout = child.stdout.take().expect("child spawned with piped stdout");
    let stderr = child.stderr.take().expect("child spawned with piped stderr");

    let stdout_state = state.clone();
    let stdout_job_id = job_id.to_string();
    let stdout_staging = staging_dir.to_path_buf();
    let stdout_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            append_log(&stdout_state, &stdout_job_id, line.clone());
            if let Ok(entry) = serde_json::from_str::<ManifestLine>(&line) {
                process_manifest_entry(&stdout_state, &stdout_job_id, &stdout_staging, entry).await;
            }
        }
    });

    let stderr_state = state.clone();
    let stderr_job_id = job_id.to_string();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            append_log(&stderr_state, &stderr_job_id, format!("[stderr] {line}"));
        }
    });

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    match child.wait().await {
        Ok(status) if status.success() => true,
        Ok(status) => {
            append_log(state, job_id, format!("script exited with {status}"));
            false
        }
        Err(e) => {
            append_log(state, job_id, format!("failed to wait on script: {e}"));
            false
        }
    }
}

/// Ingests one file referenced by a manifest line through the same
/// `ingest_bytes` pipeline as every other import path, after verifying the
/// referenced path stays inside the staging directory the script was given.
async fn process_manifest_entry(state: &AppState, job_id: &str, staging_dir: &StdPath, entry: ManifestLine) {
    let requested_path = entry.file.clone();
    let file_path = staging_dir.join(&entry.file);

    let canonical = match file_path.canonicalize() {
        Ok(p) if p.starts_with(staging_dir) => p,
        _ => {
            record_result(
                state,
                job_id,
                FileResult {
                    path: requested_path,
                    status: "error",
                    tags: entry.tags,
                    photo_id: None,
                    error: Some("file path escapes staging directory".to_string()),
                },
            );
            return;
        }
    };

    let bytes = match tokio::fs::read(&canonical).await {
        Ok(bytes) => bytes,
        Err(e) => {
            record_result(
                state,
                job_id,
                FileResult { path: requested_path, status: "error", tags: entry.tags, photo_id: None, error: Some(e.to_string()) },
            );
            return;
        }
    };

    let filename = canonical.file_name().and_then(|s| s.to_str()).unwrap_or("download").to_string();

    let result = match ingest_bytes(state, filename, None, bytes, &entry.tags).await {
        Ok(outcome) => {
            let status = status_for(outcome.created, &outcome.tags_added);
            FileResult { path: requested_path, status, tags: entry.tags, photo_id: Some(outcome.photo.id), error: None }
        }
        Err(e) => FileResult { path: requested_path, status: "error", tags: entry.tags, photo_id: None, error: Some(e.1.to_string()) },
    };
    record_result(state, job_id, result);
}
