<script lang="ts">
  import type { HighlightRule } from "../stores/app.svelte.ts";
  import { t } from "../i18n/index.svelte.ts";
  import { validateHighlightRule } from "../terminal/highlight.ts";

  let {
    rule,
    onSave,
    onCancel,
  }: {
    rule: HighlightRule;
    onSave: (rule: HighlightRule) => void;
    onCancel: () => void;
  } = $props();

  let formKw = $state("");
  let formName = $state("");
  let formColor = $state("#FF6B6B");
  let formEnabled = $state(true);
  let formIsRegex = $state(false);
  let formIsCaseSensitive = $state(false);

  function loadFromRule(r: HighlightRule) {
    formKw = r.keyword ?? "";
    formName = r.name ?? "";
    formColor = r.color || "#FF6B6B";
    formEnabled = r.enabled ?? true;
    formIsRegex = r.is_regex ?? false;
    formIsCaseSensitive = r.is_case_sensitive ?? false;
  }

  $effect(() => {
    loadFromRule(rule);
  });

  const finalRule = $derived<HighlightRule>({
    keyword: formKw.trim(),
    name: formName.trim(),
    color: formColor,
    enabled: formEnabled,
    is_regex: formIsRegex,
    is_case_sensitive: formIsCaseSensitive,
  });

  const formError = $derived(validateHighlightRule(finalRule));

  function handleSave() {
    if (!finalRule.keyword || formError) return;
    onSave(finalRule);
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "Enter") handleSave();
  }
</script>

<div class="card inline-form">
  {#if formIsRegex}
    <label>
      <span class="label-text">{t("highlight.name")}</span>
      <input type="text" bind:value={formName} placeholder={t("highlight.name_placeholder")}
        onkeydown={handleKeydown} />
    </label>
  {/if}

  <label>
    <span class="label-text">{t("highlight.keyword")}</span>
    <input type="text" bind:value={formKw} placeholder={t("highlight.keyword_placeholder")}
      onkeydown={handleKeydown} />
  </label>

  <div class="option-row">
    <label class="color-picker">
      <span class="label-text">{t("common.color")}</span>
      <div class="color-row">
        <input type="color" bind:value={formColor} />
        <span class="color-hex">{formColor}</span>
      </div>
    </label>

    <div class="toggles">
      <label class="toggle-btn" class:active={formIsRegex}>
        <input type="checkbox" bind:checked={formIsRegex} />
        <span>{t("highlight.regex")}</span>
      </label>
      <label class="toggle-btn" class:active={formIsCaseSensitive}>
        <input type="checkbox" bind:checked={formIsCaseSensitive} />
        <span>{t("highlight.case_sensitive")}</span>
      </label>
    </div>

    <div class="form-actions">
      <button class="btn btn-accent btn-sm" onclick={handleSave} disabled={!formKw.trim() || !!formError}>
        {t("common.save")}
      </button>
      <button class="btn btn-sm" onclick={onCancel}>{t("common.cancel")}</button>
    </div>
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
</div>

<style>
  .inline-form {
    display: flex; flex-direction: column; gap: 10px;
    padding: 16px; margin-bottom: 12px;
  }
  .inline-form label { display: flex; flex-direction: column; gap: 4px; }
  .label-text { font-size: 12px; color: var(--text-sub); }
  .inline-form input[type="text"] {
    width: 100%; box-sizing: border-box; font: inherit; font-size: 13px;
  }

  .option-row {
    display: flex; align-items: flex-end; gap: 16px; flex-wrap: wrap;
  }
  .color-picker { flex-shrink: 0; }
  .color-row { display: flex; align-items: center; gap: 10px; }
  .color-row input[type="color"] {
    width: 48px; height: 32px; padding: 2px;
    border: 1px solid var(--divider); border-radius: 4px;
    cursor: pointer; box-shadow: none;
  }
  .color-hex { font-size: 12px; font-family: monospace; color: var(--text-dim); }

  .toggles { display: flex; align-items: center; gap: 8px; flex: 1; }
  .toggle-btn {
    display: inline-flex; align-items: center; gap: 4px;
    padding: 5px 12px; font-size: 12px; font-weight: 600;
    border-radius: var(--radius-sm, 4px);
    background: var(--surface); color: var(--text-sub);
    border: 1.5px solid var(--divider);
    cursor: pointer; user-select: none;
    transition: all 0.15s;
  }
  .toggle-btn input {
    position: absolute; opacity: 0; pointer-events: none; width: 0; height: 0;
  }
  .toggle-btn.active {
    background: var(--accent); border-color: var(--accent); color: var(--white);
    box-shadow: 0 0 8px color-mix(in srgb, var(--accent) 40%, transparent);
  }
  .toggle-btn:not(.active):hover {
    border-color: var(--accent); color: var(--text);
  }

  .form-actions {
    display: flex; gap: 10px; margin-left: auto; align-items: center;
  }

  .form-error {
    font-size: 12px; color: var(--error, #ff6b6b);
    background: rgba(255, 107, 107, 0.08);
    padding: 6px 10px; border-radius: 4px;
  }
</style>
