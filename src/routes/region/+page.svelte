<script lang="ts">
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onDestroy, onMount } from "svelte";
  import { api, errorText } from "$lib/api";

  const MIN_SIZE = 32;
  const displayId = Number(new URLSearchParams(location.search).get("display"));

  let anchor = $state<{ x: number; y: number } | null>(null);
  let point = $state({ x: 0, y: 0 });
  let error = $state("");

  const box = $derived(
    anchor && {
      left: Math.min(anchor.x, point.x),
      top: Math.min(anchor.y, point.y),
      width: Math.abs(point.x - anchor.x),
      height: Math.abs(point.y - anchor.y),
    },
  );

  const cancel = () => getCurrentWindow().close();

  onMount(() => document.documentElement.classList.add("transparent"));
  onDestroy(() => document.documentElement.classList.remove("transparent"));

  function start(event: PointerEvent) {
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    anchor = { x: event.clientX, y: event.clientY };
    point = { x: event.clientX, y: event.clientY };
    error = "";
  }

  function move(event: PointerEvent) {
    if (anchor) point = { x: event.clientX, y: event.clientY };
  }

  async function finish() {
    const selected = box;
    anchor = null;
    if (!selected) return;
    if (selected.width < MIN_SIZE || selected.height < MIN_SIZE) {
      error = "Drag a larger area.";
      return;
    }
    try {
      await api.setRegion(displayId, selected.left, selected.top, selected.width, selected.height);
    } catch (failure) {
      error = errorText(failure);
    }
  }
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && cancel()} />

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="overlay" onpointerdown={start} onpointermove={move} onpointerup={finish}>
  {#if box}
    <div
      class="box"
      style="left:{box.left}px;top:{box.top}px;width:{box.width}px;height:{box.height}px"
    >
      <span>{Math.round(box.width)} × {Math.round(box.height)}</span>
    </div>
  {:else}
    <p class="instructions">{error || "Drag to select the area to record. Press Esc to cancel."}</p>
  {/if}
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    cursor: crosshair;
    background: rgb(0 0 0 / 0.35);
  }

  .instructions {
    position: absolute;
    top: 40%;
    left: 50%;
    transform: translateX(-50%);
    padding: 10px 16px;
    border-radius: 8px;
    background: rgb(0 0 0 / 0.75);
    color: #fff;
  }

  .box {
    position: absolute;
    border: 2px solid #36c47f;
    box-shadow: 0 0 0 100vmax rgb(0 0 0 / 0.0);
    background: rgb(255 255 255 / 0.12);
  }

  .box span {
    position: absolute;
    top: 6px;
    left: 6px;
    padding: 2px 6px;
    border-radius: 4px;
    background: rgb(0 0 0 / 0.75);
    color: #fff;
    font-size: 12px;
  }
</style>
