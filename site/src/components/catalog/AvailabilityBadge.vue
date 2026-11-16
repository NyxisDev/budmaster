<script setup>
import { computed } from 'vue'
import { AVAILABILITY } from '@/lib/format'

const props = defineProps({
  availability: { type: String, required: true },
})

const info = computed(() => AVAILABILITY[props.availability] ?? AVAILABILITY.out_of_stock)

const toneClass = {
  success: 'text-success',
  warning: 'text-warning',
  muted: 'text-muted-foreground',
}
</script>

<template>
  <span class="inline-flex items-center gap-1.5 text-sm font-medium" :class="toneClass[info.tone]">
    <span class="size-2 rounded-full bg-current" aria-hidden="true" />
    {{ info.label }}
  </span>
</template>
