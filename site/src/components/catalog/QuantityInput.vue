<script setup>
import { computed } from 'vue'
import { Minus, Plus } from '@lucide/vue'
import { Button } from '@/components/ui/button'

// Кількість. Для штучних одиниць — тільки цілі числа, для кг, м², м³ — з кроком 0,1.
const props = defineProps({
  modelValue: { type: Number, required: true },
  allowFraction: { type: Boolean, default: false },
  label: { type: String, default: 'Кількість' },
})
const emit = defineEmits(['update:modelValue'])

const step = computed(() => (props.allowFraction ? 0.1 : 1))
const min = computed(() => step.value)

function sanitize(raw) {
  let value = Number(String(raw).replace(/\s/g, '').replace(',', '.'))
  if (!Number.isFinite(value) || value < min.value) value = min.value
  if (!props.allowFraction) value = Math.floor(value)
  return Math.min(Math.round(value * 1000) / 1000, 99999)
}

function set(raw) {
  emit('update:modelValue', sanitize(raw))
}
</script>

<template>
  <div class="inline-flex items-center rounded-md border border-input">
    <Button
      type="button"
      variant="ghost"
      size="icon"
      class="rounded-r-none"
      :aria-label="`${label}: зменшити`"
      :disabled="modelValue <= min"
      @click="set(modelValue - step)"
    >
      <Minus />
    </Button>
    <input
      type="text"
      :inputmode="allowFraction ? 'decimal' : 'numeric'"
      :value="String(modelValue).replace('.', ',')"
      :aria-label="label"
      class="h-9 w-16 border-x border-input bg-transparent text-center text-sm tabular-nums outline-none focus-visible:ring-3 focus-visible:ring-ring/50"
      @change="set($event.target.value); $event.target.value = String(sanitize($event.target.value)).replace('.', ',')"
    />
    <Button
      type="button"
      variant="ghost"
      size="icon"
      class="rounded-l-none"
      :aria-label="`${label}: збільшити`"
      @click="set(modelValue + step)"
    >
      <Plus />
    </Button>
  </div>
</template>
