<template>
  <Hello :msg="'<script>not code</script>'" />
</template>

<script setup lang="ts">
import Hello from "./components/Hello.vue";
import type { Props } from "./types";
</script>

<style scoped>
.a { color: red }
</style>
