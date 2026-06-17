/**
 * "用本地程序打开"编辑会话 store。
 *
 * 后端 sftp_open_for_edit 下载远程文件到临时目录、用 opener 打开、spawn
 * 一个 notify watcher。文件被外部编辑器保存时后端 emit `sftp:file_changed:{edit_id}`，
 * 并通过 request_user_attention 把窗口拉到前台 + 闪烁任务栏图标；本 store
 * 监听该事件、把对应 session 标 pendingChange=true 让 SftpBrowser 弹模态框。
 * 用户点"上传" → sftp_accept_edit 回传远端；点"取消" → 清 pendingChange（不回传）。
 * SFTP 面板关闭 / SSH 断连 → cancelAllForSession 清理所有相关会话。
 *
 * 跟 transfers store 一样是模块级 $state，不在组件内建全局状态（R8）。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { errMsg, t } from "../i18n/index.svelte.ts";
import { toast } from "./toast.svelte.ts";

export interface EditSession {
    editId: string;
    localPath: string;
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
 * 用本地程序打开远程文件。下载 → 打开 → 开始轮询。
 * @param sftpId    当前 SftpBrowser 的 SFTP channel id
 * @param sessionId 父 SSH session id（回传时新开 SFTP channel）
 * @param remotePath 远端文件完整路径
 * @param remoteName 文件名（用于 UI 显示）
 */
export async function startEdit(
    sftpId: string,
    sessionId: string,
    remotePath: string,
    remoteName: string,
): Promise<void> {
    const result = await invoke<{ edit_id: string; local_path: string }>(
        "sftp_open_for_edit",
        { sftpId, sessionId, remotePath },
    );

    const session: EditSession = {
        editId: result.edit_id,
        localPath: result.local_path,
        remoteName,
        sessionId,
        pendingChange: false,
    };
    _list = [..._list, session];

    // 监听 file_changed / file_deleted。
    // 窗口激活（unminimize + set_focus + request_user_attention）已由后端
    // poll_file_changes 在 emit 之前完成，前端只需设置 pendingChange 弹模态框。
    const unlistenChanged = await listen(`sftp:file_changed:${result.edit_id}`, () => {
        const s = find(result.edit_id);
        if (s && !s.pendingChange) {
            s.pendingChange = true;
        }
    });
    const unlistenDeleted = await listen(`sftp:file_deleted:${result.edit_id}`, () => {
        // 临时文件被删除（用户手动删 / 外部程序清理）→ 自动取消，不回传。
        void cancelEdit(result.edit_id);
        toast.info(t("sftp.edit.deleted", { name: remoteName }));
    });
    _unlisteners.set(result.edit_id, () => {
        unlistenChanged();
        unlistenDeleted();
    });
}

/** 用户点"上传"：回传远端，清 pendingChange。轮询器继续跑（用户可能再保存）。 */
export async function acceptEdit(editId: string): Promise<void> {
    const s = find(editId);
    if (!s) return;
    try {
        await invoke("sftp_accept_edit", { editId });
        s.pendingChange = false;
    } catch (e: any) {
        toast.error(`${t("sftp.edit.upload_failed")}: ${errMsg(e)}`);
    }
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
