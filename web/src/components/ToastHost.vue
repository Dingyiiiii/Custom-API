<script setup lang="ts">
import { useAppStore } from '../store'

const store = useAppStore()
</script>

<template>
  <div class="toasts">
    <div
      v-for="toast in store.toasts"
      :key="toast.id"
      class="toast"
      :class="toast.kind"
      @click="store.dismiss(toast.id)"
    >
      {{ toast.text }}
    </div>
  </div>
</template>
