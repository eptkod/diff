<script lang="ts">
  import { api, clipTitle, errorText, formatDuration, mediaUrl, type Clip } from "$lib/api";
  import ConfirmDelete from "$lib/ConfirmDelete.svelte";

  let { clip, onclose, onchanged, onstar }: {
    clip: Clip;
    onclose: () => void;
    onchanged: (path: string | null) => void;
    onstar: () => Promise<void>;
  } = $props();

  const MIN_TRIM_SECONDS = 1;

  let confirmDelete = $state<ConfirmDelete>();

  let video = $state<HTMLVideoElement | null>(null);
  let start = $state(0);
  let end = $state<number | null>(null);
  let renaming = $state(false);
  let draftName = $state("");
  let busy = $state(false);
  let error = $state("");
  let message = $state("");

  const duration = $derived(clip.durationSeconds);
  const trimEnd = $derived(end ?? duration);
  const isTrimmed = $derived(start > 0 || trimEnd < duration);

  function setStart(event: Event) {
    start = Math.min(Number((event.target as HTMLInputElement).value), trimEnd - MIN_TRIM_SECONDS);
    (event.target as HTMLInputElement).value = String(start);
    if (video) video.currentTime = start;
  }

  function setEnd(event: Event) {
    end = Math.max(Number((event.target as HTMLInputElement).value), start + MIN_TRIM_SECONDS);
    (event.target as HTMLInputElement).value = String(end);
    if (video) video.currentTime = end;
  }

  async function run(action: () => Promise<void>) {
    busy = true;
    error = "";
    message = "";
    try {
      await action();
    } catch (failure) {
      error = errorText(failure);
    } finally {
      busy = false;
    }
  }

  const saveTrim = () =>
    run(async () => {
      const saved = await api.trimClip(clip.path, start, trimEnd);
      message = `Saved as ${saved.split(/[\\/]/).pop()}`;
      onchanged(clip.path);
    });

  function beginRename() {
    draftName = clipTitle(clip) === clip.name ? clip.name : "";
    renaming = true;
  }

  const commitRename = () =>
    run(async () => {
      if (!draftName.trim()) {
        renaming = false;
        return;
      }
      const path = await api.renameClip(clip.path, draftName);
      renaming = false;
      onchanged(path);
    });

  const remove = () =>
    run(async () => {
      await api.deleteClip(clip.path);
      onchanged(null);
    });
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && !renaming && !confirmDelete?.isOpen() && onclose()} />

<main class="player">
  <header class="row">
    <button onclick={onclose}>← Clips</button>
    {#if renaming}
      <!-- svelte-ignore a11y_autofocus -->
      <input
        class="grow"
        type="text"
        bind:value={draftName}
        placeholder="Name this clip"
        autofocus
        onkeydown={(e) => {
          e.stopPropagation();
          if (e.key === "Enter") commitRename();
          if (e.key === "Escape") renaming = false;
        }}
      />
      <button class="primary" onclick={commitRename} disabled={busy}>Save</button>
    {:else}
      <h1 class="grow truncate">{clipTitle(clip)}</h1>
      <button
        class:starred={clip.starred}
        aria-pressed={clip.starred}
        onclick={() => onstar().catch((failure) => (error = errorText(failure)))}
      >
        {clip.starred ? "★ Starred" : "☆ Star"}
      </button>
      <button onclick={beginRename}>Rename</button>
      <button onclick={() => run(() => api.revealClip(clip.path))}>Show file</button>
      <button class="danger" onclick={() => confirmDelete?.open()} disabled={busy}>Delete</button>
    {/if}
  </header>

  <!-- svelte-ignore a11y_media_has_caption -->
  <video bind:this={video} src={mediaUrl(clip.path)} controls autoplay></video>

  {#if duration >= MIN_TRIM_SECONDS * 2}
    <section class="trim">
      <div class="track">
        <div
          class="selection"
          style="left:{(start / duration) * 100}%;right:{100 - (trimEnd / duration) * 100}%"
        ></div>
        <input type="range" aria-label="Trim start" min="0" max={duration} step="0.1" value={start} oninput={setStart} />
        <input type="range" aria-label="Trim end" min="0" max={duration} step="0.1" value={trimEnd} oninput={setEnd} />
      </div>
      <div class="row spread">
        <span class="hint">
          {formatDuration(start)} to {formatDuration(trimEnd)}. The start moves to the nearest second.
        </span>
        <button class="primary" onclick={saveTrim} disabled={busy || !isTrimmed}>
          {busy ? "Saving…" : "Save trimmed copy"}
        </button>
      </div>
    </section>
  {/if}

  {#if error}<p class="error">{error}</p>{/if}
  {#if message}<p class="hint">{message}</p>{/if}

  <ConfirmDelete bind:this={confirmDelete} count={1} onconfirm={remove} />
</main>

<style>
  .starred {
    color: var(--star);
  }

  .player {
    display: flex;
    flex-direction: column;
    gap: 14px;
    height: 100vh;
    padding: 16px 20px;
  }

  video {
    flex: 1;
    min-height: 0;
    width: 100%;
    border-radius: 8px;
    background: #000;
  }

  .trim {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .track {
    position: relative;
    height: 24px;
    border-radius: 6px;
    background: var(--surface-2);
  }

  .selection {
    position: absolute;
    top: 0;
    bottom: 0;
    border-radius: 6px;
    background: color-mix(in srgb, var(--accent) 40%, transparent);
  }

  input[type="range"] {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    margin: 0;
    background: transparent;
    pointer-events: none;
    appearance: none;
    -webkit-appearance: none;
  }

  input[type="range"]::-webkit-slider-thumb {
    width: 10px;
    height: 24px;
    border-radius: 4px;
    background: var(--accent);
    pointer-events: auto;
    appearance: none;
    -webkit-appearance: none;
  }
</style>
