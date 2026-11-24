import { fileURLToPath, URL } from 'node:url'

import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import tailwindcss from '@tailwindcss/vite'
import vueDevTools from 'vite-plugin-vue-devtools'

// Повна адреса сайту для картки посилання (index.html, __SITE_URL__): месенджери беруть картинку
// лише за абсолютною адресою. На Netlify — змінна URL (основна адреса сайту), інакше SITE_URL;
// локально порожньо — адреси відносні.
const siteUrl = (process.env.SITE_URL || process.env.URL || '').replace(/\/$/, '')

function siteUrlInHtml() {
  return {
    name: 'site-url-in-html',
    transformIndexHtml: (html) => html.replaceAll('__SITE_URL__', siteUrl),
  }
}

// https://vite.dev/config/
export default defineConfig({
  plugins: [vue(), tailwindcss(), vueDevTools(), siteUrlInHtml()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
})
