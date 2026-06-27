use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;
use tauri::State;

/// Desktop-only imports: file-system event watching (notify) + window focus
/// (AppHandle/Emitter/OpenerExt). These modules don't exist on Android, so
/// they share the same `#[cfg(not(target_os = "android"))]` gate as the
/// functions that use them.
#[cfg(not(target_os = "android"))]
use notify::{RecursiveMode, Watcher};
#[cfg(not(target_os = "android"))]
use tauri::{AppHandle, Emitter};
#[cfg(not(target_os = "android"))]
use tauri_plugin_opener::OpenerExt;

use crate::error::{locked, AppError, AppResult};
use crate::models::{Credential, CredentialType};
use crate::ssh::sftp::{FileStat, RemoteEntry, SftpHandle, WalkEntry};
use crate::state::AppState;

/// Maximum recursion depth for the local walker. Mirrors the remote-side cap.
const LOCAL_WALK_DEPTH_CAP: u32 = 32;

/// RAII：注册 cancel flag 并在 drop 时自动 unregister，无论 streaming 正常返回、
/// 早 `?`、还是 panic。替代旧的手写 register/unregister 配对。
pub struct CancelGuard<'a> {
    state: &'a AppState,
    transfer_id: String,
}

impl<'a> CancelGuard<'a> {
    /// 注册 flag。返回 (guard, flag)：guard 控生命周期，flag 喂给 streaming 函数。
    /// `pub` 让 headless server 复用同一套 RAII 清理（drop 时 unregister，覆盖
    /// 正常返回 / 早 `?` / panic 三种路径），避免手写 register/remove 漏删。
    pub fn register(
        state: &'a AppState,
        transfer_id: String,
    ) -> AppResult<(Self, Arc<AtomicBool>)> {
        let flag = Arc::new(AtomicBool::new(false));
        locked(&state.transfer_cancels)?.insert(transfer_id.clone(), flag.clone());
        Ok((Self { state, transfer_id }, flag))
    }
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut m) = locked(&self.state.transfer_cancels) {
            m.remove(&self.transfer_id);
        }
    }
}

#[tauri::command]
pub async fn sftp_connect(
    state: State<'_, AppState>,
    host: String,
    port: u16,
    username: String,
    auth_type: String,
    secret: Option<String>,
) -> AppResult<String> {
    let cred = Credential {
        id: String::new(),
        name: String::new(),
        username,
        credential_type: CredentialType::from_str(&auth_type),
        secret,
        save_to_remote: false,
    };

    let timeout_secs: u64 = crate::db::settings::get(&state.db, "connect_timeout")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(crate::ssh::client::DEFAULT_CONNECT_TIMEOUT);

    let known_hosts_path = crate::ssh::known_hosts::path_for(&state.data_dir);
    let handle = crate::ssh::client::run_blocking_ssh(move || async move {
        SftpHandle::connect(host, port, cred, known_hosts_path, timeout_secs).await
    })
    .await?;
    let id = uuid::Uuid::new_v4().to_string();

    locked(&state.sftp_sessions)?.insert(id.clone(), Arc::new(handle));

    Ok(id)
}

/// Connect SFTP by reusing an active SSH session (no re-authentication).
#[tauri::command]
pub async fn sftp_connect_session(
    state: State<'_, AppState>,
    session_id: String,
) -> AppResult<String> {
    let ssh_handle = {
        let sessions = locked(&state.sessions)?;
        sessions
            .get(&session_id)
            .ok_or_else(|| AppError::not_found("ssh_session_not_found_msg", json!({})))?
            .ssh_handle()
            .clone()
    };

    let parent = session_id.clone();
    let handle = crate::ssh::client::run_blocking_ssh(move || async move {
        SftpHandle::from_handle(&ssh_handle, parent).await
    })
    .await?;
    let id = uuid::Uuid::new_v4().to_string();

    locked(&state.sftp_sessions)?.insert(id.clone(), Arc::new(handle));

    Ok(id)
}

/// 从 Mutex 中 clone 出 Arc<SftpHandle>，释放锁后再 await。
fn get_sftp(state: &State<'_, AppState>, sftp_id: &str) -> AppResult<Arc<SftpHandle>> {
    locked(&state.sftp_sessions)?
        .get(sftp_id)
        .cloned()
        .ok_or_else(|| AppError::not_found("sftp_session_not_found", json!({})))
}

#[tauri::command]
pub async fn sftp_home(state: State<'_, AppState>, sftp_id: String) -> AppResult<String> {
    let h = get_sftp(&state, &sftp_id)?;
    h.home_dir().await
}

#[tauri::command]
pub async fn sftp_list(
    state: State<'_, AppState>,
    sftp_id: String,
    path: String,
) -> AppResult<Vec<RemoteEntry>> {
    let h = get_sftp(&state, &sftp_id)?;
    h.list_dir(&path).await
}

/// Recursively list every file under a remote directory (symlink-to-file is
/// followed, symlink-to-dir is skipped to prevent cycles). The frontend queues
/// each returned entry as an independent Transfer; the directory abstraction
/// exists only inside this command.
#[tauri::command]
pub async fn sftp_walk_remote_dir(
    state: State<'_, AppState>,
    sftp_id: String,
    remote_root: String,
) -> AppResult<Vec<WalkEntry>> {
    let h = get_sftp(&state, &sftp_id)?;
    h.walk_files(&remote_root).await
}

/// Recursively list every file under a local directory; the local-side
/// counterpart of `sftp_walk_remote_dir`. `rel_path` always uses '/'; the
/// frontend swaps the separator when rebuilding the local physical path.
#[tauri::command]
pub async fn walk_local_dir(local_root: String) -> AppResult<Vec<WalkEntry>> {
    let root = PathBuf::from(&local_root);
    let mut queue: VecDeque<(PathBuf, u32)> = VecDeque::new();
    queue.push_back((root.clone(), 0));
    let mut result: Vec<WalkEntry> = Vec::new();

    while let Some((dir, depth)) = queue.pop_front() {
        if depth >= LOCAL_WALK_DEPTH_CAP {
            return Err(AppError::other(
                "local_tree_too_deep",
                json!({
                    "path": dir.display().to_string(),
                    "depth": depth,
                    "limit": LOCAL_WALK_DEPTH_CAP,
                }),
            ));
        }
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            // `entry.metadata()` does not traverse symlinks — single syscall
            // covers both type discrimination and size for regular files,
            // replacing the previous file_type() + metadata() double-stat.
            let path = entry.path();
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                queue.push_back((path, depth + 1));
            } else if meta.is_file() {
                result.push(WalkEntry {
                    rel_path: rel_unix(&path, &root),
                    size: meta.len(),
                });
            } else if meta.is_symlink() {
                // Follow once to learn what the target is. Skip symlink-to-dir
                // to avoid cycles, and silently skip broken symlinks.
                if let Ok(target_meta) = tokio::fs::metadata(&path).await {
                    if target_meta.is_file() {
                        result.push(WalkEntry {
                            rel_path: rel_unix(&path, &root),
                            size: target_meta.len(),
                        });
                    }
                }
            }
            // Anything else (block/char/fifo): skip.
        }
    }
    Ok(result)
}

/// Convert the portion of `full` relative to `root` into a '/'-separated string.
/// On Windows std::path::Component uses '\'; we normalise here and the frontend
/// converts back to the platform separator when joining.
fn rel_unix(full: &Path, root: &Path) -> String {
    let stripped = full.strip_prefix(root).unwrap_or(full);
    stripped
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[tauri::command]
pub async fn sftp_download(
    state: State<'_, AppState>,
    sftp_id: String,
    path: String,
) -> AppResult<Vec<u8>> {
    let h = get_sftp(&state, &sftp_id)?;
    h.download(&path).await
}

#[tauri::command]
pub async fn sftp_upload(
    state: State<'_, AppState>,
    sftp_id: String,
    path: String,
    data: Vec<u8>,
) -> AppResult<()> {
    let h = get_sftp(&state, &sftp_id)?;
    h.upload(&path, &data).await
}

#[tauri::command]
pub async fn sftp_mkdir(
    state: State<'_, AppState>,
    sftp_id: String,
    path: String,
) -> AppResult<()> {
    let h = get_sftp(&state, &sftp_id)?;
    h.mkdir(&path).await
}

#[tauri::command]
pub async fn sftp_close(state: State<'_, AppState>, sftp_id: String) -> AppResult<()> {
    locked(&state.sftp_sessions)?.remove(&sftp_id);
    Ok(())
}

/// Download a remote file via native Save As dialog with streaming + progress.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_save_file(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    sftp_id: String,
    remote_path: String,
    default_name: String,
) -> AppResult<Option<String>> {
    let save_path = rfd::AsyncFileDialog::new()
        .set_file_name(&default_name)
        .save_file()
        .await;

    let Some(handle) = save_path else {
        return Ok(None);
    };
    let local = handle.path().to_path_buf();

    let sftp = get_sftp(&state, &sftp_id)?;
    let transfer_id = uuid::Uuid::new_v4().to_string();
    let (_guard, cancel) = CancelGuard::register(&state, transfer_id.clone())?;
    let host = crate::emitter::Host::Tauri(app);
    sftp.download_streaming(&remote_path, &local, &host, &transfer_id, cancel)
        .await?;
    Ok(Some(local.display().to_string()))
}

/// Pick a local file via native Open dialog and upload with streaming + progress.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_pick_and_upload(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    sftp_id: String,
    remote_dir: String,
) -> AppResult<Option<String>> {
    let pick = rfd::AsyncFileDialog::new().pick_file().await;
    let Some(handle) = pick else { return Ok(None) };
    let local = handle.path().to_path_buf();

    let name = local
        .file_name()
        .ok_or_else(|| AppError::other("sftp_invalid_filename", json!({})))?
        .to_string_lossy()
        .into_owned();
    let remote_path = if remote_dir == "/" {
        format!("/{}", name)
    } else {
        format!("{}/{}", remote_dir.trim_end_matches('/'), name)
    };

    let sftp = get_sftp(&state, &sftp_id)?;
    let transfer_id = uuid::Uuid::new_v4().to_string();
    let (_guard, cancel) = CancelGuard::register(&state, transfer_id.clone())?;
    let host = crate::emitter::Host::Tauri(app);
    sftp.upload_streaming(&local, &remote_path, &host, &transfer_id, cancel)
        .await?;
    Ok(Some(name))
}

/// Open native Save-As dialog and return the chosen path. No transfer happens here.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_pick_save_path(default_name: String) -> AppResult<Option<String>> {
    let handle = rfd::AsyncFileDialog::new()
        .set_file_name(&default_name)
        .save_file()
        .await;
    Ok(handle.map(|h| h.path().display().to_string()))
}

/// Pick a folder via the native dialog. Used both as the destination root
/// (multi-select download) and the source root (recursive upload) — both
/// flows want the same rfd `pick_folder()` call, so a single command suffices.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_pick_folder() -> AppResult<Option<String>> {
    let handle = rfd::AsyncFileDialog::new().pick_folder().await;
    Ok(handle.map(|h| h.path().display().to_string()))
}

/// Pick multiple source files for upload. rfd's `pick_files` supports
/// multi-selection on every platform we ship to.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_pick_open_files() -> AppResult<Option<Vec<String>>> {
    let handles = rfd::AsyncFileDialog::new().pick_files().await;
    Ok(handles.map(|hs| {
        hs.into_iter()
            .map(|h| h.path().display().to_string())
            .collect()
    }))
}

/// Stream-download to a caller-supplied local path. transfer_id is used as the
/// `sftp:progress:{transfer_id}` event suffix (R1) so the frontend listens
/// per-transfer instead of multiplexing one global stream.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_download_to(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    sftp_id: String,
    remote_path: String,
    local_path: String,
    transfer_id: String,
) -> AppResult<()> {
    let sftp = get_sftp(&state, &sftp_id)?;
    let local = std::path::PathBuf::from(&local_path);
    let (_guard, cancel) = CancelGuard::register(&state, transfer_id.clone())?;
    let host = crate::emitter::Host::Tauri(app);
    sftp.download_streaming(&remote_path, &local, &host, &transfer_id, cancel)
        .await
        .map(|_| ())
}

/// Stream-upload from a caller-supplied local path. transfer_id mirrors above.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_upload_from(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    sftp_id: String,
    local_path: String,
    remote_path: String,
    transfer_id: String,
) -> AppResult<()> {
    let sftp = get_sftp(&state, &sftp_id)?;
    let local = std::path::PathBuf::from(&local_path);
    let (_guard, cancel) = CancelGuard::register(&state, transfer_id.clone())?;
    let host = crate::emitter::Host::Tauri(app);
    sftp.upload_streaming(&local, &remote_path, &host, &transfer_id, cancel)
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn sftp_remove(
    state: State<'_, AppState>,
    sftp_id: String,
    path: String,
) -> AppResult<()> {
    let h = get_sftp(&state, &sftp_id)?;
    h.remove(&path).await
}

#[tauri::command]
pub async fn sftp_rename(
    state: State<'_, AppState>,
    sftp_id: String,
    old_path: String,
    new_path: String,
) -> AppResult<()> {
    let h = get_sftp(&state, &sftp_id)?;
    h.rename(&old_path, &new_path).await
}

#[tauri::command]
pub async fn sftp_stat(
    state: State<'_, AppState>,
    sftp_id: String,
    path: String,
) -> AppResult<FileStat> {
    let h = get_sftp(&state, &sftp_id)?;
    h.stat(&path).await
}

/// 用户在传输页点"取消"调用：把 transfer_id 对应的 cancel flag 置 1，
/// streaming 循环下一次 chunk 检查时退出。
#[tauri::command]
pub fn sftp_cancel_transfer(state: State<'_, AppState>, transfer_id: String) -> AppResult<()> {
    use std::sync::atomic::Ordering;
    if let Some(flag) = locked(&state.transfer_cancels)?.get(&transfer_id) {
        flag.store(true, Ordering::SeqCst);
    }
    Ok(())
}

// ─── Open remote files with a local program (edit mode) ────────────────────
//
// Flow (reuses the transfers pipeline so downloads/uploads show in the
// transfer list and are cancellable):
//   1. sftp_prepare_edit    — allocate edit_id + temp dir, register EditSession
//                            (no download yet)
//   2. frontend             — transfers.startDownload streams the file into the
//                            temp dir (visible in the transfer list, cancellable)
//   3. sftp_start_edit_watch — called once the download finishes: opens the file
//                            via the system default program and starts a notify
//                            watcher
//   4. external editor saves the file → watcher emits `sftp:file_changed:{edit_id}`
//   5. frontend shows a modal → transfers.startUpload streams the file back
//      (visible in the transfer list, cancellable). The watcher keeps running so
//      the user can save again; the session is only cancelled when the SFTP panel
//      closes, the SSH session drops, or the temp file is deleted.
//   SSH disconnect / SFTP panel close → sftp_cancel_edits_for_session bulk cleanup.
//
// EditSession is created in step 1; the watcher is started in step 3 and clones
// the cancel flag. sftp_cancel_edit/sftp_cancel_edits_for_session sets the flag
// and the watcher exits on its next loop iteration.

/// An active "open with local program" edit session. The watcher holds a clone
/// of the cancel flag; setting it stops the watcher on its next iteration.
#[cfg(not(target_os = "android"))]
pub struct EditSession {
    /// Local temp path of the file ({temp}/rssh-edit/{edit_id}/{filename}).
    pub local_path: PathBuf,
    /// Parent SSH session id; sftp_cancel_edits_for_session matches on this for
    /// bulk cleanup.
    pub session_id: String,
    /// Flag to stop the watcher.
    pub cancel: Arc<AtomicBool>,
}

#[cfg(not(target_os = "android"))]
impl EditSession {
    fn new(local_path: PathBuf, session_id: String) -> Self {
        Self {
            local_path,
            session_id,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Throttle window-attention requests so a rapid burst of saves doesn't flash /
/// bounce the taskbar icon repeatedly. 2 s is enough to coalesce typical
/// auto-save / multi-step save bursts while still feeling immediate.
#[cfg(not(target_os = "android"))]
static LAST_ATTENTION: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// Bring all app windows to the foreground + flash the taskbar icon (Windows) /
/// Dock bounce (macOS) / urgency hint (Linux). `request_user_attention(Critical)`
/// is the OS-sanctioned "request attention" cue and bypasses focus-stealing
/// protection for background processes.
#[cfg(not(target_os = "android"))]
fn bring_window_to_front(app: &AppHandle) {
    use tauri::Manager;

    const ATTENTION_COOLDOWN: Duration = Duration::from_secs(2);
    let now = Instant::now();
    {
        let mut last = LAST_ATTENTION.lock().unwrap();
        if let Some(t) = *last {
            if now.duration_since(t) < ATTENTION_COOLDOWN {
                return;
            }
        }
        *last = Some(now);
    }

    for (_, win) in app.webview_windows() {
        let _ = win.unminimize();
        let _ = win.set_focus();
        let _ = win.request_user_attention(Some(tauri::UserAttentionType::Critical));
    }
}

/// Watch the temp file for changes. Uses a notify watcher
/// (inotify/FSEvents/ReadDirectoryChangesW) for instant detection; falls back
/// to a 2-second poll if the watcher cannot be created or the watch fails.
///
/// Watches the parent directory rather than the file itself — vim/VS Code etc.
/// save via "write temp file + rename", so a direct file watch would lose events
/// after the rename. Events are debounced with a 300ms quiet window to avoid
/// firing on each write of a multi-write save (Create temp → Modify → Rename).
#[cfg(not(target_os = "android"))]
fn watch_file_changes(app: AppHandle, edit_id: String, local_path: PathBuf, cancel: Arc<AtomicBool>) {
    let dir = match local_path.parent() {
        Some(d) => d.to_path_buf(),
        None => return,
    };
    let filename = local_path.file_name().map(|f| f.to_os_string());

    tokio::spawn(async move {
        let changed_event = format!("sftp:file_changed:{edit_id}");
        let deleted_event = format!("sftp:file_deleted:{edit_id}");
        let mut baseline = match tokio::fs::metadata(&local_path).await {
            Ok(m) => m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            Err(_) => return,
        };

        // Try the notify watcher (event-driven) first; fall back to polling.
        // The watcher callback is sync, so bridge to async via an unbounded channel.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
        let watcher = match notify::recommended_watcher(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            },
        ) {
            Ok(mut w) => {
                if w.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
                    Some(w)
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        if let Some(_watcher) = watcher {
            // notify mode: event-driven with debounce.
            loop {
                tokio::select! {
                    // cancel check (100ms, lightweight)
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if cancel.load(std::sync::atomic::Ordering::Relaxed) { break; }
                    }
                    event = rx.recv() => {
                        if cancel.load(std::sync::atomic::Ordering::Relaxed) { break; }
                        let Some(event) = event else { break; };
                        // Only handle events for the target file — watching the parent
                        // dir means we also get events for siblings.
                        let matches = event.paths.iter().any(|p| p.file_name() == filename.as_deref());
                        if !matches { continue; }
                        // Remove event → confirm the file is actually gone before emitting.
                        if matches!(event.kind, notify::EventKind::Remove(_)) {
                            if tokio::fs::metadata(&local_path).await.is_err() {
                                bring_window_to_front(&app);
                                let _ = app.emit(&deleted_event, ());
                                break;
                            }
                            continue;
                        }
                        // Modify/create event → debounce: wait 300ms with no further
                        // events to avoid firing on each write of a multi-write save
                        // (Create temp → Modify → Rename).
                        loop {
                            match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
                                Ok(Some(_)) => continue,  // more events, reset the debounce window
                                Ok(None) => break,         // channel closed (watcher dropped)
                                Err(_) => break,           // 300ms quiet, debounce done
                            }
                        }
                        // Confirm the mtime actually changed (ignore spurious events).
                        match tokio::fs::metadata(&local_path).await {
                            Ok(meta) => {
                                if let Ok(mtime) = meta.modified() {
                                    if mtime > baseline {
                                        baseline = mtime;
                                        bring_window_to_front(&app);
                                        let _ = app.emit(&changed_event, ());
                                    }
                                }
                            }
                            Err(_) => {
                                bring_window_to_front(&app);
                                let _ = app.emit(&deleted_event, ());
                                break;
                            }
                        }
                    }
                }
            }
            // _watcher is dropped here, stopping the watch.
        } else {
            // Fallback: poll every 2s (notify unavailable — exotic filesystem).
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if cancel.load(std::sync::atomic::Ordering::Relaxed) { break; }
                match tokio::fs::metadata(&local_path).await {
                    Ok(meta) => {
                        if let Ok(mtime) = meta.modified() {
                            if mtime > baseline {
                                baseline = mtime;
                                bring_window_to_front(&app);
                                let _ = app.emit(&changed_event, ());
                            }
                        }
                    }
                    Err(_) => {
                        bring_window_to_front(&app);
                        let _ = app.emit(&deleted_event, ());
                        break;
                    }
                }
            }
        }
    });
}

/// Extract the last path segment of `remote_path` as the filename (used for
/// the temp file name). Path-traversal hardening: reject `.`, `..`, and any
/// name containing a path separator, falling back to `"file"`.
#[cfg(not(target_os = "android"))]
fn remote_filename(remote_path: &str) -> String {
    let name = remote_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("file");
    if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        "file".to_string()
    } else {
        name.to_string()
    }
}

/// Return value of `sftp_prepare_edit`: the edit_id and the local temp path.
#[cfg(not(target_os = "android"))]
#[derive(serde::Serialize)]
pub struct OpenForEditResult {
    pub edit_id: String,
    pub local_path: String,
}

/// Prepare an edit session: allocate an edit_id, create the temp dir, and
/// register an EditSession. Does NOT download the file (the frontend calls
/// transfers.startDownload for that) and does NOT open the file
/// (sftp_start_edit_watch does that once the download finishes). Returns the
/// edit_id and local temp path so the frontend can call transfers.startDownload
/// with the local_path.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_prepare_edit(
    state: State<'_, AppState>,
    session_id: String,
    remote_path: String,
) -> AppResult<OpenForEditResult> {
    let edit_id = uuid::Uuid::new_v4().to_string();
    let filename = remote_filename(&remote_path);
    let temp_dir = std::env::temp_dir().join("rssh-edit").join(&edit_id);
    tokio::fs::create_dir_all(&temp_dir).await?;
    let local_path = temp_dir.join(&filename);

    let session = EditSession::new(local_path.clone(), session_id);
    locked(&state.edit_sessions)?.insert(edit_id.clone(), session);

    Ok(OpenForEditResult {
        edit_id,
        local_path: local_path.display().to_string(),
    })
}

/// Called once the download finishes: opens the file with the system default
/// program and starts a notify watcher for subsequent changes. Returns
/// `not_found` if the EditSession was already cancelled (SFTP panel closed,
/// etc.). On opener failure the EditSession + temp dir are cleaned up and an
/// error is returned.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_start_edit_watch(
    app: AppHandle,
    state: State<'_, AppState>,
    edit_id: String,
) -> AppResult<()> {
    let (local_path, cancel) = {
        let sessions = locked(&state.edit_sessions)?;
        let s = sessions
            .get(&edit_id)
            .ok_or_else(|| AppError::not_found("sftp_edit_not_found", json!({})))?;
        (s.local_path.clone(), s.cancel.clone())
    };

    // Open with the system default program. open_path requires path: impl
    // Into<String>; PathBuf is not Into<String>, so convert to String first.
    let local_path_str = local_path.to_string_lossy().into_owned();
    if let Err(e) = app.opener().open_path(local_path_str, None::<String>) {
        // Opener failed → clean up the EditSession + temp dir.
        if let Some(parent) = local_path.parent() {
            let _ = tokio::fs::remove_dir_all(parent).await;
        }
        locked(&state.edit_sessions)?.remove(&edit_id);
        return Err(AppError::other(
            "sftp_edit_open_failed",
            json!({ "err": e.to_string() }),
        ));
    }

    watch_file_changes(app, edit_id, local_path, cancel);
    Ok(())
}

/// Cancel a single edit session: stop the watcher, delete the temp file, and
/// remove the session from the map.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_cancel_edit(
    state: State<'_, AppState>,
    edit_id: String,
) -> AppResult<()> {
    let session = locked(&state.edit_sessions)?.remove(&edit_id);
    if let Some(s) = session {
        s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(parent) = s.local_path.parent().map(|p| p.to_path_buf()) {
            let _ = tokio::fs::remove_dir_all(parent).await;
        }
    }
    Ok(())
}

/// Cancel every edit session belonging to an SSH session (called when the SFTP
/// panel closes or the SSH session drops).
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_cancel_edits_for_session(
    state: State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    let to_cancel: Vec<EditSession> = {
        let mut sessions = locked(&state.edit_sessions)?;
        let keys: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.session_id == session_id)
            .map(|(k, _)| k.clone())
            .collect();
        keys.into_iter().filter_map(|k| sessions.remove(&k)).collect()
    };
    for s in to_cancel {
        s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(parent) = s.local_path.parent().map(|p| p.to_path_buf()) {
            let _ = tokio::fs::remove_dir_all(parent).await;
        }
    }
    Ok(())
}
