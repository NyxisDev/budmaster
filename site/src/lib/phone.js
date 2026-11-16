/**
 * Приводить український номер до вигляду +380XXXXXXXXX.
 * Приймає «050 123 45 67», «+38 (050) 123-45-67», «380501234567».
 * Повертає null, якщо це не український номер.
 */
export function normalizePhone(input) {
  const digits = String(input ?? '').replace(/\D/g, '')
  let national
  if (digits.length === 12 && digits.startsWith('380')) national = digits.slice(3)
  else if (digits.length === 11 && digits.startsWith('80')) national = digits.slice(2)
  else if (digits.length === 10 && digits.startsWith('0')) national = digits.slice(1)
  else return null
  if (national.startsWith('0')) return null
  return `+380${national}`
}
