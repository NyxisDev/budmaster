// Імітація публічного API в браузері. Поводиться так само, як майбутній сервер:
// ті самі параметри, ті самі відповіді й коди помилок (docs/design.md, розділ 12.1).

import { categories, products } from './data'
import { ApiError } from '../errors'
import { normalizePhone } from '@/lib/phone'

const DELAY_MS = 250
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i
const MIN_FILL_MS = 2000 // людина не заповнить форму швидше
const MAX_QTY = 99999

const delay = () => new Promise((resolve) => setTimeout(resolve, DELAY_MS))
const clone = (value) => JSON.parse(JSON.stringify(value))

const acceptedOrders = new Map()
let nextOrderNumber = 1001

function descendantIds(categoryId) {
  const ids = new Set([categoryId])
  let grew = true
  while (grew) {
    grew = false
    for (const category of categories) {
      if (category.parent_id && ids.has(category.parent_id) && !ids.has(category.id)) {
        ids.add(category.id)
        grew = true
      }
    }
  }
  return ids
}

export async function getCategories() {
  await delay()
  return { items: clone(categories) }
}

export async function getProducts({ category_id, q, page = 1, per_page = 12 } = {}) {
  await delay()
  let items = products

  if (category_id) {
    const ids = descendantIds(category_id)
    items = items.filter((product) => ids.has(product.category_id))
  }

  const query = (q ?? '').trim().toLocaleLowerCase('uk')
  if (query) {
    items = items.filter(
      (product) =>
        product.name.toLocaleLowerCase('uk').includes(query) ||
        product.description.toLocaleLowerCase('uk').includes(query),
    )
  }

  const safePerPage = Math.min(Math.max(Number(per_page) || 12, 1), 48)
  const safePage = Math.max(Number(page) || 1, 1)
  const start = (safePage - 1) * safePerPage

  return {
    items: clone(items.slice(start, start + safePerPage)),
    page: safePage,
    per_page: safePerPage,
    total: items.length,
  }
}

export async function getProduct(id) {
  await delay()
  const found = products.find((product) => product.id === id)
  if (!found) throw new ApiError(404, 'not_found')
  return clone(found)
}

export async function createOrder(payload) {
  await delay()

  // Поле-пастка: людина його не бачить і не заповнює.
  if (payload.website) throw new ApiError(422, 'validation', { website: 'invalid' })
  if (!(Number(payload.elapsed_ms) >= MIN_FILL_MS)) {
    throw new ApiError(422, 'validation', { elapsed_ms: 'too_fast' })
  }

  const fields = {}
  const name = (payload.customer_name ?? '').trim()
  if (name.length < 2 || name.length > 100) fields.customer_name = 'invalid'
  if (!normalizePhone(payload.customer_phone)) fields.customer_phone = 'invalid'
  if ((payload.comment ?? '').length > 1000) fields.comment = 'too_long'
  if (!['pickup', 'delivery'].includes(payload.delivery_type)) fields.delivery_type = 'invalid'
  if (payload.delivery_type === 'delivery' && !(payload.delivery_address ?? '').trim()) {
    fields.delivery_address = 'required'
  }
  if (!UUID_RE.test(payload.id ?? '')) fields.id = 'invalid'
  if (!Array.isArray(payload.items) || payload.items.length < 1 || payload.items.length > 100) {
    fields.items = 'invalid'
  }

  let total = 0
  if (!fields.items) {
    const seen = new Set()
    for (const item of payload.items) {
      const lineKey = `${item.product_id}:${item.stock_unit}`
      const duplicate = seen.has(lineKey)
      seen.add(lineKey)
      const found = products.find((product) => product.id === item.product_id)
      const qty = Number(item.qty)
      const isPack = item.stock_unit === 'pack'
      const wholeOnly = isPack || !found?.unit.allow_fraction
      const valid =
        !duplicate &&
        found &&
        ['unit', 'pack'].includes(item.stock_unit) &&
        (!isPack || found.pack) &&
        Number.isFinite(qty) &&
        qty > 0 &&
        qty <= MAX_QTY &&
        Math.abs(qty * 1000 - Math.round(qty * 1000)) < 1e-6 && // не більше 3 знаків після коми
        (!wholeOnly || Number.isInteger(qty))
      if (!valid) {
        fields.items = 'invalid'
        break
      }
      total += Math.round(qty * Number(isPack ? found.pack.price : found.price) * 100) / 100
    }
  }

  if (Object.keys(fields).length) throw new ApiError(422, 'validation', fields)

  // Повторне надсилання тієї самої заявки не створює дубль.
  if (acceptedOrders.has(payload.id)) return clone(acceptedOrders.get(payload.id))

  const result = { id: payload.id, number: nextOrderNumber++, total: total.toFixed(2) }
  acceptedOrders.set(payload.id, result)
  return clone(result)
}
