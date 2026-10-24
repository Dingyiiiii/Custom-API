<script setup lang="ts">
import type { Tone } from '../labels'

withDefaults(defineProps<{ text: string; tone?: Tone; title?: string }>(), {
  tone: 'muted',
  title: '',
})
</script>

<template>
  <span class="badge" :class="tone" :title="title">{{ text }}</span>
</template>
