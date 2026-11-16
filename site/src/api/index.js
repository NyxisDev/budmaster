// Публічний API сайту. Поки сервера немає, працює на демонстраційних даних.
// Щоб перейти на справжній сервер, задайте в .env:
//   VITE_USE_MOCKS=false
//   VITE_API_URL=https://адреса-сервера

import { ApiError } from './errors'

export { ApiError }

const useMocks = import.meta.env.VITE_USE_MOCKS !== 'false'

/** true — сайт показує демонстраційні дані, заявки нікуди не надсилаються. */
export const isDemo = useMocks
const baseUrl = `${(import.meta.env.VITE_API_URL ?? '').replace(/\/$/, '')}/api/v1`

async function request(path, { method = 'GET', query, body } = {}) {
  const url = new URL(baseUrl + path, window.location.origin)
  for (const [key, value] of Object.entries(query ?? {})) {
    if (value !== undefined && value !== null && value !== '') url.searchParams.set(key, value)
  }

  let response
  try {
    response = await fetch(url, {
      method,
      headers: body ? { 'Content-Type': 'application/json' } : undefined,
      body: body ? JSON.stringify(body) : undefined,
    })
  } catch {
    throw new ApiError(0, 'network')
  }

  const data = await response.json().catch(() => null)
  if (!response.ok) {
    throw new ApiError(response.status, data?.error?.code ?? 'unknown', data?.error?.fields ?? {})
  }
  // Відповідь 200 без JSON (сторінка-заглушка, портал Wi-Fi) — це збій, а не успіх.
  if (data === null || typeof data !== 'object') throw new ApiError(response.status, 'bad_response')
  return data
}

const http = {
  getCategories: () => request('/public/categories'),
  getProducts: (params) => request('/public/products', { query: params }),
  getProduct: (id) => request(`/public/products/${encodeURIComponent(id)}`),
  createOrder: (payload) => request('/public/orders', { method: 'POST', body: payload }),
}

// Демонстраційні дані завантажуються окремим файлом і не потрапляють у збірку для справжнього сервера.
const impl = async () => (useMocks ? await import('./mock/server') : http)

/** Категорії плоским списком: { items: [{ id, parent_id, name, slug, sort_order }] } */
export const getCategories = async () => (await impl()).getCategories()

/** Товари: параметри { category_id, q, page, per_page } → { items, page, per_page, total } */
export const getProducts = async (params) => (await impl()).getProducts(params)

/** Картка товару за id. Якщо товару немає — ApiError зі status 404. */
export const getProduct = async (id) => (await impl()).getProduct(id)

/** Заявка з сайту → { id, number, total }. Ціни рахує сервер. */
export const createOrder = async (payload) => (await impl()).createOrder(payload)
