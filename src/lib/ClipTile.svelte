<script lang="ts">
  import { api, clipTitle, formatDate, formatDuration, formatSize, mediaUrl, type Clip } from "$lib/api";

  let { clip, selecting = false, selected = false, onclick, onstar, oncontextmenu }: {
    clip: Clip;
    selecting?: boolean;
    selected?: boolean;
    onclick: () => void;
    onstar: () => void;
    oncontextmenu: (event: MouseEvent) => void;
  } = $props();

  const THUMBNAIL_WIDTH = 480;
  const JPEG_QUALITY = 0.72;

  let generated = $state<string | null>(null);
  let failed = $state(false);

  const thumbnail = $derived(clip.thumbnail ?? generated);

  /** Draws one frame from a hidden video and caches it as a small JPEG. */
  function capture(event: Event) {
    const video = event.target as HTMLVideoElement;
    try {
      const canvas = document.createElement("canvas");
      canvas.width = THUMBNAIL_WIDTH;
      canvas.height = Math.round((THUMBNAIL_WIDTH * video.videoHeight) / video.videoWidth);
      canvas.getContext("2d")?.drawImage(video, 0, 0, canvas.width, canvas.height);
      canvas.toBlob(
        async (blob) => {
          if (!blob) return;
          const bytes = Array.from(new Uint8Array(await blob.arrayBuffer()));
          generated = await api.saveThumbnail(clip.path, clip.modifiedMs, bytes);
        },
        "image/jpeg",
        JPEG_QUALITY,
      );
    } catch {
      failed = true;
    }
  }
</script>

<div class="clip">
  <button class="tile" class:selected {onclick} {oncontextmenu} aria-pressed={selecting ? selected : undefined}>
    <div class="thumb">
      {#if selecting}<span class="check">{selected ? "✓" : ""}</span>{/if}
      {#if thumbnail}
        <img src={mediaUrl(thumbnail)} alt="" draggable="false" />
      {:else if !failed}
        <video
          src={`${mediaUrl(clip.path)}#t=0.5`}
          crossorigin="anonymous"
          muted
          preload="auto"
          onseeked={capture}
          onerror={() => (failed = true)}
        ></video>
      {/if}
      <span class="duration">{formatDuration(clip.durationSeconds)}</span>
    </div>
    <span class="title truncate">{clipTitle(clip)}</span>
    <span class="hint truncate">{formatDate(clip.modifiedMs)} · {formatSize(clip.sizeBytes)}</span>
  </button>
  <button
    class="star"
    class:on={clip.starred}
    aria-label="Star clip"
    aria-pressed={clip.starred}
    title={clip.starred ? "Remove star" : "Star"}
    onclick={onstar}
    {oncontextmenu}
  >
    {clip.starred ? "★" : "☆"}
  </button>
</div>

<style>
  .clip {
    position: relative;
    min-width: 0;
  }

  .tile {
    display: flex;
    flex-direction: column;
    gap: 2px;
    width: 100%;
    height: auto;
    padding: 0 0 8px;
    border: 0;
    background: transparent;
    text-align: left;
  }

  .tile:hover:not(:disabled) {
    background: transparent;
  }

  .clip:hover .thumb,
  .tile.selected .thumb {
    outline: 2px solid var(--accent);
  }

  .check {
    position: absolute;
    z-index: 1;
    top: 6px;
    left: 6px;
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    border: 2px solid #fff;
    border-radius: 50%;
    background: rgb(0 0 0 / 0.4);
    color: var(--accent-text);
    font-size: 12px;
    font-weight: 700;
  }

  .selected .check {
    border-color: var(--accent);
    background: var(--accent);
  }

  .thumb {
    position: relative;
    aspect-ratio: 16 / 9;
    margin-bottom: 6px;
    overflow: hidden;
    border-radius: 8px;
    background: var(--surface-2);
  }

  img,
  video {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  .duration {
    position: absolute;
    right: 6px;
    bottom: 6px;
    padding: 1px 6px;
    border-radius: 4px;
    background: rgb(0 0 0 / 0.72);
    color: #fff;
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }

  .title {
    font-weight: 600;
  }

  .star {
    position: absolute;
    top: 6px;
    right: 6px;
    width: 26px;
    height: 26px;
    padding: 0;
    border: 0;
    border-radius: 6px;
    background: rgb(0 0 0 / 0.72);
    color: #fff;
    font-size: 15px;
    line-height: 1;
    opacity: 0;
  }

  .star:hover:not(:disabled) {
    background: rgb(0 0 0 / 0.88);
  }

  .star.on {
    color: #f5b83d;
  }

  .clip:hover .star,
  .star:focus-visible,
  .star.on {
    opacity: 1;
  }
</style>
