<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import type { HighlightRule } from "../stores/app.svelte.ts";
  import * as app from "../stores/app.svelte.ts";
  import { toast } from "../stores/toast.svelte.ts";
  import { t, errMsg } from "../i18n/index.svelte.ts";
  import { validateHighlightRule } from "../terminal/highlight.ts";

  let items = $state<HighlightRule[]>([]);
  let adding = $state(false);
  // Edit identity = keyword as currently stored on the backend (rename uses old → new).
  let editKw = $state<string | null>(null);

  // Form fields
  let formKw = $state("");
  let formColor = $state("#FF6B6B");
  let formEnabled = $state(true);
  let formIsRegex = $state(false);
  let formIsCaseSensitive = $state(false);

  const formRule = $derived<HighlightRule>({
    keyword: formKw.trim(),
    color: formColor,
    enabled: formEnabled,
    is_regex: formIsRegex,
    is_case_sensitive: formIsCaseSensitive,
  });
  const formError = $derived(validateHighlightRule(formRule));

  onMount(refresh);

  async function refresh() {
    items = await app.loadHighlights();
    // Tell open TerminalPanes their highlight regex is stale. Local-only
    // bump (no backend round-trip) — TerminalPane's $effect re-reads the
    // DB and recompiles its regex. Without this, edits here only take
    // effect after the next terminal reconnect.
    app.bumpHighlights();
  }

  function startAdd() {
    adding = true;
    editKw = null;
    formKw = "";
    formColor = "#FF6B6B";
    formEnabled = true;
    formIsRegex = false;
    formIsCaseSensitive = false;
  }

  function startEdit(h: HighlightRule) {
    adding = false;
    editKw = h.keyword;
    formKw = h.keyword;
    formColor = h.color;
    formEnabled = h.enabled;
    formIsRegex = h.is_regex;
    formIsCaseSensitive = h.is_case_sensitive;
  }

  function cancelForm() {
    adding = false;
    editKw = null;
  }

  function showFormValidationError() {
    if (!formError) return;
    if (formError.kind === "zero_width") {
      toast.error(t("error.highlight_regex_zero_width"));
    } else {
      toast.error(t("error.highlight_invalid_regex", { err: formError.message }));
    }
  }

  async function saveNew() {
    if (!formRule.keyword) return;
    if (formError) { showFormValidationError(); return; }
    try {
      await invoke("add_highlight", { rule: formRule });
      adding = false;
      await refresh();
    } catch (e: any) { toast.error(`${t("toast.error.add")}: ${errMsg(e)}`); }
  }

  async function saveEdit() {
    if (editKw === null) return;
    if (!formRule.keyword) return;
    if (formError) { showFormValidationError(); return; }
    try {
      await invoke("update_highlight", {
        oldKeyword: editKw,
        rule: formRule,
      });
      editKw = null;
      await refresh();
    } catch (e: any) { toast.error(`${t("toast.error.save")}: ${errMsg(e)}`); }
  }

  async function remove(keyword: string) {
    try {
      await invoke("remove_highlight", { keyword });
      if (editKw === keyword) editKw = null;
      await refresh();
    } catch (e: any) { toast.error(`${t("toast.error.delete")}: ${errMsg(e)}`); }
  }

  async function resetDefaults() {
    try {
      await invoke("reset_highlights");
      cancelForm();
      await refresh();
    } catch (e: any) { toast.error(`${t("toast.error.reset")}: ${errMsg(e)}`); }
  }
</script>

<div class="page">
  <div class="toolbar">
    <button class="btn btn-sm" onclick={resetDefaults}>{t("highlight.reset_defaults")}</button>
    <button class="btn btn-accent btn-sm" onclick={startAdd}>{t("highlight.new")}</button>
  </div>

  {#if adding}
    <div class="card inline-form">
      <label>
        <span class="label-text">{t("highlight.keyword")}</span>
        <input type="text" bind:value={formKw} placeholder={t("highlight.keyword_placeholder")}
          onkeydown={(e) => { if (e.key === "Enter") saveNew(); }} />
      </label>
      <label>
        <span class="label-text">{t("common.color")}</span>
        <div class="color-row">
          <input type="color" bind:value={formColor} />
          <span class="color-hex">{formColor}</span>
        </div>
      </label>
      <div class="form-row">
        <label class="switch-label">
          <input type="checkbox" bind:checked={formIsRegex} />
          <span>{t("highlight.regex")}</span>
        </label>
        <label class="switch-label">
          <input type="checkbox" bind:checked={formIsCaseSensitive} />
          <span>{t("highlight.case_sensitive")}</span>
        </label>
      </div>
      {#if formError}
        <div class="form-error">
          {#if formError.kind === "zero_width"}
            {t("error.highlight_regex_zero_width")}
          {:else}
            {t("error.highlight_invalid_regex", { err: formError.message })}
          {/if}
        </div>
      {/if}
      <div class="form-actions">
        <button class="btn btn-accent btn-sm" onclick={saveNew} disabled={!formKw.trim() || !!formError}>{t("common.save")}</button>
        <button class="btn btn-sm" onclick={cancelForm}>{t("common.cancel")}</button>
      </div>
    </div>
  {/if}

  {#each items as h (h.keyword)}
    {#if editKw === h.keyword}
      <div class="card inline-form">
        <label>
          <span class="label-text">{t("highlight.keyword")}</span>
          <input type="text" bind:value={formKw}
            onkeydown={(e) => { if (e.key === "Enter") saveEdit(); }} />
        </label>
      <label>
        <span class="label-text">{t("common.color")}</span>
        <div class="color-row">
          <input type="color" bind:value={formColor} />
          <span class="color-hex">{formColor}</span>
        </div>
      </label>
      <div class="form-row">
        <label class="switch-label">
          <input type="checkbox" bind:checked={formIsRegex} />
          <span>{t("highlight.regex")}</span>
        </label>
        <label class="switch-label">
          <input type="checkbox" bind:checked={formIsCaseSensitive} />
          <span>{t("highlight.case_sensitive")}</span>
        </label>
      </div>
      {#if formError}
        <div class="form-error">
          {#if formError.kind === "zero_width"}
            {t("error.highlight_regex_zero_width")}
          {:else}
            {t("error.highlight_invalid_regex", { err: formError.message })}
          {/if}
        </div>
      {/if}
      <div class="form-actions">
        <button class="btn btn-accent btn-sm" onclick={saveEdit} disabled={!formKw.trim() || !!formError}>{t("common.save")}</button>
        <button class="btn btn-sm" onclick={cancelForm}>{t("common.cancel")}</button>
      </div>
    </div>
    {:else}
      <div class="card item-row">
        <div class="item-info">
          <span class="color-swatch" style="background: {h.color}"></span>
          <div>
            <div class="item-name">{h.keyword}</div>
            <div class="item-sub">{h.color}</div>
            {#if h.is_regex || h.is_case_sensitive}
              <div class="item-tags">
                {#if h.is_regex}<span class="tag">RegEx</span>{/if}
                {#if h.is_case_sensitive}<span class="tag">Aa</span>{/if}
              </div>
            {/if}
          </div>
        </div>
        <div class="item-actions">
          <button class="btn btn-sm" onclick={() => startEdit(h)}>{t("common.edit")}</button>
          <button class="btn btn-sm btn-danger" onclick={() => remove(h.keyword)}>{t("common.delete")}</button>
        </div>
      </div>
    {/if}
  {:else}
    {#if !adding}
      <p class="empty">{t("highlight.empty")}</p>
    {/if}
  {/each}
</div>

<style>
  .page { padding: 24px; }
  .toolbar { display: flex; justify-content: flex-end; gap: 8px; margin-bottom: 16px; }
  .item-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 16px;
    gap: 12px;
  }
  .item-info { display: flex; align-items: center; gap: 10px; min-width: 0; flex: 1; }
  .item-name { font-weight: 600; font-size: 14px; font-family: monospace; }
  .item-sub { font-size: 12px; color: var(--text-sub); font-family: monospace; }
  .item-actions { display: flex; gap: 10px; flex-shrink: 0; }
  .color-swatch {
    width: 20px; height: 20px; border-radius: 4px; flex-shrink: 0;
    border: 1px solid var(--divider);
  }

  .inline-form {
    display: flex; flex-direction: column; gap: 8px;
    padding: 14px; margin-bottom: 10px;
  }
  .inline-form label { display: flex; flex-direction: column; gap: 4px; }
  .label-text { font-size: 13px; color: var(--text); }
  .inline-form input[type="text"] {
    width: 100%; box-sizing: border-box; font: inherit; font-size: 13px;
  }
  .color-row { display: flex; align-items: center; gap: 10px; }
  .color-row input[type="color"] {
    width: 48px; height: 32px; padding: 2px;
    border: 1px solid var(--divider); border-radius: 4px;
    cursor: pointer; box-shadow: none;
  }
  .color-hex { font-size: 12px; font-family: monospace; color: var(--text-dim); }
  .form-actions { display: flex; gap: 10px; margin-top: 4px; }

  .form-row { display: flex; gap: 16px; align-items: center; margin-top: 4px; }
  .switch-label { display: flex; align-items: center; gap: 6px; font-size: 13px; cursor: pointer; }
  .switch-label input { margin: 0; }
  .form-error {
    font-size: 12px; color: var(--error, #ff6b6b);
    background: rgba(255, 107, 107, 0.08);
    padding: 6px 8px; border-radius: 4px;
  }

  .item-tags { display: flex; gap: 6px; margin-top: 4px; }
  .tag {
    font-size: 10px; font-weight: 600; color: var(--text-dim);
    border: 1px solid var(--divider); border-radius: 3px;
    padding: 1px 4px; font-family: monospace;
  }

  .empty { text-align: center; color: var(--text-dim); padding: 32px; }
</style>
