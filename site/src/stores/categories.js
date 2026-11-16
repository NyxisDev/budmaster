import { computed, ref } from 'vue'
import { defineStore } from 'pinia'
import { getCategories } from '@/api'

// Категорії завантажуються один раз і використовуються в каталозі, картці товару та на головній.
export const useCategoriesStore = defineStore('categories', () => {
  const items = ref([])
  const status = ref('idle') // idle | loading | ready | error
  let pending = null

  function load() {
    if (status.value === 'ready') return Promise.resolve()
    if (pending) return pending
    status.value = 'loading'
    pending = getCategories()
      .then((response) => {
        items.value = [...response.items].sort((a, b) => a.sort_order - b.sort_order)
        status.value = 'ready'
      })
      .catch(() => {
        status.value = 'error'
      })
      .finally(() => {
        pending = null
      })
    return pending
  }

  /** Дерево: кореневі категорії з полем children. */
  const tree = computed(() => {
    const childrenOf = (parentId) =>
      items.value
        .filter((category) => category.parent_id === parentId)
        .map((category) => ({ ...category, children: childrenOf(category.id) }))
    return childrenOf(null)
  })

  const bySlug = (slug) => items.value.find((category) => category.slug === slug) ?? null
  const byId = (id) => items.value.find((category) => category.id === id) ?? null

  /** Шлях від кореня до категорії — для «хлібних крихт». */
  function pathTo(id) {
    const path = []
    let current = byId(id)
    while (current && path.length < 10) {
      path.unshift(current)
      current = current.parent_id ? byId(current.parent_id) : null
    }
    return path
  }

  return { items, status, tree, load, bySlug, byId, pathTo }
})
