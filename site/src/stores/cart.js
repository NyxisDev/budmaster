import { computed } from 'vue'
import { defineStore } from 'pinia'
import { useLocalStorage } from '@vueuse/core'

export const MAX_QTY = 99999
export const MAX_LINES = 100

// Кошик зберігається в браузері. У рядку лежить копія товару на момент додавання —
// щоб показати кошик без запиту до сервера; остаточні ціни рахує сервер.
export const useCartStore = defineStore('cart', () => {
  const lines = useLocalStorage('budmaster-cart', [])

  // У сховищі може лежати що завгодно: стара версія формату, зіпсований запис.
  // Усе, що не схоже на рядок кошика, відкидаємо — інакше впаде весь сайт.
  lines.value = Array.isArray(lines.value) ? lines.value.filter(isValidLine) : []

  const lineKey = (productId, stockUnit) => `${productId}:${stockUnit}`

  const unitPrice = (line) =>
    Number(line.stock_unit === 'pack' ? line.product.pack.price : line.product.price)

  // Сума рядка округлюється до копійок, а підсумок складається з уже округлених сум —
  // тоді «Разом» завжди дорівнює сумі рядків на екрані.
  const lineTotal = (line) => Math.round(unitPrice(line) * line.qty * 100) / 100

  const count = computed(() => lines.value.length)
  const total = computed(() =>
    lines.value.reduce((sum, line) => Math.round((sum + lineTotal(line)) * 100) / 100, 0),
  )
  const isFull = computed(() => lines.value.length >= MAX_LINES)

  /** Повертає false, якщо в кошику вже максимум позицій. */
  function add(product, stockUnit, qty) {
    const key = lineKey(product.id, stockUnit)
    const existing = lines.value.find((line) => line.key === key)
    if (existing) {
      existing.qty = normalizeQty(existing.qty + qty, existing.stock_unit, product)
      existing.product = product
      return true
    }
    if (isFull.value) return false
    lines.value.push({
      key,
      stock_unit: stockUnit,
      qty: normalizeQty(qty, stockUnit, product),
      product,
    })
    return true
  }

  function setQty(key, qty) {
    const line = lines.value.find((item) => item.key === key)
    if (line) line.qty = normalizeQty(qty, line.stock_unit, line.product)
  }

  function remove(key) {
    lines.value = lines.value.filter((line) => line.key !== key)
  }

  /**
   * Оновлює копію товару або прибирає рядок, якщо товару (чи його фасування) більше немає.
   * Повертає, що змінилося: { removed, priceChanged } — щоб повідомити покупця.
   */
  function refreshProduct(productId, product) {
    const result = { removed: 0, priceChanged: 0 }
    const next = []
    for (const line of lines.value) {
      if (line.product.id !== productId) {
        next.push(line)
        continue
      }
      if (!product || !isValidProduct(product) || (line.stock_unit === 'pack' && !product.pack)) {
        result.removed += 1
        continue
      }
      const updated = { ...line, product, qty: normalizeQty(line.qty, line.stock_unit, product) }
      if (unitPrice(updated) !== unitPrice(line)) result.priceChanged += 1
      next.push(updated)
    }
    lines.value = next
    return result
  }

  function clear() {
    lines.value = []
  }

  return {
    lines,
    count,
    total,
    isFull,
    unitPrice,
    lineTotal,
    add,
    setQty,
    remove,
    refreshProduct,
    clear,
  }
})

/** Кількість у межах 0,001…99999; для штучних одиниць і пачок — ціле число від 1. */
function normalizeQty(value, stockUnit, product) {
  const fraction = stockUnit === 'unit' && product.unit.allow_fraction
  let qty = Number(value)
  if (!Number.isFinite(qty)) qty = 1
  qty = fraction ? Math.round(qty * 1000) / 1000 : Math.round(qty)
  const min = fraction ? 0.001 : 1
  return Math.min(Math.max(qty, min), MAX_QTY)
}

function isValidProduct(product) {
  return (
    product !== null &&
    typeof product === 'object' &&
    typeof product.id === 'string' &&
    typeof product.name === 'string' &&
    Number.isFinite(Number(product.price)) &&
    product.unit !== null &&
    typeof product.unit === 'object' &&
    typeof product.unit.short_name === 'string' &&
    Array.isArray(product.images) &&
    (product.pack === null ||
      (typeof product.pack === 'object' && Number.isFinite(Number(product.pack.price))))
  )
}

function isValidLine(line) {
  return (
    line !== null &&
    typeof line === 'object' &&
    typeof line.key === 'string' &&
    (line.stock_unit === 'unit' || line.stock_unit === 'pack') &&
    Number.isFinite(line.qty) &&
    line.qty > 0 &&
    isValidProduct(line.product) &&
    (line.stock_unit === 'unit' || line.product.pack !== null)
  )
}
