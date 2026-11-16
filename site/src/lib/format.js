const priceFormat = new Intl.NumberFormat('uk-UA', {
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
})
const qtyFormat = new Intl.NumberFormat('uk-UA', { maximumFractionDigits: 3 })

/** «1 234,50 ₴». Приймає число або десятковий рядок з API. */
export function formatPrice(value) {
  return `${priceFormat.format(Number(value))} ₴`
}

export function formatQty(value) {
  return qtyFormat.format(Number(value))
}

export const AVAILABILITY = {
  in_stock: { label: 'В наявності', tone: 'success' },
  coming_soon: { label: 'Скоро з’явиться', tone: 'warning' },
  out_of_stock: { label: 'Немає в наявності', tone: 'muted' },
}
