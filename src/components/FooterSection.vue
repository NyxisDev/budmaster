<script setup>
import { RouterLink } from 'vue-router'
import AppLogo from './layout/AppLogo.vue'
import { company } from '@/content/company'

const year = new Date().getFullYear()
</script>

<template>
  <footer class="border-t bg-card text-card-foreground">
    <div class="container-page py-12">
      <p class="font-heading text-2xl leading-snug font-bold sm:text-3xl">
        Обирайте якість. Обирайте надійність.
        <span class="text-brand">Обирайте нас!</span>
      </p>

      <div class="mt-10 grid gap-8 border-t pt-8 sm:grid-cols-3">
        <div>
          <AppLogo />
          <p class="mt-3 text-sm text-muted-foreground">{{ company.address }}</p>
          <p class="text-sm text-muted-foreground">{{ company.hours }}</p>
        </div>

        <nav class="flex flex-col gap-2 text-sm" aria-label="Навігація у футері">
          <RouterLink to="/#about" class="hover:text-brand">Про нас</RouterLink>
          <RouterLink to="/#contact" class="hover:text-brand">Контакти</RouterLink>
        </nav>

        <div class="flex flex-col gap-2 text-sm">
          <a
            v-for="phone in company.phones"
            :key="phone.href"
            :href="phone.href"
            class="font-medium tabular-nums hover:text-brand"
          >
            {{ phone.label }}
          </a>
        </div>
      </div>

      <p class="mt-8 text-sm text-muted-foreground">
        © {{ year }} {{ company.legalName }}. Всі права захищені.
      </p>
    </div>
  </footer>
</template>
