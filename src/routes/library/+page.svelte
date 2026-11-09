<script lang="ts">
  import { onMount } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import { Menu } from "@tauri-apps/api/menu";
  import { api, clipTitle, errorText, formatHotkey, onClipsChanged, onSettingsChanged, revealLabel, type AppState, type Clip } from "$lib/api";
  import ClipPlayer from "$lib/ClipPlayer.svelte";
  import ClipTile from "$lib/ClipTile.svelte";
  import ConfirmDelete from "$lib/ConfirmDelete.svelte";
  import { searchBy } from "$lib/search";
  import { DATE_OPTIONS, LENGTH_OPTIONS, NO_FILTERS, clipMatches, isFiltering } from "$lib/filters";

  let app = $state<AppState | null>(null);
  let clips = $state<Clip[]>([]);
  let openPath = $state<string | null>(null);
  let error = $state("");
  let filters = $state({ ...NO_FILTERS });

  let selecting = $state(false);
  let busy = $state(false);
  const selected = new SvelteSet<string>();
  let confirmDelete = $state<ConfirmDelete>();

  const openClip = $derived(clips.find((clip) => clip.path === openPath) ?? null);
  const shownClips = $derived(
    searchBy(clips.filter((clip) => clipMatches(clip, filters, Date.now())), filters.query, clipTitle),
  );
  // Read through `shownClips` so a clip that has gone, or that the filters hide, doesn't count.
  const selectedClips = $derived(shownClips.filter((clip) => selected.has(clip.path)));
  const selectedPaths = $derived(selectedClips.map((clip) => clip.path));
  const allStarred = $derived(selectedClips.length > 0 && selectedClips.every((clip) => clip.starred));
  const allSelected = $derived(shownClips.length > 0 && selectedPaths.length === shownClips.length);

  const statusText = $derived.by(() => {
    switch (app?.status.state) {
      case "recording":
        return "Recording";
      case "paused":
        return "Paused";
      case "needsPermission":
        return "Needs permission";
      case "error":
        return "Recording failed";
      default:
        return "Not recording";
    }
  });

  async function loadClips() {
    try {
      clips = await api.listClips();
      error = "";
    } catch (failure) {
      error = errorText(failure);
    }
  }

  const loadState = async () => {
    app = await api.getState();
  };

  onMount(() => {
    loadClips();
    loadState();
    const stops = [onClipsChanged(loadClips), onSettingsChanged(loadState)];
    return () => {
      stops.forEach((stop) => stop.then((unlisten) => unlisten()));
    };
  });

  function changed(path: string | null) {
    openPath = path;
    loadClips();
  }

  function stopSelecting() {
    selecting = false;
    selected.clear();
  }

  function toggle(path: string) {
    if (!selected.delete(path)) selected.add(path);
  }

  function toggleAll() {
    const wasAll = allSelected;
    selected.clear();
    if (!wasAll) shownClips.forEach((clip) => selected.add(clip.path));
  }

  async function toggleStar(clip: Clip) {
    const starred = !clip.starred;
    clips = clips.map((other) => (other.path === clip.path ? { ...other, starred } : other));
    try {
      await api.setClipStarred(clip.path, starred);
    } catch (failure) {
      await loadClips();
      throw failure;
    }
  }

  const starFromTile = (clip: Clip) => toggleStar(clip).catch((failure) => (error = errorText(failure)));

  async function showClipMenu(event: MouseEvent, clip: Clip) {
    event.preventDefault();
    try {
      const menu = await Menu.new({
        items: [
          {
            text: revealLabel(),
            action: () => api.revealClip(clip.path).catch((failure) => (error = errorText(failure))),
          },
        ],
      });
      await menu.popup();
    } catch (failure) {
      error = errorText(failure);
    }
  }

  const deleteSelected = () => applyToSelected(() => api.deleteClips(selectedPaths));
  const starSelected = () => applyToSelected(() => api.setClipsStarred(selectedPaths, !allStarred));

  async function applyToSelected(action: () => Promise<void>) {
    busy = true;
    error = "";
    try {
      await action();
      stopSelecting();
    } catch (failure) {
      error = errorText(failure);
    } finally {
      busy = false;
    }
  }
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && selecting && !confirmDelete?.isOpen() && stopSelecting()} />

{#if openClip}
  {#key openClip.path}
    <ClipPlayer
      clip={openClip}
      onclose={() => (openPath = null)}
      onchanged={changed}
      onstar={() => toggleStar(openClip)}
    />
  {/key}
{:else}
  <main class="page">
    <header class="row spread">
      <div class="row">
        <h1>Clips</h1>
        <span class="status" class:live={app?.status.state === "recording"}>{statusText}</span>
      </div>
      <div class="row">
        {#if selecting}
          <span class="hint">{selectedPaths.length} selected</span>
          <button onclick={toggleAll}>{allSelected ? "Select none" : "Select all"}</button>
          <button onclick={starSelected} disabled={busy || selectedPaths.length === 0}>{allStarred ? "Unstar" : "Star"}</button>
          <button class="danger" onclick={() => confirmDelete?.open()} disabled={busy || selectedPaths.length === 0}>Delete</button>
          <button onclick={stopSelecting}>Cancel</button>
        {:else}
          {#if clips.length > 0}<button onclick={() => (selecting = true)}>Select</button>{/if}
          <button class="primary" onclick={api.clipNow}>Clip now</button>
          <button onclick={api.openSettings}>Settings</button>
        {/if}
      </div>
    </header>

    {#if error}<p class="error">{error}</p>{/if}

    {#if clips.length > 0}
      <div class="filters">
        <input class="search" type="text" bind:value={filters.query} placeholder="Search by name" aria-label="Search by name" />
        <button
          class="starred"
          aria-pressed={filters.starred}
          onclick={() => (filters.starred = !filters.starred)}
        >
          <span class="star-mark">{filters.starred ? "★" : "☆"}</span> Starred
        </button>
        <select bind:value={filters.date} aria-label="Filter by date">
          {#each DATE_OPTIONS as option (option.id)}
            <option value={option.id}>{option.label}</option>
          {/each}
        </select>
        <select bind:value={filters.length} aria-label="Filter by length">
          {#each LENGTH_OPTIONS as option (option.id)}
            <option value={option.id}>{option.label}</option>
          {/each}
        </select>
        {#if isFiltering(filters)}
          <button onclick={() => (filters = { ...NO_FILTERS })}>Clear</button>
        {/if}
      </div>

      {#if shownClips.length > 0}
        <div class="grid">
          {#each shownClips as clip (clip.path)}
            <ClipTile
              {clip}
              {selecting}
              selected={selected.has(clip.path)}
              onclick={() => (selecting ? toggle(clip.path) : (openPath = clip.path))}
              onstar={() => starFromTile(clip)}
              oncontextmenu={(event) => showClipMenu(event, clip)}
            />
          {/each}
        </div>
      {:else}
        <div class="empty">
          <p><strong>No clips match these filters</strong></p>
        </div>
      {/if}
    {:else if app}
      <div class="empty">
        <p><strong>No clips yet</strong></p>
        <p class="hint">Press {formatHotkey(app.settings.hotkey)} or say “diff clip that” to save your first one.</p>
      </div>
    {/if}

    <ConfirmDelete bind:this={confirmDelete} count={selectedPaths.length} onconfirm={deleteSelected} />
  </main>
{/if}

<style>
  .status {
    padding: 2px 8px;
    border-radius: 999px;
    background: var(--surface-2);
    color: var(--muted);
    font-size: 12px;
  }

  .status.live {
    background: color-mix(in srgb, var(--accent) 18%, transparent);
    color: var(--accent);
    font-weight: 600;
  }

  .filters {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }

  .search {
    flex: 1;
    min-width: 160px;
  }

  .starred[aria-pressed="true"] {
    background: var(--surface-2);
    font-weight: 600;
  }

  .star-mark {
    color: var(--star);
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(210px, 1fr));
    gap: 16px;
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
    padding: 96px 0;
    text-align: center;
  }
</style>
