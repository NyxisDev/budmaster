<script setup>
import { RouterView } from 'vue-router'
import AppHeader from '@/components/layout/AppHeader.vue'
import FooterSection from '@/components/FooterSection.vue'
</script>

<template>
  <a
    href="#main"
    class="sr-only focus:not-sr-only focus:fixed focus:top-2 focus:left-2 focus:z-50 focus:rounded-md focus:bg-primary focus:px-4 focus:py-2 focus:text-primary-foreground"
  >
    Перейти до вмісту
  </a>
  <AppHeader />
  <main id="main" class="min-h-[60vh]">
    <RouterView />
  </main>
  <FooterSection />
</template>
