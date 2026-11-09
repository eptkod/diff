<script lang="ts">
  import { api, CLIP_LENGTHS, errorText, formatHotkey, formatLength, hotkeyFromEvent, isMac, QUALITIES, type Settings } from "$lib/api";
  import SourcePicker from "$lib/SourcePicker.svelte";

  let {
    settings,
    following = null,
    onsaved,
    full = true,
  }: {
    settings: Settings;
    following?: string | null;
    onsaved: (settings: Settings) => void;
    full?: boolean;
  } = $props();

  let error = $state("");
  let capturingHotkey = $state(false);

  const bitrate = $derived(QUALITIES.find((q) => q.id === settings.quality)?.bitrate ?? 0);
  const memoryMb = $derived(Math.round(((bitrate / 8) * settings.clipSeconds) / (1024 * 1024)));

  async function update(change: Partial<Settings>) {
    try {
      const state = await api.saveSettings({ ...settings, ...change });
      error = "";
      onsaved(state.settings);
    } catch (failure) {
      error = errorText(failure);
    }
  }

  async function chooseFolder() {
    const folder = await api.chooseFolder();
    if (folder) await update({ saveFolder: folder });
  }

  function captureHotkey(event: KeyboardEvent) {
    if (!capturingHotkey) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.key === "Escape") {
      capturingHotkey = false;
      return;
    }
    const hotkey = hotkeyFromEvent(event);
    if (hotkey) {
      capturingHotkey = false;
      update({ hotkey });
    }
  }
</script>

<svelte:window onkeydown={captureHotkey} />

<SourcePicker value={settings.source} {following} onchange={(source) => update({ source })} />

<div class="field">
  <span class="label">Clip length</span>
  <div class="segments">
    {#each CLIP_LENGTHS as seconds}
      <button aria-pressed={settings.clipSeconds === seconds} onclick={() => update({ clipSeconds: seconds })}>
        {seconds < 60 ? `${seconds}s` : `${seconds / 60}m`}
      </button>
    {/each}
  </div>
  <p class="hint">Keeps the last {formatLength(settings.clipSeconds)}, using about {memoryMb} MB of memory.</p>
</div>

<div class="field">
  <span class="label">Clip shortcut</span>
  <button onclick={() => (capturingHotkey = !capturingHotkey)} aria-pressed={capturingHotkey}>
    {capturingHotkey ? "Press your shortcut… (Esc to cancel)" : formatHotkey(settings.hotkey)}
  </button>
</div>

<label class="toggle">
  <span>
    <span class="label-text">Voice clipping</span>
    <span class="hint block">Say “diff clip that”. Listening happens only on this computer.</span>
  </span>
  <input
    type="checkbox"
    checked={settings.voiceEnabled}
    onchange={(e) => update({ voiceEnabled: (e.target as HTMLInputElement).checked })}
  />
</label>

<label class="toggle">
  <span>
    <span class="label-text">Include my microphone in clips</span>
    <span class="hint block">Your computer's sound is always included.</span>
  </span>
  <input
    type="checkbox"
    checked={settings.includeMic}
    onchange={(e) => update({ includeMic: (e.target as HTMLInputElement).checked })}
  />
</label>

{#if full}
  <div class="field">
    <span class="label">Quality</span>
    <div class="segments">
      {#each QUALITIES as quality}
        <button aria-pressed={settings.quality === quality.id} onclick={() => update({ quality: quality.id })}>
          {quality.label}
        </button>
      {/each}
    </div>
    <p class="hint">{QUALITIES.find((q) => q.id === settings.quality)?.detail}</p>
  </div>

  <div class="field">
    <span class="label">Save clips to</span>
    <div class="row">
      <span class="grow truncate hint" title={settings.saveFolder}>{settings.saveFolder}</span>
      <button onclick={chooseFolder}>Change…</button>
    </div>
  </div>

  <label class="toggle">
    <span class="label-text">Start diff when I log in{isMac() ? "" : " to Windows"}</span>
    <input
      type="checkbox"
      checked={settings.startAtLogin}
      onchange={(e) => update({ startAtLogin: (e.target as HTMLInputElement).checked })}
    />
  </label>
{/if}

{#if error}<p class="error">{error}</p>{/if}

<style>
  .label-text {
    font-weight: 600;
  }

  .block {
    display: block;
  }
</style>
