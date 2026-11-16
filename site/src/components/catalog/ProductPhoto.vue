<script setup>
import { Package } from '@lucide/vue'

// Фото товару або нейтральна заглушка, якщо фото ще не завантажили.
defineProps({
  src: { type: String, default: '' },
  alt: { type: String, required: true },
  compact: { type: Boolean, default: false }, // маленька мініатюра без підпису
})
</script>

<template>
  <div class="aspect-[4/3] w-full overflow-hidden bg-muted">
    <img v-if="src" :src="src" :alt="alt" class="h-full w-full object-cover" loading="lazy" />
    <div
      v-else
      class="flex h-full w-full flex-col items-center justify-center gap-2 text-muted-foreground"
      role="img"
      :aria-label="`${alt} — фото відсутнє`"
    >
      <Package :class="compact ? 'size-6' : 'size-10'" class="opacity-50" aria-hidden="true" />
      <span v-if="!compact" class="text-xs">Фото скоро буде</span>
    </div>
  </div>
</template>
