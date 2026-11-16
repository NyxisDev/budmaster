<script setup>
import { RouterView } from 'vue-router'
import AppHeader from '@/components/layout/AppHeader.vue'
import FooterSection from '@/components/FooterSection.vue'
import { isDemo } from '@/api'
</script>

<template>
  <a
    href="#main"
    class="sr-only focus:not-sr-only focus:fixed focus:top-2 focus:left-2 focus:z-50 focus:rounded-md focus:bg-primary focus:px-4 focus:py-2 focus:text-primary-foreground"
  >
    Перейти до вмісту
  </a>
  <!-- Поки сайт не під’єднано до сервера, відвідувач має знати, що ціни умовні, а заявки не доходять. -->
  <p
    v-if="isDemo"
    class="bg-primary px-4 py-2 text-center text-sm font-medium text-primary-foreground"
    role="note"
  >
    Демонстраційна версія: ціни та наявність умовні, заявки з сайту не надсилаються.
  </p>
  <AppHeader />
  <main id="main" class="min-h-[60vh]">
    <RouterView />
  </main>
  <FooterSection />
</template>
