<script setup lang="ts">
withDefaults(
  defineProps<{
    title?: string
    hint?: string
    /** Removes body padding, for tables that run edge to edge. */
    tight?: boolean
  }>(),
  { title: '', hint: '', tight: false },
)
</script>

<template>
  <section class="card">
    <header v-if="title || $slots.actions" class="card-head">
      <h3 v-if="title">{{ title }}</h3>
      <span v-if="hint" class="hint">{{ hint }}</span>
      <span class="spacer" />
      <slot name="actions" />
    </header>
    <div class="card-body" :class="{ tight }">
      <slot />
    </div>
  </section>
</template>
