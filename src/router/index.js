import { createRouter, createWebHistory } from 'vue-router'
import HomeView from '@/views/HomeView.vue'

const router = createRouter({
  history: createWebHistory(import.meta.env.BASE_URL),
  routes: [
    { path: '/', name: 'home', component: HomeView, meta: { title: 'Budmaster — будівельні матеріали в Чугуєві' } },
    { path: '/:pathMatch(.*)*', name: 'not-found', component: () => import('@/views/NotFoundView.vue'), meta: { title: 'Сторінку не знайдено — Budmaster' } },
  ],
  scrollBehavior(to, from, savedPosition) {
    // Перше відкриття посилання з якорем (/#contact): якір важливіший за збережену позицію.
    if (to.hash && !from.name) return { el: to.hash, top: 72, behavior: 'instant' }
    if (savedPosition) return savedPosition
    if (to.hash) return { el: to.hash, top: 72 }
    return { top: 0 }
  },
})

router.afterEach((to) => {
  if (to.meta.title) document.title = to.meta.title
})

export default router
