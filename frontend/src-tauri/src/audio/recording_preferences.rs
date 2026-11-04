use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

use anyhow::Result;
#[cfg(target_os = "macos")]
use log::error;

#[cfg(target_os = "macos")]
use crate::audio::capture::AudioCaptureBackend;

/// Default template used to name each meeting's folder inside the save folder.
/// Produces e.g. `Team Standup_2025-11-04_14-30`.
pub const DEFAULT_FOLDER_NAME_TEMPLATE: &str = "{meeting}_{date}_{time}";

/// Default base name (without extension) of the merged audio file inside a meeting folder.
/// Kept as `audio` so existing meetings, retranscription, and import keep working unchanged.
pub const DEFAULT_AUDIO_FILE_NAME: &str = "audio";

fn default_folder_name_template() -> String {
    DEFAULT_FOLDER_NAME_TEMPLATE.to_string()
}

fn default_audio_file_name() -> String {
    DEFAULT_AUDIO_FILE_NAME.to_string()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecordingPreferences {
    pub save_folder: PathBuf,
    pub auto_save: bool,
    pub file_format: String,
    #[serde(default)]
    pub preferred_mic_device: Option<String>,
    #[serde(default)]
    pub preferred_system_device: Option<String>,
    #[cfg(target_os = "macos")]
    #[serde(default)]
    pub system_audio_backend: Option<String>,
    /// Template for the per-meeting folder name. Supports `{meeting}`, `{date}`,
    /// `{time}`, `{datetime}`, `{year}`, `{month}`, `{day}`, `{hour}`, `{minute}`, `{second}`.
    #[serde(default = "default_folder_name_template")]
    pub folder_name_template: String,
    /// Base name (without extension) of the merged audio file. Same placeholders as the
    /// folder template are supported.
    #[serde(default = "default_audio_file_name")]
    pub audio_file_name: String,
}

impl Default for RecordingPreferences {
    fn default() -> Self {
        Self {
            save_folder: get_default_recordings_folder(),
            auto_save: true,
            file_format: "mp4".to_string(),
            preferred_mic_device: None,
            preferred_system_device: None,
            #[cfg(target_os = "macos")]
            system_audio_backend: Some("coreaudio".to_string()),
            folder_name_template: default_folder_name_template(),
            audio_file_name: default_audio_file_name(),
        }
    }
}

impl RecordingPreferences {
    /// Folder/file naming options to hand to the recording saver.
    pub fn save_options(&self) -> RecordingSaveOptions {
        RecordingSaveOptions {
            base_folder: self.save_folder.clone(),
            folder_name_template: self.folder_name_template.clone(),
            audio_file_name: self.audio_file_name.clone(),
        }
    }
}

/// Where and under which names a recording session is written.
#[derive(Debug, Clone)]
pub struct RecordingSaveOptions {
    pub base_folder: PathBuf,
    pub folder_name_template: String,
    pub audio_file_name: String,
}

impl Default for RecordingSaveOptions {
    fn default() -> Self {
        Self {
            base_folder: get_default_recordings_folder(),
            folder_name_template: default_folder_name_template(),
            audio_file_name: default_audio_file_name(),
        }
    }
}

impl RecordingSaveOptions {
    /// Render the meeting folder name for `meeting_name` at `now` (local time).
    pub fn render_folder_name(&self, meeting_name: &str, now: chrono::DateTime<chrono::Local>) -> String {
        render_name_template(&self.folder_name_template, meeting_name, now, DEFAULT_FOLDER_NAME_TEMPLATE)
    }

    /// Render the audio file name (with `.mp4` extension) for `meeting_name` at `now`.
    pub fn render_audio_file_name(&self, meeting_name: &str, now: chrono::DateTime<chrono::Local>) -> String {
        let stem = render_name_template(&self.audio_file_name, meeting_name, now, DEFAULT_AUDIO_FILE_NAME);
        format!("{}.mp4", stem)
    }
}

/// Expand `{placeholder}` tokens in a naming template and sanitize the result for the
/// filesystem. Falls back to `fallback_template` when the template renders to nothing.
pub fn render_name_template(
    template: &str,
    meeting_name: &str,
    now: chrono::DateTime<chrono::Local>,
    fallback_template: &str,
) -> String {
    use super::audio_processing::sanitize_filename;

    let expand = |template: &str| -> String {
        let meeting = sanitize_filename(meeting_name);
        let meeting = if meeting.is_empty() { "Meeting".to_string() } else { meeting };
        template
            .replace("{meeting}", &meeting)
            .replace("{datetime}", &now.format("%Y-%m-%d_%H-%M-%S").to_string())
            .replace("{date}", &now.format("%Y-%m-%d").to_string())
            .replace("{time}", &now.format("%H-%M").to_string())
            .replace("{year}", &now.format("%Y").to_string())
            .replace("{month}", &now.format("%m").to_string())
            .replace("{day}", &now.format("%d").to_string())
            .replace("{hour}", &now.format("%H").to_string())
            .replace("{minute}", &now.format("%M").to_string())
            .replace("{second}", &now.format("%S").to_string())
    };

    // Sanitize again after expansion: a template may itself contain path separators,
    // and `.`/`..` must never become a folder name.
    let clean = |rendered: String| -> String {
        let sanitized = sanitize_filename(&rendered);
        let trimmed = sanitized.trim_matches(|c: char| c == '.' || c.is_whitespace());
        trimmed.to_string()
    };

    let rendered = clean(expand(template.trim()));
    if !rendered.is_empty() {
        return rendered;
    }

    let fallback = clean(expand(fallback_template));
    if fallback.is_empty() {
        clean(expand(DEFAULT_FOLDER_NAME_TEMPLATE))
    } else {
        fallback
    }
}

/// Get the default recordings folder based on platform
pub fn get_default_recordings_folder() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        // Windows: %USERPROFILE%\Music\meetily-recordings
        if let Some(music_dir) = dirs::audio_dir() {
            music_dir.join("meetily-recordings")
        } else {
            // Fallback to Documents if Music folder is not available
            dirs::document_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("meetily-recordings")
        }
    }

    #[cfg(target_os = "macos")]
    {
        // macOS: ~/Movies/meetily-recordings
        if let Some(movies_dir) = dirs::video_dir() {
            movies_dir.join("meetily-recordings")
        } else {
            // Fallback to Documents if Movies folder is not available
            dirs::document_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("meetily-recordings")
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // Linux/Others: ~/Documents/meetily-recordings
        dirs::document_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("meetily-recordings")
    }
}

/// Ensure the recordings directory exists
pub fn ensure_recordings_directory(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
        info!("Created recordings directory: {:?}", path);
    }
    Ok(())
}

/// Generate a unique filename for a recording
pub fn generate_recording_filename(format: &str) -> String {
    let now = chrono::Utc::now();
    let timestamp = now.format("%Y%m%d_%H%M%S");
    format!("recording_{}.{}", timestamp, format)
}

/// Load recording preferences from store
pub async fn load_recording_preferences<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<RecordingPreferences> {
    // Try to load from Tauri store
    let store = match app.store("recording_preferences.json") {
        Ok(store) => store,
        Err(e) => {
            warn!("Failed to access store: {}, using defaults", e);
            return Ok(RecordingPreferences::default());
        }
    };

    // Try to get the preferences from store
    let prefs = if let Some(value) = store.get("preferences") {
        match serde_json::from_value::<RecordingPreferences>(value.clone()) {
            Ok(mut p) => {
                info!("Loaded recording preferences from store");
                // Update macOS backend to current value if needed
                #[cfg(target_os = "macos")]
                {
                    let backend = crate::audio::capture::get_current_backend();
                    p.system_audio_backend = Some(backend.to_string());
                }
                p
            }
            Err(e) => {
                warn!("Failed to deserialize preferences: {}, using defaults", e);
                RecordingPreferences::default()
            }
        }
    } else {
        info!("No stored preferences found, using defaults");
        RecordingPreferences::default()
    };

    info!("Loaded recording preferences: save_folder={:?}, auto_save={}, format={}, mic={:?}, system={:?}, folder_template={:?}, audio_file_name={:?}",
          prefs.save_folder, prefs.auto_save, prefs.file_format,
          prefs.preferred_mic_device, prefs.preferred_system_device,
          prefs.folder_name_template, prefs.audio_file_name);
    Ok(prefs)
}

/// Load the save folder and naming options, falling back to defaults when the
/// preferences cannot be read. Used by the recording start paths.
pub async fn load_recording_save_options<R: Runtime>(app: &AppHandle<R>) -> RecordingSaveOptions {
    match load_recording_preferences(app).await {
        Ok(prefs) => prefs.save_options(),
        Err(e) => {
            warn!("Failed to load recording preferences for save options, using defaults: {}", e);
            RecordingSaveOptions::default()
        }
    }
}

/// Save recording preferences to store
pub async fn save_recording_preferences<R: Runtime>(
    app: &AppHandle<R>,
    preferences: &RecordingPreferences,
) -> Result<()> {
    info!("Saving recording preferences: save_folder={:?}, auto_save={}, format={}, mic={:?}, system={:?}, folder_template={:?}, audio_file_name={:?}",
          preferences.save_folder, preferences.auto_save, preferences.file_format,
          preferences.preferred_mic_device, preferences.preferred_system_device,
          preferences.folder_name_template, preferences.audio_file_name);

    // Validate the save folder before persisting anything: an empty or relative path
    // would silently scatter recordings relative to the process working directory.
    let folder_str = preferences.save_folder.to_string_lossy();
    if folder_str.trim().is_empty() {
        return Err(anyhow::anyhow!("Save folder cannot be empty"));
    }
    if !preferences.save_folder.is_absolute() {
        return Err(anyhow::anyhow!(
            "Save folder must be an absolute path (got {:?})",
            preferences.save_folder
        ));
    }

    // Ensure the directory exists and is writable before we commit the preference.
    ensure_recordings_directory(&preferences.save_folder)?;
    verify_folder_writable(&preferences.save_folder)?;

    // Get or create store
    let store = app
        .store("recording_preferences.json")
        .map_err(|e| anyhow::anyhow!("Failed to access store: {}", e))?;

    // Serialize preferences to JSON value
    let prefs_value = serde_json::to_value(preferences)
        .map_err(|e| anyhow::anyhow!("Failed to serialize preferences: {}", e))?;

    // Save to store
    store.set("preferences", prefs_value);

    // Persist to disk
    store
        .save()
        .map_err(|e| anyhow::anyhow!("Failed to save store to disk: {}", e))?;

    info!("Successfully persisted recording preferences to disk");

    // Save backend preference to global config
    #[cfg(target_os = "macos")]
    if let Some(backend_str) = &preferences.system_audio_backend {
        if let Some(backend) = AudioCaptureBackend::from_string(backend_str) {
            info!("Setting audio capture backend to: {:?}", backend);
            crate::audio::capture::set_current_backend(backend);
        }
    }

    Ok(())
}

/// Create and remove a probe file to confirm recordings can actually be written here.
fn verify_folder_writable(path: &PathBuf) -> Result<()> {
    let probe = path.join(format!(".meetily-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"meetily")
        .map_err(|e| anyhow::anyhow!("Save folder {:?} is not writable: {}", path, e))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Tauri commands for recording preferences
#[tauri::command]
pub async fn get_recording_preferences<R: Runtime>(
    app: AppHandle<R>,
) -> Result<RecordingPreferences, String> {
    load_recording_preferences(&app)
        .await
        .map_err(|e| format!("Failed to load recording preferences: {}", e))
}

#[tauri::command]
pub async fn set_recording_preferences<R: Runtime>(
    app: AppHandle<R>,
    preferences: RecordingPreferences,
) -> Result<(), String> {
    save_recording_preferences(&app, &preferences)
        .await
        .map_err(|e| format!("Failed to save recording preferences: {}", e))
}

#[tauri::command]
pub async fn get_default_recordings_folder_path() -> Result<String, String> {
    let path = get_default_recordings_folder();
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
pub async fn open_recordings_folder<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let preferences = load_recording_preferences(&app)
        .await
        .map_err(|e| format!("Failed to load preferences: {}", e))?;

    // Ensure directory exists before trying to open it
    ensure_recordings_directory(&preferences.save_folder)
        .map_err(|e| format!("Failed to create directory: {}", e))?;

    let folder_path = preferences.save_folder.to_string_lossy().to_string();

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(&folder_path)
            .spawn()
            .map_err(|e| format!("Failed to open folder: {}", e))?;
    }

    info!("Opened recordings folder: {}", folder_path);
    Ok(())
}

/// Open a native folder picker and return the chosen folder (or `None` if cancelled).
/// The choice is NOT persisted here; the frontend saves it through `set_recording_preferences`
/// so validation (absolute path, writable) runs in one place.
#[tauri::command]
pub async fn select_recording_folder<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    info!("Opening folder picker for recordings save location");

    let current = load_recording_preferences(&app)
        .await
        .map(|p| p.save_folder)
        .unwrap_or_else(|_| get_default_recordings_folder());

    // The blocking picker must not run on the async runtime thread.
    let app_clone = app.clone();
    let picked = tokio::task::spawn_blocking(move || {
        let mut builder = app_clone.dialog().file().set_title("Choose where to save recordings");
        if current.exists() {
            builder = builder.set_directory(&current);
        }
        builder.blocking_pick_folder()
    })
    .await
    .map_err(|e| format!("Folder dialog task failed: {}", e))?;

    match picked {
        Some(path) => {
            let path_str = path.to_string();
            info!("User selected recordings folder: {}", path_str);
            Ok(Some(path_str))
        }
        None => {
            info!("Folder selection cancelled");
            Ok(None)
        }
    }
}

/// Render a naming template with a sample meeting name and the current local time so the
/// settings UI can show what a folder/file will be called.
#[tauri::command]
pub async fn preview_recording_name(
    template: String,
    meeting_name: Option<String>,
) -> Result<String, String> {
    let sample = meeting_name.unwrap_or_else(|| "Team Standup".to_string());
    Ok(render_name_template(
        &template,
        &sample,
        chrono::Local::now(),
        DEFAULT_FOLDER_NAME_TEMPLATE,
    ))
}

// Backend selection commands

/// Get available audio capture backends for the current platform
#[tauri::command]
pub async fn get_available_audio_backends() -> Result<Vec<String>, String> {
    #[cfg(target_os = "macos")]
    {
        let backends = crate::audio::capture::get_available_backends();
        Ok(backends.iter().map(|b| b.to_string()).collect())
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Only ScreenCaptureKit available on non-macOS
        Ok(vec!["screencapturekit".to_string()])
    }
}

/// Get current audio capture backend
#[tauri::command]
pub async fn get_current_audio_backend() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        let backend = crate::audio::capture::get_current_backend();
        Ok(backend.to_string())
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok("screencapturekit".to_string())
    }
}

/// Set audio capture backend
#[tauri::command]
pub async fn set_audio_backend(backend: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use crate::audio::capture::AudioCaptureBackend;
        use crate::audio::permissions::{
            check_screen_recording_permission, request_screen_recording_permission,
        };

        let backend_enum = AudioCaptureBackend::from_string(&backend)
            .ok_or_else(|| format!("Invalid backend: {}", backend))?;

        // If switching to Core Audio, log information about Audio Capture permission
        if backend_enum == AudioCaptureBackend::CoreAudio {
            info!("🔐 Core Audio backend requires Audio Capture permission (macOS 14.4+)");
            info!("📍 Permission dialog will appear automatically when recording starts");

            // Check if permission is already granted (this is informational only)
            if !check_screen_recording_permission() {
                warn!("⚠️  Audio Capture permission may not be granted");

                // Attempt to open System Settings (opens System Settings)
                if let Err(e) = request_screen_recording_permission() {
                    error!("Failed to open System Settings: {}", e);
                }

                return Err(
                    "Core Audio requires Audio Capture permission. \
                    The permission dialog will appear when you start recording. \
                    If already denied, enable it in System Settings → Privacy & Security → Audio Capture, \
                    then restart the app.".to_string()
                );
            }

            info!(
                "✅ Core Audio backend selected - permission check will occur at recording start"
            );
        }

        info!("Setting audio backend to: {:?}", backend_enum);
        crate::audio::capture::set_current_backend(backend_enum);
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        if backend != "screencapturekit" {
            return Err(format!(
                "Backend {} not available on this platform",
                backend
            ));
        }
        Ok(())
    }
}

/// Get backend information (name and description)
#[derive(Serialize)]
pub struct BackendInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[tauri::command]
pub async fn get_audio_backend_info() -> Result<Vec<BackendInfo>, String> {
    #[cfg(target_os = "macos")]
    {
        use crate::audio::capture::AudioCaptureBackend;

        let backends = vec![
            BackendInfo {
                id: AudioCaptureBackend::ScreenCaptureKit.to_string(),
                name: AudioCaptureBackend::ScreenCaptureKit.name().to_string(),
                description: AudioCaptureBackend::ScreenCaptureKit
                    .description()
                    .to_string(),
            },
            BackendInfo {
                id: AudioCaptureBackend::CoreAudio.to_string(),
                name: AudioCaptureBackend::CoreAudio.name().to_string(),
                description: AudioCaptureBackend::CoreAudio.description().to_string(),
            },
        ];
        Ok(backends)
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(vec![BackendInfo {
            id: "screencapturekit".to_string(),
            name: "ScreenCaptureKit".to_string(),
            description: "Default system audio capture".to_string(),
        }])
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn sample_time() -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(2025, 11, 4, 14, 30, 5).unwrap()
    }

    #[test]
    fn default_template_matches_legacy_layout() {
        let options = RecordingSaveOptions::default();
        assert_eq!(
            options.render_folder_name("Team Standup", sample_time()),
            "Team Standup_2025-11-04_14-30"
        );
        assert_eq!(options.render_audio_file_name("Team Standup", sample_time()), "audio.mp4");
    }

    #[test]
    fn all_placeholders_expand() {
        let rendered = render_name_template(
            "{year}-{month}-{day} {hour}{minute}{second} {datetime} {meeting}",
            "Sync",
            sample_time(),
            DEFAULT_FOLDER_NAME_TEMPLATE,
        );
        assert_eq!(rendered, "2025-11-04 143005 2025-11-04_14-30-05 Sync");
    }

    #[test]
    fn unsafe_characters_are_sanitized() {
        let rendered = render_name_template(
            "{meeting}/{date}",
            "Q4: plan <draft>?",
            sample_time(),
            DEFAULT_FOLDER_NAME_TEMPLATE,
        );
        assert_eq!(rendered, "Q4_ plan _draft___2025-11-04");
    }

    #[test]
    fn empty_or_dot_only_template_falls_back() {
        let rendered = render_name_template("   ", "Sync", sample_time(), DEFAULT_FOLDER_NAME_TEMPLATE);
        assert_eq!(rendered, "Sync_2025-11-04_14-30");

        let rendered = render_name_template("..", "Sync", sample_time(), DEFAULT_FOLDER_NAME_TEMPLATE);
        assert_eq!(rendered, "Sync_2025-11-04_14-30");

        // Audio stem fallback is "audio" even when the template is blank.
        let options = RecordingSaveOptions {
            audio_file_name: String::new(),
            ..RecordingSaveOptions::default()
        };
        assert_eq!(options.render_audio_file_name("Sync", sample_time()), "audio.mp4");
    }

    #[test]
    fn empty_meeting_name_uses_placeholder() {
        let rendered = render_name_template("{meeting}", "   ", sample_time(), DEFAULT_FOLDER_NAME_TEMPLATE);
        assert_eq!(rendered, "Meeting");
    }

    #[test]
    fn legacy_preferences_without_new_fields_deserialize_with_defaults() {
        let legacy = serde_json::json!({
            "save_folder": "/tmp/meetily-recordings",
            "auto_save": true,
            "file_format": "mp4"
        });
        let prefs: RecordingPreferences = serde_json::from_value(legacy).unwrap();
        assert_eq!(prefs.folder_name_template, DEFAULT_FOLDER_NAME_TEMPLATE);
        assert_eq!(prefs.audio_file_name, DEFAULT_AUDIO_FILE_NAME);
    }
}
