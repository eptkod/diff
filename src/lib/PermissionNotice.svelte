<script lang="ts">
  import { api } from "$lib/api";

  let { onretry }: { onretry: () => void } = $props();

  async function allow() {
    const granted = await api.requestScreenPermission();
    if (granted) onretry();
    else await api.openPermissionSettings();
  }
</script>

<div class="notice">
  <p><strong>diff needs permission to record your screen.</strong></p>
  <p class="hint">
    Turn on diff in System Settings under Screen &amp; System Audio Recording, then reopen diff.
    Your recordings stay on this computer.
  </p>
  <div class="row">
    <button class="primary" onclick={allow}>Allow screen recording</button>
    <button onclick={onretry}>Check again</button>
  </div>
</div>

<style>
  .notice {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 14px;
    border: 1px solid var(--line);
    border-radius: 10px;
    background: var(--surface);
  }
</style>
