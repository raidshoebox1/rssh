use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::json;
use tauri::{AppHandle, Emitter, State};
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

/// Open native Open dialog and return the chosen path. No transfer happens here.
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_pick_open_path() -> AppResult<Option<String>> {
    let handle = rfd::AsyncFileDialog::new().pick_file().await;
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

// ─── 用本地程序打开远程文件（编辑模式）─────────────────────────────────────
//
// 流程：sftp_open_for_edit 下载文件到临时目录 → 用 opener 打开 → spawn
// 一个轮询器每 2 秒检查 mtime，变化时 emit `sftp:file_changed:{edit_id}`。
// 前端弹模态框 → sftp_accept_edit（上传回远程）或 sftp_cancel_edit（删临时文件）。
// 轮询器通过 EditSession.cancel 控制；SSH 断连 / SFTP 面板关闭时前端调
// sftp_cancel_edit 清理。

/// 一个活跃的"用本地程序打开"编辑会话。轮询器持有 cancel 的克隆，
/// sftp_cancel_edit 置位后轮询器下次循环退出。
#[cfg(not(target_os = "android"))]
pub struct EditSession {
    /// 临时文件本地路径（{temp}/rssh-edit/{edit_id}/{filename}）。
    pub local_path: PathBuf,
    /// 远端路径，回传时用。
    pub remote_path: String,
    /// 父 SSH session id，回传时新开 SFTP channel。
    pub session_id: String,
    /// 停止轮询器。
    pub cancel: Arc<AtomicBool>,
}

#[cfg(not(target_os = "android"))]
impl EditSession {
    fn new(local_path: PathBuf, remote_path: String, session_id: String) -> (Self, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let session = Self {
            local_path,
            remote_path,
            session_id,
            cancel: cancel.clone(),
        };
        (session, cancel)
    }
}

/// 轮询临时文件 mtime。变化 → emit `sftp:file_changed:{edit_id}`；删除 →
/// emit `sftp:file_deleted:{edit_id}` 并退出。
#[cfg(not(target_os = "android"))]
fn poll_file_changes(app: AppHandle, edit_id: String, local_path: PathBuf, cancel: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut baseline = match std::fs::metadata(&local_path) {
            Ok(m) => m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            Err(_) => return,
        };
        let changed_event = format!("sftp:file_changed:{edit_id}");
        let deleted_event = format!("sftp:file_deleted:{edit_id}");
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match std::fs::metadata(&local_path) {
                Ok(meta) => {
                    if let Ok(mtime) = meta.modified() {
                        if mtime > baseline {
                            baseline = mtime;
                            let _ = app.emit(&changed_event, ());
                        }
                    }
                }
                Err(_) => {
                    let _ = app.emit(&deleted_event, ());
                    break;
                }
            }
        }
    });
}

/// 从 remote_path 提取最后一段作为 filename（用于临时文件名）。
#[cfg(not(target_os = "android"))]
fn remote_filename(remote_path: &str) -> String {
    remote_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("file")
        .to_string()
}

/// 用本地程序打开远程文件。下载到 {temp}/rssh-edit/{edit_id}/{filename}，
/// 用 opener 打开，spawn 轮询器检测后续修改。
///
/// `open_with` 为 None 时用系统默认程序；Some(path) 时用指定程序（"打开为"）。
/// `session_id` 是父 SSH session id，用于回传时新开 SFTP channel。
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_open_for_edit(
    app: AppHandle,
    state: State<'_, AppState>,
    sftp_id: String,
    session_id: String,
    remote_path: String,
    open_with: Option<String>,
) -> AppResult<serde_json::Value> {
    let sftp = get_sftp(&state, &sftp_id)?;
    let data = sftp.download(&remote_path).await?;

    let edit_id = uuid::Uuid::new_v4().to_string();
    let filename = remote_filename(&remote_path);
    let temp_dir = std::env::temp_dir().join("rssh-edit").join(&edit_id);
    tokio::fs::create_dir_all(&temp_dir).await?;
    let local_path = temp_dir.join(&filename);
    tokio::fs::write(&local_path, &data).await?;

    // 用 opener 打开。open_with = None → 默认程序；Some → 指定程序。
    // open_path 的签名要求 path: impl Into<String>，PathBuf 不直接 Into<String>，
    // 所以转成 String 传入。
    let local_path_str = local_path.to_string_lossy().into_owned();
    let open_result = if let Some(ref prog) = open_with {
        app.opener().open_path(local_path_str.clone(), Some(prog.clone()))
    } else {
        app.opener().open_path(local_path_str.clone(), None::<String>)
    };
    if let Err(e) = open_result {
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        return Err(AppError::other(
            "sftp_edit_open_failed",
            json!({ "err": e.to_string() }),
        ));
    }

    let (session, cancel) = EditSession::new(local_path.clone(), remote_path, session_id);
    locked(&state.edit_sessions)?.insert(edit_id.clone(), session);
    poll_file_changes(app, edit_id.clone(), local_path.clone(), cancel);

    Ok(serde_json::json!({
        "edit_id": edit_id,
        "local_path": local_path.display().to_string()
    }))
}

/// 接受外部编辑器的修改：读取本地临时文件 → 新开 SFTP channel 上传回远端。
/// 不移除 EditSession —— 用户可能再次保存，轮询器继续跑。
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn sftp_accept_edit(
    state: State<'_, AppState>,
    edit_id: String,
) -> AppResult<()> {
    let (local_path, remote_path, session_id) = {
        let sessions = locked(&state.edit_sessions)?;
        let s = sessions
            .get(&edit_id)
            .ok_or_else(|| AppError::not_found("sftp_edit_not_found", json!({})))?;
        (s.local_path.clone(), s.remote_path.clone(), s.session_id.clone())
    };

    // 新开 SFTP channel（不复用 SftpBrowser 的 sftp_id，它可能已关闭）。
    let ssh_handle = {
        let sessions = locked(&state.sessions)?;
        sessions
            .get(&session_id)
            .ok_or_else(|| AppError::not_found("ssh_session_not_found", json!({})))?
            .ssh_handle()
            .clone()
    };
    let parent = session_id.clone();
    let sftp = crate::ssh::client::run_blocking_ssh(move || async move {
        SftpHandle::from_handle(&ssh_handle, parent).await
    })
    .await?;
    let sftp_id = uuid::Uuid::new_v4().to_string();
    locked(&state.sftp_sessions)?.insert(sftp_id.clone(), Arc::new(sftp));

    // 读取本地临时文件 → 上传回远端。两个操作错误类型不同（io::Error vs AppError），
    // 分别处理，避免 ? 在混合类型上失败。
    // Arc clone 让 async 块拥有自己的引用，不借用被 move 的 sftp。
    let sftp_handle = {
        let sessions = locked(&state.sftp_sessions)?;
        sessions
            .get(&sftp_id)
            .cloned()
            .ok_or_else(|| AppError::not_found("sftp_session_not_found", json!({})))?
    };
    let result: AppResult<()> = async {
        let data = tokio::fs::read(&local_path)
            .await
            .map_err(|e| AppError::other("sftp_edit_read_failed", json!({ "err": e.to_string() })))?;
        sftp_handle.upload(&remote_path, &data).await
    }
    .await;

    // 无论成功失败都关掉临时 SFTP channel。
    locked(&state.sftp_sessions)?.remove(&sftp_id);

    result?;

    Ok(())
}

/// 取消编辑会话：停止轮询器，删除临时文件，从 map 移除。
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub fn sftp_cancel_edit(state: State<'_, AppState>, edit_id: String) -> AppResult<()> {
    let session = locked(&state.edit_sessions)?.remove(&edit_id);
    if let Some(s) = session {
        s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(parent) = s.local_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
    Ok(())
}

/// 批量取消某 SSH session 的所有编辑会话（SFTP 面板关闭 / SSH 断连时调用）。
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub fn sftp_cancel_edits_for_session(
    state: State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    let mut sessions = locked(&state.edit_sessions)?;
    let keys: Vec<String> = sessions
        .iter()
        .filter(|(_, s)| s.session_id == session_id)
        .map(|(k, _)| k.clone())
        .collect();
    for k in keys {
        if let Some(s) = sessions.remove(&k) {
            s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(parent) = s.local_path.parent() {
                let _ = std::fs::remove_dir_all(parent);
            }
        }
    }
    Ok(())
}
