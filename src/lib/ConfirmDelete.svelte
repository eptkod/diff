<script lang="ts">
  import { isMac } from "$lib/api";

  let { count, onconfirm }: { count: number; onconfirm: () => void } = $props();

  let dialog = $state<HTMLDialogElement>();

  export const open = () => dialog?.showModal();
  /** Esc closes the dialog, so pages check this before acting on it themselves. */
  export const isOpen = () => dialog?.open ?? false;

  function confirm() {
    dialog?.close();
    onconfirm();
  }
</script>

<dialog bind:this={dialog}>
  <h2>{count === 1 ? "Delete this clip?" : `Delete ${count} clips?`}</h2>
  <p class="hint">Deleted clips go to the {isMac() ? "Trash" : "Recycle Bin"}.</p>
  <!-- Cancel comes first so it takes focus, and Enter can't delete by accident. -->
  <div class="row">
    <button onclick={() => dialog?.close()}>Cancel</button>
    <button class="danger" onclick={confirm}>Delete</button>
  </div>
</dialog>

<style>
  dialog {
    width: 300px;
    padding: 18px;
    border: 1px solid var(--line);
    border-radius: 10px;
    background: var(--surface);
    color: var(--text);
  }

  dialog::backdrop {
    background: rgb(0 0 0 / 0.45);
  }

  h2 {
    font-size: 15px;
    font-weight: 650;
  }

  .row {
    justify-content: flex-end;
    margin-top: 16px;
  }
</style>
