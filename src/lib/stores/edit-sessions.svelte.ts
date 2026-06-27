/**
 * "Open with local program" edit-session store.
 *
 * Reuses the transfers pipeline for both download and upload (both show in the
 * transfer list, stream, and are cancellable):
 *   1. sftp_prepare_edit        — allocate edit_id + temp dir, register EditSession
 *   2. transfers.startDownload  — stream the file into the temp dir (transfer list)
 *   3. download done → sftp_start_edit_watch opens the file + starts a notify watcher
 *   4. external editor saves the file → backend emits sftp:file_changed:{edit_id}
 *      → frontend sets pendingChange=true and shows a modal
 *   5. user clicks "Upload"  → transfers.startUpload streams it back (transfer list)
 *      user clicks "Cancel"  → dismissChange (clears pendingChange, does NOT upload)
 *   SFTP panel close / SSH disconnect → cancelAllForSession cleans up every session.
 *
 * Module-level $state like the transfers store — no component-local globals (R8).
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { errMsg, t } from "../i18n/index.svelte.ts";
import { toast } from "./toast.svelte.ts";
import * as transfers from "./transfers.svelte.ts";

export interface EditSession {
    editId: string;
    localPath: string;
    /** Full remote path; needed by transfers.startUpload when uploading back. */
    remotePath: string;
    remoteName: string;
    /** Parent SSH session id; used by cancelAllForSession for matching. */
    sessionId: string;
    /** true = file changed, modal is showing. */
    pendingChange: boolean;
    /** true = a new change arrived while the modal was already open. */
    hasNewChange?: boolean;
}

let _list = $state<EditSession[]>([]);

/** editId → unlisten (combined file_changed / file_deleted listener). */
const _unlisteners = new Map<string, UnlistenFn>();

function find(editId: string): EditSession | undefined {
    return _list.find((s) => s.editId === editId);
}

/**
 * Open a remote file with a local program. Flow:
 *   prepare_edit → transfers.startDownload → waitDone → start_edit_watch
 * Silently cleans up (no modal) when the download fails/is cancelled or the
 * edit session was already cancelled.
 * @param sessionId Parent SSH session id (the transfer system opens its own SFTP channel).
 * @param remotePath Full remote file path.
 * @param remoteName File name (for UI display).
 */
export async function startEdit(
    sessionId: string,
    remotePath: string,
    remoteName: string,
): Promise<void> {
    // Stage 1: backend creates the temp dir + registers the EditSession.
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

    // Stage 2: download via the transfer system (visible in the transfer list, cancellable).
    const transferId = await transfers.startDownload({
        sessionId,
        remotePath,
        localPath: local_path,
        editMode: true,
    });

    // Stage 3: once the download is done, open the file + start the watcher.
    void transfers.waitDone(transferId).then(async () => {
        // The edit session may have been cancelled (SFTP panel closed, etc.) → skip silently.
        if (!find(edit_id)) return;
        const tr = transfers.list().find((x) => x.id === transferId);
        if (!tr || tr.status !== "done") {
            // Download failed/cancelled → clean up the backend EditSession + temp dir.
            await invoke("sftp_cancel_edit", { editId: edit_id }).catch(() => {});
            _list = _list.filter((s) => s.editId !== edit_id);
            if (tr && tr.status === "failed") {
                toast.error(`${t("sftp.edit.open_failed")}: ${tr.error ?? ""}`);
            }
            return;
        }
        // Download succeeded → open the file + start the notify watcher.
        try {
            await invoke("sftp_start_edit_watch", { editId: edit_id });
        } catch (e: any) {
            await invoke("sftp_cancel_edit", { editId: edit_id }).catch(() => {});
            _list = _list.filter((s) => s.editId !== edit_id);
            toast.error(`${t("sftp.edit.open_failed")}: ${errMsg(e)}`);
            return;
        }
        // Register the file_changed / file_deleted listeners.
        // Window activation (unminimize + set_focus + request_user_attention) is
        // already done by watch_file_changes on the backend before emitting, so
        // the frontend only sets pendingChange to show the modal.
        let unlistenChanged: UnlistenFn;
        let unlistenDeleted: UnlistenFn;
        try {
            unlistenChanged = await listen(`sftp:file_changed:${edit_id}`, () => {
                const s = find(edit_id);
                if (!s) return;
                if (s.pendingChange) {
                    // A newer save happened while the modal was already open.
                    // Re-show it once the user dismisses the current one.
                    s.hasNewChange = true;
                } else {
                    s.pendingChange = true;
                }
            });
            unlistenDeleted = await listen(`sftp:file_deleted:${edit_id}`, () => {
                // Temp file deleted (user deleted it / external cleanup) → auto-cancel, no upload.
                void cancelEdit(edit_id);
                toast.info(t("sftp.edit.deleted", { name: remoteName }));
            });
        } catch (e: any) {
            // listen failed (Tauri event system error) → clean up; we can't detect changes.
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

/** User clicks "Upload": upload back to the remote via the transfer system,
 *  clear pendingChange. The watcher keeps running (the user may save again).
 *  Failure shows a toast. */
export async function acceptEdit(editId: string): Promise<void> {
    const s = find(editId);
    if (!s) return;
    s.pendingChange = false;
    s.hasNewChange = false;
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

/** User clicks "Cancel" (modal): clear pendingChange, do NOT upload. The
 *  watcher keeps running. If a newer save arrived while the modal was open,
 *  re-show it immediately so the user is aware of the latest change. */
export function dismissChange(editId: string): void {
    const s = find(editId);
    if (!s) return;
    if (s.hasNewChange) {
        s.pendingChange = true;
        s.hasNewChange = false;
    } else {
        s.pendingChange = false;
    }
}

/**
 * Cancel a single edit session: stop the watcher, delete the temp file, and
 * remove it from the map. Used when the temp file is deleted (file_deleted
 * event) for automatic cleanup.
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
        // Backend may already have cleaned up (sftp_cancel_edits_for_session ran
        // first on SSH disconnect).
    }
}

/**
 * Cancel every edit session for an SSH session. Called when the SFTP panel
 * closes or the SSH session drops. The backend matches on session_id to clean
 * up temp files and stop watchers; the frontend clears listeners + the list.
 */
export async function cancelAllForSession(sessionId: string): Promise<void> {
    const matching = _list.filter((s) => s.sessionId === sessionId);
    if (matching.length === 0) return;

    // Backend cleans up in one shot (matched by session_id).
    try {
        await invoke("sftp_cancel_edits_for_session", { sessionId });
    } catch {
        // On SSH disconnect the backend session may already be gone — ignore.
    }

    // Frontend: clear listeners + the list.
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
