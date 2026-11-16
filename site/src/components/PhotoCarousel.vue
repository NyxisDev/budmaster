<script setup>
import { ref } from 'vue'
import { ChevronLeft, ChevronRight } from '@lucide/vue'

const props = defineProps({
  images: { type: Array, required: true },
  label: { type: String, required: true },
})

const current = ref(0)

function go(index) {
  const count = props.images.length
  current.value = (index + count) % count
}
</script>

<template>
  <div
    class="relative overflow-hidden rounded-xl border bg-muted"
    role="group"
    aria-roledescription="карусель"
    :aria-label="label"
  >
    <div class="aspect-[4/3] w-full">
      <Transition
        mode="out-in"
        enter-active-class="transition-opacity duration-300"
        leave-active-class="transition-opacity duration-200"
        enter-from-class="opacity-0"
        leave-to-class="opacity-0"
      >
        <img
          :key="current"
          :src="images[current]"
          :alt="`${label}, фото ${current + 1} з ${images.length}`"
          class="h-full w-full object-cover"
          loading="lazy"
        />
      </Transition>
    </div>

    <button
      type="button"
      class="absolute top-1/2 left-3 flex size-10 -translate-y-1/2 items-center justify-center rounded-full bg-[#0b1121]/70 text-white transition-colors hover:bg-primary hover:text-primary-foreground"
      aria-label="Попереднє фото"
      @click="go(current - 1)"
    >
      <ChevronLeft class="size-5" />
    </button>
    <button
      type="button"
      class="absolute top-1/2 right-3 flex size-10 -translate-y-1/2 items-center justify-center rounded-full bg-[#0b1121]/70 text-white transition-colors hover:bg-primary hover:text-primary-foreground"
      aria-label="Наступне фото"
      @click="go(current + 1)"
    >
      <ChevronRight class="size-5" />
    </button>

    <div class="absolute bottom-3 left-1/2 flex -translate-x-1/2 gap-1 rounded-full bg-[#0b1121]/60 px-2 py-1">
      <button
        v-for="(image, index) in images"
        :key="image"
        type="button"
        class="flex size-5 items-center justify-center"
        :aria-label="`Фото ${index + 1}`"
        :aria-current="index === current"
        @click="go(index)"
      >
        <span
          class="size-2 rounded-full transition-colors"
          :class="index === current ? 'bg-primary' : 'bg-white/60'"
        />
      </button>
    </div>
  </div>
</template>
