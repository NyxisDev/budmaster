<script setup>
import { computed } from 'vue'
import { RouterLink } from 'vue-router'

// Дерево категорій будь-якої глибини. Посилання зберігають рядок пошуку, але скидають сторінку.
const props = defineProps({
  tree: { type: Array, required: true },
  activeSlug: { type: String, default: '' },
  search: { type: String, default: '' },
})

defineEmits(['navigate'])

// Дерево розгортається у плоский список із рівнем вкладеності — так простіше малювати відступи.
const rows = computed(() => {
  const result = []
  const walk = (nodes, depth) => {
    for (const node of nodes) {
      result.push({ id: node.id, name: node.name, slug: node.slug, depth })
      walk(node.children ?? [], depth + 1)
    }
  }
  walk(props.tree, 0)
  return result
})

const queryFor = (slug) => ({
  ...(slug ? { category: slug } : {}),
  ...(props.search ? { q: props.search } : {}),
})

const linkClass = (active) => [
  'block rounded-md px-3 py-2 text-sm transition-colors',
  active
    ? 'bg-primary/15 font-semibold text-foreground'
    : 'text-muted-foreground hover:bg-accent hover:text-foreground',
]
</script>

<template>
  <nav aria-label="Категорії">
    <ul class="space-y-0.5">
      <li>
        <RouterLink
          :to="{ name: 'catalog', query: queryFor('') }"
          :class="linkClass(!activeSlug)"
          :aria-current="!activeSlug ? 'page' : undefined"
          @click="$emit('navigate')"
        >
          Усі товари
        </RouterLink>
      </li>
      <li
        v-for="row in rows"
        :key="row.id"
        :style="{ paddingLeft: `${Math.min(row.depth, 4) * 0.875}rem` }"
      >
        <RouterLink
          :to="{ name: 'catalog', query: queryFor(row.slug) }"
          :class="linkClass(activeSlug === row.slug)"
          :aria-current="activeSlug === row.slug ? 'page' : undefined"
          @click="$emit('navigate')"
        >
          {{ row.name }}
        </RouterLink>
      </li>
    </ul>
  </nav>
</template>
