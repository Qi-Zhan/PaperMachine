<template>
  <div class="markdown-view" v-html="html" />
</template>

<script setup lang="ts">
import DOMPurify from 'dompurify'
import { marked } from 'marked'
import { computed } from 'vue'

const props = defineProps<{ source: string }>()

const html = computed(() => {
  const rendered = marked.parse(props.source, { async: false, breaks: true })
  return DOMPurify.sanitize(rendered)
})
</script>
