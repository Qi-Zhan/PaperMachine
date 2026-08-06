<template>
  <span class="status-badge" :data-status="status">
    <span class="status-dot" aria-hidden="true" />
    {{ statusLabel(status) }}
  </span>
</template>

<script setup lang="ts">
import { statusLabel } from '../format'

defineProps<{ status: string }>()
</script>
