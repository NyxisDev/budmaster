import { useColorMode } from '@vueuse/core'

// Тема: за замовчуванням — як у системі; вибір користувача зберігається в браузері.
// Ключ сховища збігається зі скриптом в index.html, який ставить тему до завантаження сторінки.
export function useTheme() {
  const mode = useColorMode({ storageKey: 'budmaster-theme', disableTransition: true })

  function toggle() {
    mode.value = mode.value === 'dark' ? 'light' : 'dark'
  }

  return { mode, toggle }
}
