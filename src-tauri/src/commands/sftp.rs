use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::json;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_opener::OpenerExt;

/// 桌面端专用 import：notify 文件系统事件监听 + tauri::Manager（get_webview_windows）。
/// Android 上这些模块不存在，与函数级 #[cfg(not(target_os = "android"))] 配合。
#[cfg(not(target_os = "android"))]
use notify::{RecursiveMode, Watcher};

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

// ─── 用本地程序打开远程文件（编辑模式）─────────────────────────────────────
//
// 流程（全复用 transfers 管道，下载/上传均在传输列表可见）：
//   1. sftp_prepare_edit    生成 edit_id + 临时目录，注册 EditSession（不下载）
//   2. 前端 transfers.startDownload（流式下载，传输列表可见，可取消）
//   3. sftp_start_edit_watch 下载完成后调用：opener 打开 + spawn notify watcher
//   4. 文件被外部编辑器保存 → watcher emit `sftp:file_changed:{edit_id}`
//   5. 前端弹模态框 → transfers.startUpload（流式上传，传输列表可见，可取消）
//      或 sftp_cancel_edit（删临时文件，停 watcher）
//   SSH 断连 / SFTP 面板关闭 → sftp_cancel_edits_for_session 批量清理。
//
// EditSession 在步骤 1 创建、步骤 3 启动 watcher。cancel 在步骤 3 从 session
// 克隆给 watcher；sftp_cancel_edit 置位后 watcher 下次循环退出。

/// 一个活跃的"用本地程序打开"编辑会话。watcher 持有 cancel 的克隆，
/// sftp_cancel_edit 置位后 watcher 下次循环退出。
#[cfg(not(target_os = "android"))]
pub struct EditSession {
    /// 临时文件本地路径（{temp}/rssh-edit/{edit_id}/{filename}）。
    pub local_path: PathBuf,
    /// 父 SSH session id，sftp_cancel_edits_for_session 按它匹配批量清理。
    pub session_id: String,
    /// 停止 watcher。
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

/// 将应用所有窗口拉到前台 + 闪烁任务栏图标（Windows）/ Dock 弹跳（macOS）/
/// urgency hint（Linux）。`request_user_attention(Critical)` 是 OS 认可的
/// "请求注意"方式，不受后台进程焦点窃取保护限制。
#[cfg(not(target_os = "android"))]
fn bring_window_to_front(app: &AppHandle) {
    use tauri::Manager;
    for (_, win) in app.webview_windows() {
        let _ = win.unminimize();
        let _ = win.set_focus();
        let _ = win.request_user_attention(Some(tauri::UserAttentionType::Critical));
    }
}

/// 监听临时文件变更。用 notify watcher（inotify/FSEvents/ReadDirectoryChangesW）
/// 替代轮询，实现即时变更检测。watcher 创建/监听失败时退化为 2 秒轮询。
///
/// 监听父目录而非文件本身——vim/VS Code 等编辑器用"写临时文件 + rename"保存，
/// 直接 watch 文件在 rename 后会丢失事件。事件去抖 300ms 避免连续写入的抖动。
#[cfg(not(target_os = "android"))]
fn poll_file_changes(app: AppHandle, edit_id: String, local_path: PathBuf, cancel: Arc<AtomicBool>) {
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

        // 尝试用 notify watcher（即时事件）；失败则退化为 2 秒轮询。
        // watcher 回调是同步的，用 unbounded channel 桥接到 async。
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
            // notify 模式：事件驱动 + 去抖
            loop {
                tokio::select! {
                    // cancel 检查（100ms 一次，轻量）
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if cancel.load(std::sync::atomic::Ordering::Relaxed) { break; }
                    }
                    event = rx.recv() => {
                        if cancel.load(std::sync::atomic::Ordering::Relaxed) { break; }
                        let Some(event) = event else { break; };
                        // 只处理目标文件的事件（watch 父目录会收到同目录其他文件的事件）
                        let matches = event.paths.iter().any(|p| p.file_name() == filename.as_deref());
                        if !matches { continue; }
                        // 删除事件 → 确认文件确实没了再 emit
                        if matches!(event.kind, notify::EventKind::Remove(_)) {
                            if tokio::fs::metadata(&local_path).await.is_err() {
                                bring_window_to_front(&app);
                                let _ = app.emit(&deleted_event, ());
                                break;
                            }
                            continue;
                        }
                        // 修改/创建事件 → 去抖：等 300ms 无新事件，避免编辑器
                        // 保存时的连续写入（Create temp → Modify → Rename）触发多次
                        loop {
                            match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
                                Ok(Some(_)) => continue,  // 还有事件，重置去抖窗口
                                Ok(None) => break,         // channel 关闭（watcher drop）
                                Err(_) => break,           // 300ms 无新事件，去抖结束
                            }
                        }
                        // 确认实际 mtime 变化（spurious 事件不触发误报）
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
            // _watcher 在此 drop，停止监听
        } else {
            // 退化为轮询（notify 创建/监听失败——exotic 文件系统）
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

/// 从 remote_path 提取最后一段作为 filename（用于临时文件名）。
/// 防御路径穿越：拒绝 `.`、`..`、含路径分隔符的名称，回退到 `"file"`。
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

/// `sftp_prepare_edit` 的返回值：edit_id + 本地临时路径。
#[cfg(not(target_os = "android"))]
#[derive(serde::Serialize)]
pub struct OpenForEditResult {
    pub edit_id: String,
    pub local_path: String,
}

/// 准备编辑会话：生成 edit_id、创建临时目录、注册 EditSession。
/// 不下载文件（由传输系统的 startDownload 负责）、不打开文件（由
/// sftp_start_edit_watch 负责）。返回 edit_id 和本地临时路径，前端用
/// local_path 调 transfers.startDownload。
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

/// 下载完成后调用：用 opener 打开本地文件，启动 notify watcher 监听后续修改。
/// 若 EditSession 已被 sftp_cancel_edit 清理（SFTP 面板关闭等），返回 not_found。
/// opener 打开失败时清理 EditSession + 临时目录后返回错误。
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

    // 用 opener 以系统默认程序打开。open_path 要求 path: impl Into<String>，
    // PathBuf 不直接 Into<String>，所以转成 String 传入。
    let local_path_str = local_path.to_string_lossy().into_owned();
    if let Err(e) = app.opener().open_path(local_path_str, None::<String>) {
        // 打开失败 → 清理 EditSession + 临时目录。
        if let Some(parent) = local_path.parent() {
            let _ = tokio::fs::remove_dir_all(parent).await;
        }
        locked(&state.edit_sessions)?.remove(&edit_id);
        return Err(AppError::other(
            "sftp_edit_open_failed",
            json!({ "err": e.to_string() }),
        ));
    }

    poll_file_changes(app, edit_id, local_path, cancel);
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
