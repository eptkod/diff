<script lang="ts">
  import { onMount } from "svelte";
  import { api, onSettingsChanged, type AppState } from "$lib/api";
  import PermissionNotice from "$lib/PermissionNotice.svelte";
  import SettingsForm from "$lib/SettingsForm.svelte";

  let app = $state<AppState | null>(null);

  const load = async () => {
    app = await api.getState();
  };
  const retry = async () => {
    app = await api.retryCapture();
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
    <h1>Settings</h1>
    {#if app.status.state === "needsPermission"}
      <PermissionNotice onretry={retry} />
    {:else if app.status.state === "error"}
      <div class="row spread">
        <p class="error">{app.status.message}</p>
        <button onclick={retry}>Try again</button>
      </div>
    {/if}
    {#if app.voiceIssue}
      <div class="row spread">
        <p class="error">{app.voiceIssue}</p>
        <button onclick={api.openMicrophoneSettings}>Open settings</button>
      </div>
    {/if}
    <SettingsForm settings={app.settings} following={app.following} onsaved={load} />
  </main>
{/if}
