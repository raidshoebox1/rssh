/**
 * "用本地程序打开"编辑会话 store。
 *
 * 全流程复用 transfers 管道（下载/上传均在传输列表可见，流式、可取消）：
 *   1. sftp_prepare_edit    生成 edit_id + 临时目录，注册 EditSession（不下载）
 *   2. transfers.startDownload  流式下载到临时目录（传输列表可见）
 *   3. 下载完成 → sftp_start_edit_watch  opener 打开 + notify watcher
 *   4. 文件被外部编辑器保存 → 后端 emit sftp:file_changed:{edit_id}
 *      → 前端设 pendingChange=true，弹模态框
 *   5. 用户点"上传" → transfers.startUpload  流式上传回远端（传输列表可见）
 *      用户点"取消" → dismissChange（清 pendingChange，不回传）
 *   SFTP 面板关闭 / SSH 断连 → cancelAllForSession 清理所有相关会话。
 *
 * 跟 transfers store 一样是模块级 $state，不在组件内建全局状态（R8）。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { errMsg, t } from "../i18n/index.svelte.ts";
import { toast } from "./toast.svelte.ts";
import * as transfers from "./transfers.svelte.ts";

export interface EditSession {
    editId: string;
    localPath: string;
    /** 远端完整路径，上传时用（transfers.startUpload 需要）。 */
    remotePath: string;
    remoteName: string;
    /** 父 SSH session id，用于 cancelAllForSession 匹配。 */
    sessionId: string;
    /** true = 文件已变，模态框正显示 */
    pendingChange: boolean;
}

let _list = $state<EditSession[]>([]);

/** editId → unlisten（file_changed / file_deleted 两个 listener 合并） */
const _unlisteners = new Map<string, UnlistenFn>();

function find(editId: string): EditSession | undefined {
    return _list.find((s) => s.editId === editId);
}

/**
 * 用本地程序打开远程文件。流程：
 *   prepare_edit → transfers.startDownload → waitDone → start_edit_watch
 * 下载失败/取消/编辑会话已被取消时静默清理，不弹模态框。
 * @param sessionId 父 SSH session id（传输系统开自己的 SFTP channel）
 * @param remotePath 远端文件完整路径
 * @param remoteName 文件名（用于 UI 显示）
 */
export async function startEdit(
    sessionId: string,
    remotePath: string,
    remoteName: string,
): Promise<void> {
    // 阶段 1: 后端创建临时目录 + 注册 EditSession
    const { edit_id, local_path } = await invoke<{ edit_id: string; local_path: string }>(
        "sftp_prepare_edit",
        { sessionId, remotePath },
    );

    const session: EditSession = {
        editId: edit_id,
        localPath: local_path,
        remotePath,
        remoteName,
        sessionId,
        pendingChange: false,
    };
    _list = [..._list, session];

    // 阶段 2: 通过传输系统下载（传输列表可见，流式，可取消）
    const transferId = await transfers.startDownload({
        sessionId,
        remotePath,
        localPath: local_path,
        editMode: true,
    });

    // 阶段 3: 下载完成后打开文件 + 启动监听
    void transfers.waitDone(transferId).then(async () => {
        // 编辑会话可能已被取消（SFTP 面板关闭等）→ 静默跳过
        if (!find(edit_id)) return;
        const tr = transfers.list().find((x) => x.id === transferId);
        if (!tr || tr.status !== "done") {
            // 下载失败/取消 → 清理后端 EditSession + 临时目录
            await invoke("sftp_cancel_edit", { editId: edit_id }).catch(() => {});
            _list = _list.filter((s) => s.editId !== edit_id);
            if (tr && tr.status === "failed") {
                toast.error(`${t("sftp.edit.open_failed")}: ${tr.error ?? ""}`);
            }
            return;
        }
        // 下载成功 → 打开文件 + 启动 notify watcher
        try {
            await invoke("sftp_start_edit_watch", { editId: edit_id });
        } catch (e: any) {
            await invoke("sftp_cancel_edit", { editId: edit_id }).catch(() => {});
            _list = _list.filter((s) => s.editId !== edit_id);
            toast.error(`${t("sftp.edit.open_failed")}: ${errMsg(e)}`);
            return;
        }
        // 注册 file_changed / file_deleted 监听。
        // 窗口激活（unminimize + set_focus + request_user_attention）已由后端
        // poll_file_changes 在 emit 之前完成，前端只需设置 pendingChange 弹模态框。
        let unlistenChanged: UnlistenFn;
        let unlistenDeleted: UnlistenFn;
        try {
            unlistenChanged = await listen(`sftp:file_changed:${edit_id}`, () => {
                const s = find(edit_id);
                if (s && !s.pendingChange) {
                    s.pendingChange = true;
                }
            });
            unlistenDeleted = await listen(`sftp:file_deleted:${edit_id}`, () => {
                // 临时文件被删除（用户手动删 / 外部程序清理）→ 自动取消，不回传。
                void cancelEdit(edit_id);
                toast.info(t("sftp.edit.deleted", { name: remoteName }));
            });
        } catch (e: any) {
            // listen 失败（Tauri 事件系统异常）→ 清理，无法检测后续变更。
            await invoke("sftp_cancel_edit", { editId: edit_id }).catch(() => {});
            _list = _list.filter((s) => s.editId !== edit_id);
            toast.error(`${t("sftp.edit.open_failed")}: ${errMsg(e)}`);
            return;
        }
        _unlisteners.set(edit_id, () => {
            unlistenChanged();
            unlistenDeleted();
        });
    }).catch((e) => {
        console.error("[edit-sessions] startEdit download callback failed:", e);
    });
}

/** 用户点"上传"：通过传输系统上传回远端，清 pendingChange。
 *  轮询器继续跑（用户可能再次保存）。失败弹 toast。 */
export async function acceptEdit(editId: string): Promise<void> {
    const s = find(editId);
    if (!s) return;
    s.pendingChange = false;
    const id = await transfers.startUpload({
        sessionId: s.sessionId,
        localPath: s.localPath,
        remotePath: s.remotePath,
        editMode: true,
    });
    void transfers.waitDone(id).then(() => {
        const tr = transfers.list().find((x) => x.id === id);
        if (tr && tr.status === "failed") {
            toast.error(`${t("sftp.edit.upload_failed")}: ${tr.error ?? ""}`);
        }
    }).catch((e) => {
        console.error("[edit-sessions] acceptEdit upload callback failed:", e);
    });
}

/** 用户点"取消"（模态框）：清 pendingChange，不回传。轮询器继续跑。 */
export function dismissChange(editId: string): void {
    const s = find(editId);
    if (s) s.pendingChange = false;
}

/**
 * 取消单个编辑会话：停止轮询器、删临时文件、从 map 移除。
 * 用于临时文件被删除（file_deleted 事件）时自动清理。
 */
export async function cancelEdit(editId: string): Promise<void> {
    const unlisten = _unlisteners.get(editId);
    if (unlisten) {
        unlisten();
        _unlisteners.delete(editId);
    }
    _list = _list.filter((s) => s.editId !== editId);
    try {
        await invoke("sftp_cancel_edit", { editId });
    } catch {
        // 后端可能已经清理（SSH 断连时 sftp_cancel_edits_for_session 先跑过）。
    }
}

/**
 * 批量取消某 SSH session 的所有编辑会话。
 * SFTP 面板关闭 / SSH 断连时调用。后端按 session_id 匹配清理临时文件 +
 * 停轮询器；前端清 listener + 列表。
 */
export async function cancelAllForSession(sessionId: string): Promise<void> {
    const matching = _list.filter((s) => s.sessionId === sessionId);
    if (matching.length === 0) return;

    // 后端一次清完（按 session_id 匹配）。
    try {
        await invoke("sftp_cancel_edits_for_session", { sessionId });
    } catch {
        // SSH 断连时后端 session 可能已不在——忽略。
    }

    // 前端清 listener + 列表。
    for (const s of matching) {
        const unlisten = _unlisteners.get(s.editId);
        if (unlisten) {
            unlisten();
            _unlisteners.delete(s.editId);
        }
    }
    _list = _list.filter((s) => s.sessionId !== sessionId);
}

export function editSessions(): EditSession[] {
    return _list;
}
