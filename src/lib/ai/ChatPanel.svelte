<script lang="ts">
    import * as ai from "./store.svelte.ts";
    import * as app from "../stores/app.svelte.ts";
    import type { AiTargetKind, ChatItem, ConversationMeta } from "./types.ts";
    import CommandConfirmDialog from "./CommandConfirmDialog.svelte";
    import AuditPanel from "./AuditPanel.svelte";
    import Modal from "../components/Modal.svelte";
    import DangerModeToggle from "./DangerModeToggle.svelte";
    import BlockContextMenu, {type MenuItem} from "../components/BlockContextMenu.svelte";
    import { renderMarkdown } from "./markdown.ts";
    import { formatTokenCount } from "./tokens.ts";
    import { t, errMsg } from "../i18n/index.svelte.ts";
    import { toast } from "../stores/toast.svelte.ts";
    import { readText as readClipboard, writeText as writeClipboard } from "../clipboard.ts";
    import { onMount } from "svelte";

    // tabId 是 AI 会话身份（切 tab / 重连不丢；显式关闭面板时结束）。
    // targetId 是当前 SSH/PTY session_id —— 给 executeCommand 路由 ssh_write/pty_write 用。
    // 重连后 targetId 会换（前端 prop 自动跟随），tabId 不变。
    let { tabId, targetKind, targetId, active } = $props<{
        tabId: string;
        targetKind: AiTargetKind;
        targetId: string | null;
        active: boolean;
    }>();

    type PanelOwner = Readonly<{
        tabId: string;
        targetKind: AiTargetKind;
        lease: ai.SessionLease;
    }>;
    const snapshotOwner = (): PanelOwner => ({
        tabId,
        targetKind,
        lease: ai.captureSessionLease(tabId),
    });

    let inputText = $state("");
    let auditOpen = $state(false);
    let busy = $state(false);
    let banner = $state<string | null>(null);
    let inputEl = $state<HTMLTextAreaElement | null>(null);
    let chatBoxEl = $state<HTMLDivElement | null>(null);
    let showClearDialog = $state(false);
    let clearDialogOwner = $state<PanelOwner | null>(null);
    let rollingBack = $state(false);
    let rollbackDialog = $state<{
        owner: PanelOwner;
        instanceId: string;
        userMessageIndex: number;
        text: string;
    } | null>(null);

    let session = $derived(ai.sessionForTab(tabId));
    let items: ChatItem[] = $derived(ai.chatItems(tabId));
    // 流式响应进行中 —— send 按钮换成"停止"按钮。依赖 items 变化重算（last item 的 streaming flag）。
    let streaming = $derived(ai.isStreaming(tabId));
    // 危险模式标记 —— 用户在 AI Settings 里切换后，标题旁的红色后缀立刻同步。
    // 走 ai.settings() 读 store 的 $state，自动响应式（不需要手动 loadSettings 触发）。
    let dangerMode = $derived(ai.settings()?.danger_mode === true);
    // 本会话累计 token 用量（actor 生命周期，清上下文不归零——花掉的钱不会退）。
    let tokens = $derived(ai.tokenUsage(tabId));
    // Currently running model: prefer the model the active session actually started
    // with (authoritative — a later settings change doesn't affect a live session);
    // fall back to the configured model (what will run) when there's no session yet.
    // Empty string when neither is known — the .model span still works as the spring.
    let currentModel = $derived(session?.model ?? ai.settings()?.model ?? "");

    // 该 profile 下持久化的历史对话 —— 仅会话未启动时展示（picker）。
    // null = 还没加载完，与空数组（确无历史）区分，避免列表闪现。
    let conversations = $state<ConversationMeta[] | null>(null);

    onMount(async () => {
        // 只拉 settings（提示词标题的 danger 旗等要它）。不在这里预启 session ——
        // shell 探测已移到 SSH 连接成功时跑（TerminalPane），开 panel 不再为探测拉
        // 起 actor。会话改为首次发消息时（send → ensureSession）惰性启动。
        if (!ai.settings()) {
            try { await ai.loadSettings(); } catch { /* 静默 */ }
        }
    });

    // 色条"发送到 AI"塞进来的输入：消费一次就清掉，避免切 tab 回来又灌一遍。
    // 无条件先把 $state 读进局部变量，确保依赖被跟踪（即便为 null）——否则首跑
    // 为空时 Svelte 5 不会登记对 _prefill 的依赖，后续 prefill 永远触发不了。
    $effect(() => {
        const p = ai.pendingPrefill(tabId);
        if (!p) return;
        // 在光标位置插入(替换选区,保留两侧草稿),与 pasteIntoInput 语义一致,
        // 避免把用户已输入的草稿整个覆盖掉。面板刚 openPanel 挂载、inputEl
        // 尚为 null 时 fallback 到末尾追加(此时草稿为空,等效于原覆盖行为)。
        const el = inputEl;
        const start = el?.selectionStart ?? inputText.length;
        const end = el?.selectionEnd ?? inputText.length;
        inputText = inputText.slice(0, start) + p.text + inputText.slice(end);
        auditOpen = false;
        ai.clearPrefill(tabId);
        if (active) el?.focus();
        if (el) {
            const pos = start + p.text.length;
            requestAnimationFrame(() => {
                try { el.setSelectionRange(pos, pos); } catch { /* no-op */ }
            });
        }
    });

    // 固定挂载的隐藏面板不能保留全局 modal：否则 A 隐藏后仍会拦截 B 的 Esc。
    // 草稿等面板内状态继续保留，只关闭越出 panel 边界的确认层。
    $effect(() => {
        if (active) return;
        showClearDialog = false;
        clearDialogOwner = null;
        rollbackDialog = null;
        // 右键菜单也属于越出 panel 边界的浮层：键盘切 tab（无 mousedown）不会
        // 触发 BlockContextMenu 的 outside-click 关闭，这里兜底清掉，避免回到
        // 这个 tab 时菜单还在旧坐标上挂着。
        chatCtxMenu = null;
        inputCtxMenu = null;
    });

    // 历史对话随当前 target（同一 tab 重连时 session id 会变）重新加载。
    // seq 守卫丢弃旧连接的迟到响应，避免它覆盖新连接的列表。
    let convSeq = 0;
    $effect(() => {
        const kind = targetKind;
        const id = targetId;
        const seq = ++convSeq;
        if (session) return; // 会话已存在：picker 不展示，无需拉取
        conversations = null;
        if (!id) return; // 断线期间保活面板，但不拿 null target 查历史
        // 两个回调都同时 gate seq + session：用户开面板后立刻发消息，会话先
        // 起来、列表请求后返回 —— 此时 picker 已无意义，迟到的失败不该在活跃
        // 对话里弹错误 banner（seq 不增长，单靠它挡不住这条路径）。
        ai.listConversations(kind, id)
            .then((list) => { if (seq === convSeq && !session) conversations = list; })
            .catch((e) => {
                // 加载失败不挡新对话，但必须上 banner —— 静默置空会让"有历史但
                // 后端抽风"看起来跟"确无历史"一模一样，用户以为记录丢了。
                console.error("[ai] list conversations:", e);
                if (seq === convSeq && !session) {
                    conversations = [];
                    banner = errMsg(e);
                }
            });
    });

    $effect(() => {
        items.length;
        if (chatBoxEl) {
            queueMicrotask(() => { chatBoxEl!.scrollTop = chatBoxEl!.scrollHeight; });
        }
    });

    /** 单飞 guard：onMount 预热 + send() 都会调 ensureSession，并发时两次都看不到
     *  session 存在（store 写入是 startSession 完成后才落），双 startSession 后端会
     *  报 session_already_exists。promise 复用：第二个调用方等同一个 promise 完成。 */
    let ensureInFlight: Promise<void> | null = null;

    function liveTargetId(): string {
        if (!targetId) throw new Error(t("common.disconnected"));
        return targetId;
    }

    /** start/resume 期间若连接重建，TerminalPane 可能在 actor 尚未落 store 时跳过
     *  rebind；启动完成后在这里补一次，保证 actor 绑定当前连接而不是死句柄。 */
    async function rebindIfNeeded(owner: PanelOwner, startedTargetId: string) {
        const latestTargetId = liveTargetId();
        if (latestTargetId !== startedTargetId) {
            await ai.rebindTarget(owner.tabId, owner.targetKind, latestTargetId, owner.lease);
        }
    }

    /** 没 session 就先启动；启动失败抛错。
     *  skill 固定 general —— 用户自定义 skill 已自动拼进 master prompt，让 LLM 自己路由。
     *  远端 shell 探测不在这里 —— 它在 SSH 连接时已跑过并写进 profile 缓存，
     *  startSession 从缓存读初始 shell（缓存 miss 则 POSIX 兜底）。 */
    async function ensureSession(owner: PanelOwner): Promise<void> {
        if (ai.sessionForTab(owner.tabId)) return;
        if (ensureInFlight) return ensureInFlight;
        ensureInFlight = (async () => {
            const settings = ai.settings() ?? await ai.loadSettings();
            if (!settings.has_api_key) {
                throw new Error(t("ai.error.no_api_key"));
            }
            // targetId 是连接句柄：同一 tab 重连后应使用此刻的最新值，不能在点击时
            // 冻结旧句柄；tabId / targetKind 才是这次动作不可变的 owner。
            const startedTargetId = liveTargetId();
            await ai.startSession({
                tabId: owner.tabId,
                targetKind: owner.targetKind,
                targetId: startedTargetId,
                skill: "general",
                provider: settings.provider, model: settings.model,
                lease: owner.lease,
            });
            await rebindIfNeeded(owner, startedTargetId);
        })();
        try {
            await ensureInFlight;
        } finally {
            ensureInFlight = null;
        }
    }

    // 同一行的 resume / delete 互斥：删除进行中点恢复同一行会产生可避免的
    // not_found 报错。按行互斥（不全局禁）—— 删 A 的几十毫秒里恢复 B 是合法操作。
    let deletingId = $state<string | null>(null);

    /** 点历史对话：actor 带旧 history 出生，UI 灌回存储的 timeline，直接可续聊。 */
    async function resumeConversation(id: string) {
        const owner = snapshotOwner();
        if (busy || session || deletingId === id) return;
        banner = null;
        busy = true;
        try {
            const settings = ai.settings() ?? await ai.loadSettings();
            if (!settings.has_api_key) {
                throw new Error(t("ai.error.no_api_key"));
            }
            const startedTargetId = liveTargetId();
            await ai.resumeSession({
                tabId: owner.tabId,
                targetKind: owner.targetKind,
                targetId: startedTargetId,
                skill: "general",
                provider: settings.provider, model: settings.model,
                lease: owner.lease,
            }, id);
            await rebindIfNeeded(owner, startedTargetId);
        } catch (e: any) {
            console.error("[ai] resume failed:", e);
            banner = errMsg(e);
        } finally {
            busy = false;
        }
    }

    async function deleteConversation(id: string) {
        if (busy || deletingId) return;
        deletingId = id;
        try {
            await ai.deleteConversation(id);
            conversations = (conversations ?? []).filter((c) => c.id !== id);
        } catch (e) {
            console.error("[ai] delete conversation:", e);
            banner = errMsg(e);
        } finally {
            deletingId = null;
        }
    }

    function fmtDate(ms: number) {
        return new Date(ms).toLocaleString();
    }

    async function send() {
        const owner = snapshotOwner();
        const text = inputText.trim();
        if (!text || busy) return;
        banner = null;
        busy = true;
        try {
            await ensureSession(owner);
            inputText = "";
            await ai.sendMessage(owner.tabId, text, owner.lease);
        } catch (e: any) {
            console.error("[ai] send failed:", e);
            banner = errMsg(e);
        } finally {
            busy = false;
        }
    }

    /** 显式关面板 = 结束并归档当前会话；重开回到首次打开状态。 */
    function closePanel() {
        void ai.closePanel(tabId).catch((e) => {
            console.warn("[ai] close panel session:", e);
            toast.error(errMsg(e));
        });
    }

    /** 点扫帚按钮：开二次确认模态。actor 不在就不弹（清个空气没意义）。 */
    function openClearDialog() {
        if (!session || rollbackDialog || rollingBack) return;
        clearDialogOwner = snapshotOwner();
        showClearDialog = true;
    }

    function closeClearDialog() {
        showClearDialog = false;
        clearDialogOwner = null;
    }

    /** 用户在模态里点"清空"：actor 不死，只把 history 清空 —— 下条消息从头来过。
     *  若正在流式响应，先把流停掉，避免 in-flight delta 落到已清空的气泡数组。 */
    async function clearContext() {
        const owner = clearDialogOwner;
        const wasStreaming = owner ? ai.isStreaming(owner.tabId) : false;
        closeClearDialog();
        if (!owner || !ai.sessionForTab(owner.tabId)) return;
        try {
            if (wasStreaming) {
                await ai.cancelStream(owner.tabId, owner.lease);
            }
            await ai.clearContext(owner.tabId, owner.lease);
        } catch (e) {
            console.error("[ai] clear context:", e);
            banner = errMsg(e);
        }
    }

    /** 打断当前流式响应；会话上下文保留，用户可立刻发下一条纠正。 */
    async function stopStreaming() {
        const owner = snapshotOwner();
        if (!ai.sessionForTab(owner.tabId)) return;
        try {
            await ai.cancelStream(owner.tabId, owner.lease);
        } catch (e) {
            // 不能只 console.error 就完事——失败的话用户还卡在 streaming/disabled 状态，
            // 看不到任何错误反馈。复用 banner 让用户知道"停止没生效，再点一次或刷新"。
            console.error("[ai] cancel stream:", e);
            banner = errMsg(e);
        }
    }

    function onKeyDown(e: KeyboardEvent) {
        if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            send();
        }
    }

    function fmt(ts: number) {
        return new Date(ts).toLocaleTimeString();
    }

    async function copyUserMessage(text: string) {
        try {
            await writeClipboard(text);
        } catch (error) {
            toast.error(errMsg(error));
        }
    }

    function userMessageIndexAt(itemIndex: number): number {
        return items.slice(0, itemIndex).filter((item) => item.kind === "user").length;
    }

    function openRollbackDialog(itemIndex: number, text: string) {
        if (rollingBack || rollbackDialog || showClearDialog || !session) return;
        rollbackDialog = {
            owner: snapshotOwner(),
            instanceId: session.instance_id,
            userMessageIndex: userMessageIndexAt(itemIndex),
            text,
        };
    }

    function closeRollbackDialog() {
        rollbackDialog = null;
    }

    async function confirmRollback() {
        const target = rollbackDialog;
        closeRollbackDialog();
        if (
            !target
            || rollingBack
            || ai.sessionForTab(target.owner.tabId)?.instance_id !== target.instanceId
        ) return;
        rollingBack = true;
        try {
            if (ai.isStreaming(target.owner.tabId)) {
                await ai.cancelStream(target.owner.tabId, target.owner.lease);
            }
            await ai.rollbackContext(
                target.owner.tabId,
                target.userMessageIndex,
                target.text,
                target.owner.lease,
            );
        } catch (error) {
            console.error("[ai] rollback context:", error);
            toast.error(errMsg(error));
        } finally {
            rollingBack = false;
        }
    }

    // ─── Right-click context menu on AI output ────────────────────────────
    /** Active context menu. `msgId` is the assistant message id whose bubble the
     *  right click landed in (null when clicking empty chat padding / on a user
     *  msg); `selection` is the text currently selected in the document, used
     *  for the copy / send-to-terminal items. Looking up by stable message id
     *  (not array index) survives rollbacks that shift indices while the menu
     *  is open. */
    let chatCtxMenu = $state<{ x: number; y: number; msgId: string | null; selection: string } | null>(null);

    /** 展开的思考过程气泡：assistant item id → 展开。默认折叠，点击展开。
     *  思考链是只读展示，不进终端、不参与任何命令路由。 */
    let expandedReasoning = $state<Set<string>>(new Set());
    function toggleReasoning(id: string) {
        const next = new Set(expandedReasoning);
        if (next.has(id)) next.delete(id); else next.add(id);
        expandedReasoning = next;
    }
    function isReasoningExpanded(id: string): boolean {
        return expandedReasoning.has(id);
    }
    /** 思考链是否已完成：首个非空正文 chunk 到达（reasoningDone）即算完成，
     *  不需要等整轮输出结束；无思考链的 item 不会走到这里。 */
    function thinkingFinished(item: ChatItem): boolean {
        return item.kind === "assistant" && (item.reasoningDone === true || !item.streaming);
    }

    /** Close on outside click / Esc — BlockContextMenu already listens for this,
     *  so here we only reset our own state when it fires onClose. */
    function closeChatCtxMenu() { chatCtxMenu = null; }

    /** Right-click on the chat area: only pop the menu when the pointer is over
     *  an assistant bubble with a selection (raw markdown needs the bubble, copy
     *  needs the selection — "no selection, no menu" keeps the gesture safe). */
    function onChatContextMenu(e: MouseEvent) {
        const sel = window.getSelection()?.toString() ?? "";
        if (!sel) return;
        const target = e.target as Element | null;
        const bubble = target?.closest?.(".bubble.assistant.md");
        if (!bubble) return;
        const msgId = (bubble as HTMLElement).dataset.msgId ?? null;
        // The extracted plain-text selection is what copy / send-to-terminal use;
        // the raw markdown comes from the stored ChatItem via `msgId`.
        e.preventDefault();
        chatCtxMenu = { x: e.clientX, y: e.clientY, msgId, selection: sel.trim() };
    }

    function copyChatSelection() {
        const m = chatCtxMenu;
        if (!m || !m.selection) return;
        void writeClipboard(m.selection).catch((error) => toast.error(errMsg(error)));
    }

    /** Copy the original markdown source of the assistant message, not the
     *  rendered/selected plain text — users often want the raw code fences. */
    function copyChatMarkdown() {
        const m = chatCtxMenu;
        const item = m?.msgId != null ? items.find((it) => it.kind === "assistant" && it.id === m.msgId) : undefined;
        if (!m || !item || item.kind !== "assistant" || !item.text) return;
        void writeClipboard(item.text).catch((error) => toast.error(errMsg(error)));
    }

    /** Send the selected text to this panel's terminal (the one the AI is
     *  diagnosing — identified by `tabId`, not the globally active tab) as input
     *  (no Enter — the user reviews it in the terminal first, hence "send", not
     *  "run"). Uses the bracketed-paste path so multi-line blocks (heredocs etc.)
     *  are pasted as one unit and don't get a bash PS2 "> " continuation prompt
     *  per line. */
    function sendChatSelectionToTerminal() {
        const m = chatCtxMenu;
        if (!m || !m.selection || !targetId) return;
        try {
            app.terminalPaste(tabId, m.selection);
        } catch (error) {
            toast.error(errMsg(error));
        }
    }

    /** Send the selected text to this panel's composer (input box), ready for
     *  the user to review/edit before sending — the same prefill channel the
     *  terminal block "Send to AI" uses. */
    function sendChatSelectionToInput() {
        const m = chatCtxMenu;
        if (!m || !m.selection) return;
        ai.prefillInput(tabId, m.selection);
    }

    function chatCtxItems(): MenuItem[] {
        const m = chatCtxMenu;
        const itemMsgId = m?.msgId ?? null;
        const mdItem = itemMsgId != null ? items.find((it) => it.kind === "assistant" && it.id === itemMsgId) : undefined;
        const hasMarkdown = !!mdItem && mdItem.kind === "assistant" && !!mdItem.text;
        const canSendTerminal = !!targetId;
        return [
            {
                label: t("common.copy"),
                action: copyChatSelection,
                disabled: !m?.selection,
            },
            {
                label: t("ai.ctx.copy_markdown"),
                action: copyChatMarkdown,
                disabled: !hasMarkdown,
            },
            {
                label: t("ai.ctx.send_to_terminal"),
                action: sendChatSelectionToTerminal,
                disabled: !canSendTerminal,
            },
            {
                label: t("ai.ctx.send_to_input"),
                action: sendChatSelectionToInput,
                disabled: !m?.selection,
            },
        ];
    }

    // ─── Composer height (draggable) ─────────────────────────────────────
    const INPUT_HEIGHT_MIN = 48;
    const INPUT_HEIGHT_PANEL_FRAC = 0.6;

    let inputAreaEl = $state<HTMLDivElement | null>(null);

    /** Drag the handle above the composer to change its explicit height (the
     *  textarea height — not max-height — so the box visibly grows/shrinks).
     *  Mirrors the AI panel width gesture: track start position, live-update
     *  during move, persist once (global pref) on mouse-up. No Enter/newline
     *  involved — extra content scrolls inside the box. */
    function startInputResize(e: MouseEvent) {
        e.preventDefault();
        const startY = e.clientY;
        const startHeight = ai.inputHeight();
        // Cap by the panel height so the composer can never swallow the chat.
        let panelArea = (inputAreaEl?.parentElement?.getBoundingClientRect().height ?? 0);
        if (panelArea > 0) panelArea = Math.max(panelArea * INPUT_HEIGHT_PANEL_FRAC, INPUT_HEIGHT_MIN);
        let stopped = false;

        function stop() {
            if (stopped) return;
            stopped = true;
            document.removeEventListener("mousemove", onMove);
            document.removeEventListener("mouseup", stop);
            window.removeEventListener("blur", stop);
            // Persist once on mouse-up — onMove only touches $state, matching
            // the AI panel width gesture (setPanelWidth + commitPanelWidth).
            ai.commitInputHeight();
        }

        function onMove(ev: MouseEvent) {
            const dy = ev.clientY - startY;
            // Dragging up (negative dy) grows the composer.
            const next = startHeight - dy;
            const clamped = panelArea > 0
                ? Math.min(Math.max(next, INPUT_HEIGHT_MIN), panelArea)
                : Math.max(next, INPUT_HEIGHT_MIN);
            ai.setInputHeight(clamped);
        }

        document.addEventListener("mousemove", onMove);
        document.addEventListener("mouseup", stop);
        window.addEventListener("blur", stop);
    }

    /** Double-click the handle: reset to the default height (set + persist). */
    function resetInputHeight() {
        ai.resetInputHeight();
    }

    // ─── Composer right-click menu (copy / paste) ───────────────────────
    let inputCtxMenu = $state<{ x: number; y: number } | null>(null);

    function closeInputCtxMenu() { inputCtxMenu = null; }

    /** Right-click on the textarea: always offer the menu; Copy is greyed out
     *  when nothing is selected (same disabled styling as the terminal block
     *  menu via BlockContextMenu). */
    function onInputContextMenu(e: MouseEvent) {
        e.preventDefault();
        e.stopPropagation();
        inputCtxMenu = { x: e.clientX, y: e.clientY };
    }

    function selectedInputText(): string {
        const el = inputEl;
        if (!el) return "";
        return el.value.slice(el.selectionStart ?? 0, el.selectionEnd ?? 0);
    }

    function copyInputSelection() {
        const sel = selectedInputText();
        if (!sel) return;
        void writeClipboard(sel).catch((error) => toast.error(errMsg(error)));
    }

    async function pasteIntoInput() {
        let text: string;
        try {
            text = await readClipboard();
        } catch (error) {
            toast.error(errMsg(error));
            return;
        }
        const el = inputEl;
        if (!el) return;
        const start = el.selectionStart ?? inputText.length;
        const end = el.selectionEnd ?? inputText.length;
        inputText = inputText.slice(0, start) + text + inputText.slice(end);
        // Restore the caret just past the pasted text.
        requestAnimationFrame(() => {
            const pos = start + text.length;
            try { el.setSelectionRange(pos, pos); } catch { /* no-op */ }
            el.focus();
        });
    }

    function inputCtxItems(): MenuItem[] {
        return [
            {
                label: t("common.copy"),
                action: copyInputSelection,
                disabled: !selectedInputText(),
            },
            {
                label: t("common.paste"),
                action: pasteIntoInput,
            },
        ];
    }
</script>

<div class="ai-panel">
    <div class="toolbar">
        <!-- Current model: left-aligned, single line, ellipsis on overflow (full
             text on hover). Also the flex spring (flex:1) that pushes the controls
             to the right — replaces the old empty .grow spacer. -->
        <span class="model" title={currentModel}>{currentModel}</span>
        <span class="tokens" title={t("ai.toolbar.tokens_tip", { tin: tokens.tokens_in, tout: tokens.tokens_out })}>
            ↑{formatTokenCount(tokens.tokens_in)} ↓{formatTokenCount(tokens.tokens_out)}
        </span>
        <!-- Audit log toggle: file-text icon in chat view, chat bubble in audit view (= go back).
             Toolbar controls render unconditionally (stable layout); they disable until the
             session lazy-starts on first send — no actor, nothing to audit or clear. -->
        <button class="btn-icon" onclick={() => (auditOpen = !auditOpen)} disabled={!session}
                title={auditOpen ? t("ai.toolbar.back_to_chat") : t("ai.toolbar.audit")}
                aria-label={auditOpen ? t("ai.toolbar.back_to_chat") : t("ai.toolbar.audit")}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                {#if auditOpen}
                    <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>
                {:else}
                    <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/>
                    <polyline points="14 2 14 8 20 8"/>
                    <line x1="16" y1="13" x2="8" y2="13"/>
                    <line x1="16" y1="17" x2="8" y2="17"/>
                    <polyline points="10 9 8 9"/>
                {/if}
            </svg>
        </button>
        <!-- 清理上下文：SVG 扫帚图标（22×22）跟"×"视觉重心对齐。 -->
        <button class="btn-icon" onclick={openClearDialog} disabled={!session} title={t("ai.toolbar.clear_context")} aria-label={t("ai.toolbar.clear_context")}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M20 4 L13 11"/>
                <path d="M11 9 L15 13"/>
                <path d="M11 9 L5 15"/>
                <path d="M12.33 10.33 L7 17"/>
                <path d="M13.67 11.67 L9 18.5"/>
                <path d="M15 13 L11 19.5"/>
            </svg>
        </button>
        <!-- Danger-mode toggle: always visible, selected (red) when ON. The toggle
             logic + confirm modal live in DangerModeToggle (shared with AiSettings —
             one safety contract); here we only render the icon. No disabled={!session}
             — danger_mode is a global setting, settable before the session starts. -->
        <DangerModeToggle {active} onError={(m) => (banner = m)}>
            {#snippet trigger(requestToggle, saving)}
                <button class="btn-icon danger-toggle" class:on={dangerMode}
                        onclick={requestToggle} disabled={saving}
                        title={dangerMode ? t("ai.title.danger_tip") : t("ai.toolbar.danger_enable")}
                        aria-label={t("ai.toolbar.danger_aria")} aria-pressed={dangerMode}>
                    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/>
                        <line x1="12" y1="9" x2="12" y2="13"/>
                        <line x1="12" y1="17" x2="12.01" y2="17"/>
                    </svg>
                </button>
            {/snippet}
        </DangerModeToggle>
        <!-- 显示思考过程：全局开关（localStorage 持久化）。默认为关——不渲染
             思考链；开启后按当前实现展示（默认折叠，点击逐条展开）。灯泡图标
             与其余图标同风格，开启时 accent 高亮（区别于危险模式的红色）。 -->
        <button class="btn-icon reasoning-toggle" class:on={ai.showReasoning()}
                onclick={() => ai.setShowReasoning(!ai.showReasoning())}
                title={ai.showReasoning() ? t("ai.toolbar.reasoning_hide") : t("ai.toolbar.reasoning_show")}
                aria-label={ai.showReasoning() ? t("ai.toolbar.reasoning_hide") : t("ai.toolbar.reasoning_show")}
                aria-pressed={ai.showReasoning()}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <!-- feather-style lightbulb: bulb + filament + rays -->
                <path d="M9 18h6"/>
                <path d="M10 22h4"/>
                <path d="M12 2a7 7 0 0 0-4.9 12c.6.6 1 1.4 1.3 2.2A2 2 0 0 0 10.3 18h3.4a2 2 0 0 0 1.9-1.8c.3-.8.7-1.6 1.3-2.2A7 7 0 0 0 12 2z"/>
            </svg>
        </button>
        <button class="btn-icon" onclick={closePanel} title={t("ai.toolbar.close_session")} aria-label={t("ai.toolbar.close_session")}>×</button>
    </div>

    {#if banner}
        <div class="banner">
            <span>{banner}</span>
            <button class="btn-icon" onclick={() => (banner = null)}>×</button>
        </div>
    {/if}

    {#if auditOpen && session}
        <AuditPanel {tabId} />
    {:else}
        <div class="chat" bind:this={chatBoxEl} oncontextmenu={app.isMobile ? undefined : onChatContextMenu}>
            {#each items as item, i (i)}
                <div class="item item-{item.kind}">
                    {#if item.kind === "user"}
                        <div class="ts">{fmt(item.at)}</div>
                        <div class="user-message">
                            <div class="message-actions">
                                <button class="message-action" onclick={() => copyUserMessage(item.text)}
                                        title={t("ai.message.copy")} aria-label={t("ai.message.copy")}>
                                    <svg aria-hidden="true" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                        <rect x="9" y="9" width="13" height="13" rx="2"/>
                                        <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>
                                    </svg>
                                </button>
                                <button class="message-action rollback" onclick={() => openRollbackDialog(i, item.text)}
                                        disabled={rollingBack} title={t("ai.message.rollback")}
                                        aria-label={t("ai.message.rollback")}>
                                    <svg aria-hidden="true" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                        <path d="M9 14 4 9l5-5"/>
                                        <path d="M4 9h10a6 6 0 0 1 6 6v5"/>
                                    </svg>
                                </button>
                            </div>
                            <div class="bubble user">{item.text}</div>
                        </div>
                    {:else if item.kind === "assistant"}
                        <div class="ts">{fmt(item.at)}</div>
                        {#if item.reasoning && ai.showReasoning()}
                            <!-- 思考链折叠块：只看 UI 层，绝不进入终端。折叠时显示
                                 "> 思考中…"（思考进行中）或 "> 思考过程"（思考完成）；
                                 点击展开。思考完成的信号是首个非空正文 chunk 到达，
                                 而不是整轮输出结束。仅当工具栏的"显示思考过程"开关
                                 开启时渲染。 -->
                            <button class="thinking" class:open={isReasoningExpanded(item.id)}
                                    onclick={() => toggleReasoning(item.id)}
                                    aria-expanded={isReasoningExpanded(item.id)}
                                    title={thinkingFinished(item)
                                        ? t("ai.bubble.thinking_tip_done")
                                        : t("ai.bubble.thinking_tip_streaming")}>
                                <span class="thinking-icon" class:down={isReasoningExpanded(item.id)}>&gt;</span>
                                <span class="thinking-label">{thinkingFinished(item)
                                    ? t("ai.bubble.thinking_done")
                                    : t("ai.bubble.thinking_streaming")}</span>
                            </button>
                            {#if isReasoningExpanded(item.id)}
                                <div class="thinking-body">{item.reasoning}</div>
                            {/if}
                        {/if}
                        <!-- eslint-disable-next-line svelte/no-at-html-tags -->
                        <div class="bubble assistant md" data-msg-id={item.id} class:streaming={item.streaming} class:cancelled={item.cancelled}>
                            {#if item.text}
                                {@html renderMarkdown(item.text)}
                            {:else if !item.cancelled}
                                …
                            {/if}
                            {#if item.cancelled}
                                <span class="cancelled-tag">{t("ai.bubble.cancelled")}</span>
                            {/if}
                        </div>
                    {:else if item.kind === "command" && session}
                        {#key item.cmd.id}
                            <CommandConfirmDialog
                                {tabId}
                                instanceId={session.instance_id}
                                targetKind={targetKind}
                                targetSessionId={targetId}
                                cmd={item.cmd}
                                result={item.result}
                                rejected={item.rejected}
                                {active}
                            />
                        {/key}
                    {:else if item.kind === "error"}
                        <div class="bubble error">{item.text}</div>
                    {:else if item.kind === "note"}
                        <div class="bubble note">{item.text}</div>
                    {/if}
                </div>
            {/each}
            {#if items.length === 0 && !session}
                <div class="placeholder dim">
                    <p>{t("ai.placeholder.welcome")}</p>
                    <p class="hint">{t("ai.placeholder.example_hint")}</p>
                    <p class="hint">{t("ai.placeholder.confirm_hint")}</p>
                </div>
                {#if conversations && conversations.length > 0}
                    <div class="history">
                        <div class="history-title">{t("ai.history.title")}</div>
                        {#each conversations as c (c.id)}
                            <div class="history-row">
                                <button class="history-item" onclick={() => resumeConversation(c.id)}
                                        disabled={busy || deletingId === c.id} title={t("ai.history.resume_tip")}>
                                    <span class="history-name">{c.title || t("ai.history.untitled")}</span>
                                    <span class="history-time">{fmtDate(c.updated_at)}</span>
                                </button>
                                <!-- 删除全局互斥（deletingId 只能追踪一个 in-flight），禁用范围
                                     必须跟守卫一致：删除进行中所有删除按钮都禁，恢复按钮仍按行。 -->
                                <button class="btn-icon history-del" onclick={() => deleteConversation(c.id)}
                                        disabled={busy || deletingId !== null}
                                        title={t("ai.history.delete")} aria-label={t("ai.history.delete")}>×</button>
                            </div>
                        {/each}
                    </div>
                {/if}
            {/if}
        </div>

        {#if chatCtxMenu}
            <BlockContextMenu x={chatCtxMenu.x} y={chatCtxMenu.y} items={chatCtxItems()} onClose={closeChatCtxMenu} />
        {/if}

        <div class="input-area" bind:this={inputAreaEl}>
            {#if !app.isMobile}
                <div class="input-resize-handle"
                     onmousedown={startInputResize}
                     ondblclick={resetInputHeight}
                     role="separator"
                     aria-orientation="horizontal"
                     title={t("common.resize_hint")}></div>
            {/if}
            <textarea
                bind:this={inputEl}
                bind:value={inputText}
                style="height: {ai.inputHeight()}px; min-height: 48px;"
                placeholder={busy ? (session ? t("ai.input.replying") : t("ai.input.starting")) : (streaming ? t("ai.input.replying") : t("ai.input.placeholder"))}
                onkeydown={onKeyDown}
                oncontextmenu={app.isMobile ? undefined : onInputContextMenu}
                disabled={busy}
                readonly={streaming}
            ></textarea>
            {#if inputCtxMenu}
                <BlockContextMenu x={inputCtxMenu.x} y={inputCtxMenu.y} items={inputCtxItems()} onClose={closeInputCtxMenu} />
            {/if}
            {#if streaming}
                <button class="btn btn-sm btn-stop" onclick={stopStreaming} title={t("ai.input.stop")}>
                    {t("ai.input.stop")}
                </button>
            {:else}
                <button class="btn btn-sm btn-primary" onclick={send} disabled={!inputText.trim() || busy}>
                    {busy && !session ? t("ai.input.starting_short") : t("ai.input.send")}
                </button>
            {/if}
        </div>
    {/if}
</div>

<!-- Clear-context confirmation. Tauri webview drops native confirm() silently,
     so we use the same custom modal pattern as AiSettings' danger-mode dialog. -->
{#if showClearDialog}
    <Modal onClose={closeClearDialog} class="stack"
           aria-labelledby="clear-dialog-title" aria-describedby="clear-dialog-body">
        <h3 id="clear-dialog-title" class="dialog-title">{t("ai.toolbar.clear_confirm_title")}</h3>
        <div id="clear-dialog-body" class="dialog-body">{t("ai.toolbar.clear_confirm")}</div>
        <div class="modal-actions">
            <button class="btn btn-sm" onclick={closeClearDialog}>
                {t("common.cancel")}
            </button>
            <button class="btn btn-sm btn-primary" onclick={clearContext}>
                {t("ai.toolbar.clear_confirm_action")}
            </button>
        </div>
    </Modal>
{/if}

{#if rollbackDialog}
    <Modal onClose={closeRollbackDialog} class="stack"
           aria-labelledby="rollback-dialog-title" aria-describedby="rollback-dialog-body">
        <h3 id="rollback-dialog-title" class="dialog-title">{t("ai.message.rollback_confirm_title")}</h3>
        <div id="rollback-dialog-body" class="dialog-body">{t("ai.message.rollback_confirm")}</div>
        <div class="modal-actions">
            <button class="btn btn-sm" onclick={closeRollbackDialog}>{t("common.cancel")}</button>
            <button class="btn btn-sm btn-danger" onclick={confirmRollback}>
                {t("ai.message.rollback_confirm_action")}
            </button>
        </div>
    </Modal>
{/if}

<style>
    .ai-panel {
        display: flex;
        flex-direction: column;
        height: 100%;
        background: var(--bg);
        border-left: 1px solid var(--divider);
        border-right: 1px solid var(--divider);
    }
    .toolbar {
        display: flex; align-items: center; gap: 8px;
        padding: 8px; border-bottom: 1px solid var(--divider);
        flex-shrink: 0;
    }
    .model {
        flex: 1;
        min-width: 0;
        font-size: 11px;
        font-family: monospace;
        color: var(--text-dim);
        white-space: nowrap;
        overflow: hidden;
        text-overflow: ellipsis;
    }
    .tokens {
        font-size: 10.5px;
        font-family: monospace;
        color: var(--text-dim);
        white-space: nowrap;
        flex-shrink: 0;
    }
    .btn-primary { background: var(--accent); color: var(--white); border-color: var(--accent); }
    .btn-primary:disabled { opacity: 0.5; cursor: not-allowed; }
    /* Never let the composer's growing textarea stretch or squeeze the button. */
    .btn-primary, .btn-stop { flex-shrink: 0; }
    .btn-stop {
        background: var(--error);
        color: var(--white);
        border-color: var(--error);
        cursor: pointer;
    }
    .btn-stop:hover { opacity: 0.85; }
    .btn-ghost { background: transparent; }
    .btn-icon {
        background: transparent; border: none;
        font-size: 18px; cursor: pointer;
        color: var(--text); padding: 4px 6px;
        display: inline-flex; align-items: center; justify-content: center;
        line-height: 1;
        border-radius: 4px;
    }
    .btn-icon:hover {
        background: color-mix(in srgb, var(--text) 8%, transparent);
        color: var(--text);
    }
    .btn-icon:disabled {
        opacity: 0.35;
        cursor: default;
    }
    .btn-icon:disabled:hover { background: transparent; }
    /* Danger-mode toggle, selected state: red icon + red-tinted fill so it reads
       as "on" among the otherwise-neutral toolbar icons. The :hover rule keeps it
       red (overriding .btn-icon:hover's neutral color via higher specificity). */
    .danger-toggle.on {
        color: var(--error);
        background: color-mix(in srgb, var(--error) 14%, transparent);
    }
    .danger-toggle.on:hover {
        color: var(--error);
        background: color-mix(in srgb, var(--error) 22%, transparent);
    }
    /* 显示思考过程开关：开启态 accent 高亮（与危险模式的红色区分开）。 */
    .reasoning-toggle.on {
        color: var(--accent);
        background: color-mix(in srgb, var(--accent) 14%, transparent);
    }
    .reasoning-toggle.on:hover {
        color: var(--accent);
        background: color-mix(in srgb, var(--accent) 22%, transparent);
    }
    .banner {
        display: flex; align-items: center; gap: 8px;
        padding: 8px 12px;
        background: color-mix(in srgb, var(--error) 18%, var(--bg));
        color: var(--error);
        border-bottom: 1px solid var(--divider);
        font-size: 12px;
        flex-shrink: 0;
    }
    .banner span { flex: 1; word-break: break-word; }

    .placeholder {
        padding: 24px; text-align: center;
        color: var(--text-dim);
        line-height: 1.6;
    }
    .placeholder.dim { font-size: 13px; padding: 32px 32px 8px; }
    .hint { font-size: 12px; }

    /* 历史对话 picker —— 仅空状态（无会话）时出现在欢迎语下方。 */
    .history { padding: 0 16px; display: flex; flex-direction: column; gap: 2px; }
    .history-title {
        font-size: 11px; font-weight: 600; color: var(--text-dim);
        text-transform: uppercase; letter-spacing: 0.05em;
        margin: 8px 0 4px;
    }
    .history-row { display: flex; align-items: center; gap: 2px; }
    .history-item {
        flex: 1; min-width: 0;
        display: flex; align-items: baseline; gap: 8px;
        padding: 5px 8px;
        background: transparent; border: none; cursor: pointer;
        border-radius: 4px; color: var(--text);
        text-align: left; font-size: 12.5px;
    }
    .history-item:hover { background: color-mix(in srgb, var(--text) 8%, transparent); }
    .history-item:disabled { opacity: 0.5; cursor: default; }
    .history-name {
        flex: 1; min-width: 0;
        overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
    }
    .history-time {
        font-size: 10.5px; color: var(--text-dim);
        font-family: monospace; flex-shrink: 0;
    }
    .history-del { font-size: 14px; padding: 2px 5px; color: var(--text-dim); }

    .chat {
        flex: 1; overflow-y: auto; padding: 6px;
        display: flex; flex-direction: column; gap: 3px;
    }
    .item { display: flex; flex-direction: column; gap: 1px; }
    .user-message {
        display: flex; align-items: center; justify-content: flex-end; gap: 4px;
    }
    .message-actions {
        display: flex; gap: 1px;
        opacity: 0; pointer-events: none;
        transition: opacity 120ms ease;
    }
    .item-user:hover .message-actions,
    .item-user:focus-within .message-actions {
        opacity: 1; pointer-events: auto;
    }
    .message-action {
        width: 24px; height: 24px; padding: 0;
        display: inline-flex; align-items: center; justify-content: center;
        flex-shrink: 0;
        border: 0; border-radius: 4px; background: transparent;
        color: var(--text-dim); cursor: pointer;
    }
    .message-action:hover {
        color: var(--text);
        background: color-mix(in srgb, var(--text) 8%, transparent);
    }
    .message-action.rollback:hover { color: var(--error); }
    .message-action:disabled { opacity: 0.4; cursor: default; }
    @media (hover: none), (any-pointer: coarse) {
        .user-message {
            flex-direction: column;
            align-items: flex-end;
        }
        .message-actions { opacity: 1; pointer-events: auto; }
        .message-action { width: 44px; height: 44px; }
        .message-actions { order: 2; }
        .bubble.user { order: 1; }
    }
    .ts {
        font-size: 10px; color: var(--text-dim);
        font-family: monospace;
    }
    /* ── 思考链折叠块 ──
       只渲染于 UI 层，绝不进入终端。折叠:一行弱化小字"> 思考中…/思考过程"；
       展开:等宽小字 + 弱化背景，明显区别于 markdown 正文。 */
    .thinking {
        align-self: flex-start;
        display: inline-flex; align-items: center; gap: 5px;
        margin: 2px 0 2px;
        padding: 2px 8px;
        border: none; border-radius: 4px;
        background: color-mix(in srgb, var(--text-dim) 10%, transparent);
        color: var(--text-dim);
        font-family: inherit; font-size: 12px;
        cursor: pointer;
        max-width: 100%;
    }
    .thinking:hover {
        background: color-mix(in srgb, var(--text-dim) 18%, transparent);
        color: var(--text);
    }
    .thinking .thinking-icon {
        font-family: monospace; font-weight: 600;
        /* 折叠时指向右（>），展开后旋转 90° 指向下（v）作为开合指示。 */
        display: inline-block;
        transition: transform 120ms ease;
    }
    .thinking .thinking-icon.down {
        transform: rotate(90deg);
    }
    .thinking-label {
        white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
    }
    .thinking-body {
        align-self: flex-start;
        max-width: 95%;
        width: 100%;
        margin: 2px 0 4px;
        padding: 6px 9px;
        border-left: 2px solid var(--divider);
        background: color-mix(in srgb, var(--text-dim) 6%, transparent);
        color: var(--text-sub);
        font-family: monospace; font-size: 11.5px;
        line-height: 1.4;
        white-space: pre-wrap; word-break: break-word;
        border-radius: 0 4px 4px 0;
    }
    .bubble {
        padding: 5px 9px; border-radius: 6px;
        max-width: 95%; word-break: break-word; white-space: pre-wrap;
        font-size: 13px;
    }
    .bubble.user {
        background: var(--accent); color: var(--white);
    }
    .bubble.assistant {
        background: color-mix(in srgb, var(--text) 8%, var(--bg));
        align-self: flex-start;
    }
    .bubble.assistant.streaming {
        position: relative;
    }
    /* 用户打断的响应：气泡尾部跟一个本地化小徽章，区别于"AI 自己结束的对话"。
       徽章本身在 ChatPanel 模板里用 i18n 渲染，避免把英文 marker 硬塞进 LLM 输出文本。 */
    .cancelled-tag {
        display: inline-block;
        margin-left: 6px;
        padding: 1px 6px;
        border-radius: 3px;
        background: color-mix(in srgb, var(--text-dim) 18%, transparent);
        color: var(--text-dim);
        font-size: 10.5px;
        font-weight: 500;
        vertical-align: middle;
    }
    .bubble.assistant.streaming::after {
        content: "▋";
        display: inline-block;
        margin-left: 2px;
        animation: blink 1s steps(2, start) infinite;
        color: var(--text-dim);
    }
    @keyframes blink {
        to { visibility: hidden; }
    }
    /* Markdown 内容样式 — 极致紧凑 */
    /* 关键：覆盖 .bubble 默认的 pre-wrap。marked 输出的 HTML 标签间有 source-only `\n`，
       pre-wrap 会把那些 `\n` 渲染成可见空行——经典 bug，markdown 气泡必须用 normal。 */
    .bubble.md { line-height: 1.32; font-size: 12.5px; white-space: normal; }
    .bubble.md :global(> *:first-child) { margin-top: 0; }
    .bubble.md :global(> *:last-child) { margin-bottom: 0; }
    .bubble.md :global(p) { margin: 0; }
    .bubble.md :global(p + p) { margin-top: 0; }
    .bubble.md :global(br) { line-height: 1; }
    .bubble.md :global(code) {
        background: color-mix(in srgb, var(--text) 12%, transparent);
        padding: 0 3px; border-radius: 2px;
        font-family: monospace; font-size: 11.5px;
    }
    .bubble.md :global(pre) {
        background: color-mix(in srgb, var(--text) 8%, var(--bg));
        padding: 4px 6px; border-radius: 3px;
        overflow-x: auto; font-size: 11.5px;
        margin: 2px 0; line-height: 1.3;
    }
    .bubble.md :global(pre code) { background: transparent; padding: 0; font-size: inherit; }
    .bubble.md :global(ul), .bubble.md :global(ol) { margin: 1px 0; padding-left: 16px; }
    .bubble.md :global(li) { margin: 0; }
    .bubble.md :global(li > p) { margin: 0; }
    .bubble.md :global(li > ul), .bubble.md :global(li > ol) { margin: 0; }
    .bubble.md :global(strong) { font-weight: 600; }
    .bubble.md :global(em) { font-style: italic; }
    .bubble.md :global(a) { color: var(--accent); }
    .bubble.md :global(h1),
    .bubble.md :global(h2),
    .bubble.md :global(h3),
    .bubble.md :global(h4) {
        margin: 3px 0 1px; font-weight: 600; line-height: 1.2;
    }
    .bubble.md :global(:first-child:is(h1, h2, h3, h4)) { margin-top: 0; }
    .bubble.md :global(h1) { font-size: 14px; }
    .bubble.md :global(h2) { font-size: 13px; }
    .bubble.md :global(h3), .bubble.md :global(h4) { font-size: 12.5px; }
    .bubble.md :global(blockquote) {
        border-left: 2px solid var(--divider);
        padding-left: 5px; margin: 1px 0;
        color: var(--text-dim);
    }
    .bubble.md :global(hr) {
        border: 0; border-top: 1px solid var(--divider);
        margin: 3px 0;
    }
    .bubble.md :global(table) {
        border-collapse: collapse; margin: 2px 0; font-size: 11.5px;
    }
    .bubble.md :global(th), .bubble.md :global(td) {
        border: 1px solid var(--divider); padding: 1px 5px;
    }
    .bubble.error {
        background: color-mix(in srgb, var(--error) 15%, var(--bg));
        color: var(--error);
        font-size: 12px;
    }
    .bubble.note {
        background: transparent;
        color: var(--text-dim);
        font-size: 12px;
        font-style: italic;
        align-self: center;
    }

    .input-area {
        position: relative;
        display: flex; align-items: flex-end; gap: 8px; padding: 8px;
        border-top: 1px solid var(--divider);
        flex-shrink: 0;
    }
    /* Draggable handle above the composer: pulls its max-height up/down.
       Styled like the AI panel width grip (accent on hover) but horizontal.
       top:0 (not -3px) keeps the hit region inside .input-area so it can't
       intercept text selection at the bottom of the chat scroll area. */
    .input-resize-handle {
        position: absolute;
        left: 0; right: 0; top: 0;
        height: 6px;
        cursor: row-resize;
        z-index: 10;
        background: transparent;
        transition: background 0.12s ease;
    }
    .input-resize-handle:hover,
    .input-resize-handle:active {
        background: var(--accent);
        opacity: 0.45;
    }
    textarea {
        flex: 1; min-height: 48px; resize: none;
        padding: 6px 8px; border: 1px solid var(--divider);
        border-radius: 4px; background: var(--bg); color: var(--text);
        font-family: inherit; font-size: 13px;
        box-sizing: border-box;
        /* The inline height from ai.inputHeight() drives the box height and
           the resize handle; any extra content scrolls inside the box. */
        overflow-y: auto;
    }

    /* Clear-context confirmation modal — shell lives in Modal.svelte, typography
       in global .dialog-title/.dialog-body; only the multi-line body is local. */
    .dialog-body {
        white-space: pre-line;
    }
</style>
