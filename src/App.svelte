<script lang="ts">
  import { getVersion } from '@tauri-apps/api/app';
  import { invoke } from '@tauri-apps/api/core';

  let version = $state('');
  let colab = $state<string | null | undefined>(undefined);

  $effect(() => {
    getVersion().then((value) => (version = value));
    invoke<string | null>('colab_path').then((value) => (colab = value));
  });
</script>

<main>
  <h1>Colab ComfyUI</h1>
  <p class="version">{version ? `v${version}` : ''}</p>
  <dl>
    <dt>colab</dt>
    <dd>
      {#if colab === undefined}
        …
      {:else if colab === null}
        not found
      {:else}
        {colab}
      {/if}
    </dd>
  </dl>
</main>

<style>
  :global(body) {
    margin: 0;
    font: 14px/1.5 -apple-system, BlinkMacSystemFont, sans-serif;
    color-scheme: light dark;
  }

  main {
    padding: 48px;
  }

  h1 {
    margin: 0;
    font-size: 24px;
  }

  .version {
    margin: 4px 0 24px;
    opacity: 0.6;
  }

  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 4px 16px;
    margin: 0;
  }

  dt {
    opacity: 0.6;
  }

  dd {
    margin: 0;
    font-family: ui-monospace, monospace;
    word-break: break-all;
  }
</style>
