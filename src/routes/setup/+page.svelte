<script lang="ts">
  import { onMount } from "svelte";
  import { api, formatHotkey, formatLength, onSettingsChanged, type AppState } from "$lib/api";
  import PermissionNotice from "$lib/PermissionNotice.svelte";
  import SettingsForm from "$lib/SettingsForm.svelte";

  let app = $state<AppState | null>(null);

  const load = async () => {
    app = await api.getState();
  };

  onMount(() => {
    load();
    const stop = onSettingsChanged(load);
    return () => {
      stop.then((unlisten) => unlisten());
    };
  });
</script>

{#if app}
  <main class="page">
    <div class="field">
      <h1>Welcome to diff</h1>
      <p class="hint">
        diff quietly keeps the last {formatLength(app.settings.clipSeconds)} of your screen. Press
        {formatHotkey(app.settings.hotkey)} or say “diff clip that” to save it.
      </p>
    </div>

    {#if app.hasPermission}
      <SettingsForm settings={app.settings} onsaved={load} full={false} />
      <button class="primary" onclick={api.finishSetup}>Start recording</button>
    {:else}
      <PermissionNotice onretry={load} />
    {/if}
  </main>
{/if}
