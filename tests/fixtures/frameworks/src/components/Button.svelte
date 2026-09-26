<script lang="ts">
  import { format } from "../util";
  import { onMount } from "svelte";
  let { label }: { label: string } = $props();
</script>

<svelte:head>
  <script src="https://cdn.example.com/analytics.js"></script>
</svelte:head>

<button>{format(label)}</button>
