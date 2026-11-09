<script lang="ts">
  import { onMount } from "svelte";
  import { api, errorText, type ListedSource, type Source, type SourceInfo } from "$lib/api";

  let {
    value,
    following = null,
    onchange,
  }: { value: Source | null; following?: string | null; onchange: (source: Source | null) => void } =
    $props();

  type Kind = ListedSource["kind"];

  const AUTOMATIC = "automatic";
  const ACTIVE_WINDOW: Source = { kind: "activeWindow" };

  let sources = $state<SourceInfo[]>([]);
  let error = $state("");
  let chosenKind = $state<Kind | null>(null);
  let regionDisplay = $state<number | null>(null);

  const automatic = $derived(value?.kind === "activeWindow");
  const kind = $derived<Kind>(
    chosenKind ?? (value?.kind === "activeWindow" ? "window" : value?.kind) ?? "display",
  );
  const displays = $derived(sources.filter((s) => s.source.kind === "display"));
  const windows = $derived(sources.filter((s) => s.source.kind === "window"));

  const idOf = (source: ListedSource) => (source.kind === "region" ? source.display_id : source.id);
  const selectedId = $derived(value && value.kind === kind ? idOf(value) : null);
  const regionDisplayId = $derived(
    regionDisplay ?? (value?.kind === "region" ? value.display_id : null) ??
      (displays[0] ? idOf(displays[0].source) : null),
  );

  async function refresh() {
    try {
      sources = await api.listSources();
      error = "";
    } catch (failure) {
      error = errorText(failure);
    }
  }

  onMount(refresh);

  function chooseKind(next: Kind) {
    chosenKind = next;
    refresh();
    if (next === "display" && displays[0]) onchange(displays[0].source);
    if (next === "window" && !automatic && value?.kind !== "window") onchange(ACTIVE_WINDOW);
  }

  function chooseSource(event: Event) {
    const choice = (event.target as HTMLSelectElement).value;
    if (choice === AUTOMATIC) {
      onchange(ACTIVE_WINDOW);
      return;
    }
    const id = Number(choice);
    const match = sources.find((s) => s.source.kind === kind && idOf(s.source) === id);
    if (match) onchange(match.source);
  }

  async function drawRegion() {
    if (regionDisplayId === null) return;
    try {
      await api.openRegionPicker(regionDisplayId);
    } catch (failure) {
      error = errorText(failure);
    }
  }
</script>

<div class="field">
  <span class="label">What to record</span>
  <div class="segments">
    <button aria-pressed={kind === "display"} onclick={() => chooseKind("display")}>Display</button>
    <button aria-pressed={kind === "window"} onclick={() => chooseKind("window")}>Window</button>
    <button aria-pressed={kind === "region"} onclick={() => chooseKind("region")}>Part of screen</button>
  </div>

  {#if kind === "display"}
    <select value={selectedId ?? (displays[0] ? idOf(displays[0].source) : undefined)} onchange={chooseSource}>
      {#each displays as display (idOf(display.source))}
        <option value={idOf(display.source)}>
          {display.name} ({Math.round(display.width)} × {Math.round(display.height)})
        </option>
      {/each}
    </select>
  {:else if kind === "window"}
    <select value={selectedId ?? AUTOMATIC} onchange={chooseSource}>
      <option value={AUTOMATIC}>Automatic: the game or window you're using</option>
      <optgroup label="Always record">
        {#each windows as window (idOf(window.source))}
          <option value={idOf(window.source)}>{window.name}</option>
        {/each}
      </optgroup>
    </select>
    {#if automatic}
      {#if following}
        <p class="hint truncate" title={following}>Recording now: <strong>{following}</strong></p>
      {/if}
      <p class="hint">
        Games are picked up on their own and stay recorded until you close them, even if you tab out
        to a browser. Otherwise diff moves to whichever window you settle on, and each move starts a
        fresh recording.
      </p>
    {:else}
      <p class="hint">Open the app first if it isn't listed.</p>
    {/if}
  {:else}
    <div class="row">
      {#if displays.length > 1}
        <select
          class="grow"
          value={regionDisplayId}
          onchange={(e) => (regionDisplay = Number((e.target as HTMLSelectElement).value))}
        >
          {#each displays as display (idOf(display.source))}
            <option value={idOf(display.source)}>{display.name}</option>
          {/each}
        </select>
      {/if}
      <button class="grow" onclick={drawRegion} disabled={regionDisplayId === null}>
        {value?.kind === "region" ? "Redraw area" : "Draw area on screen"}
      </button>
    </div>
    {#if value?.kind === "region"}
      <p class="hint">Recording a {Math.round(value.width)} × {Math.round(value.height)} area.</p>
    {/if}
  {/if}

  {#if error}<p class="error">{error}</p>{/if}
</div>
