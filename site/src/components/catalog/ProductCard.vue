<script setup>
import { RouterLink } from 'vue-router'
import AvailabilityBadge from './AvailabilityBadge.vue'
import ProductPhoto from './ProductPhoto.vue'
import { formatPrice } from '@/lib/format'

defineProps({
  product: { type: Object, required: true },
})
</script>

<template>
  <article
    class="group relative flex flex-col overflow-hidden rounded-xl border bg-card text-card-foreground transition-shadow focus-within:ring-3 focus-within:ring-ring/50 hover:shadow-md"
  >
    <ProductPhoto :src="product.images[0]?.thumb_url" :alt="product.name" />

    <div class="flex flex-1 flex-col gap-2 p-4">
      <h3 class="text-base leading-snug font-semibold">
        <RouterLink
          :to="{ name: 'product', params: { id: product.id } }"
          class="outline-none after:absolute after:inset-0 group-hover:text-brand"
        >
          {{ product.name }}
        </RouterLink>
      </h3>

      <AvailabilityBadge :availability="product.availability" />

      <div class="mt-auto pt-2">
        <p class="font-heading text-xl font-bold tabular-nums">
          {{ formatPrice(product.price) }}
          <span class="text-sm font-normal text-muted-foreground">
            / {{ product.unit.short_name }}
          </span>
        </p>
        <p v-if="product.pack" class="text-sm text-muted-foreground tabular-nums">
          пачка {{ product.pack.size }} {{ product.unit.short_name }} —
          {{ formatPrice(product.pack.price) }}
        </p>
      </div>
    </div>
  </article>
</template>
