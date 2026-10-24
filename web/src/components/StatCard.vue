<script setup lang="ts">
import type { Tone } from '../labels'

withDefaults(
  defineProps<{
    label: string
    value: string
    foot?: string
    tone?: Tone
    title?: string
  }>(),
  { foot: '', tone: 'muted', title: '' },
)
</script>

<template>
  <div class="stat" :class="tone" :title="title">
    <div class="stat-label">{{ label }}</div>
    <div class="stat-value">{{ value }}</div>
    <div v-if="foot" class="stat-foot">{{ foot }}</div>
  </div>
</template>
