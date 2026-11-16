// Помилка API. Сервер відповідає { "error": { "code": "...", "fields": { ... } } }.
export class ApiError extends Error {
  constructor(status, code, fields = {}) {
    super(`API error ${status}: ${code}`)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.fields = fields
  }
}
