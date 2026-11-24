<script setup>
import { ref } from 'vue'
import { RouterLink } from 'vue-router'
import { Menu, Moon, Sun } from '@lucide/vue'
import AppLogo from './AppLogo.vue'
import { Button } from '@/components/ui/button'
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from '@/components/ui/sheet'
import { useTheme } from '@/composables/useTheme'
import { company } from '@/content/company'

const { mode, toggle } = useTheme()
const menuOpen = ref(false)

const links = [
  { to: '/#about', label: 'Про нас' },
  { to: '/#contact', label: 'Контакти' },
]
</script>

<template>
  <header class="sticky top-0 z-40 border-b bg-background/90 backdrop-blur">
    <div class="container-page flex h-16 items-center gap-4">
      <RouterLink to="/" class="shrink-0 rounded-md" aria-label="Budmaster — на головну">
        <AppLogo />
      </RouterLink>

      <nav class="ml-4 hidden items-center gap-1 md:flex" aria-label="Основна навігація">
        <RouterLink
          v-for="link in links"
          :key="link.to"
          :to="link.to"
          class="rounded-md px-3 py-2 text-sm font-medium text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          {{ link.label }}
        </RouterLink>
      </nav>

      <div class="ml-auto flex items-center gap-1">
        <a
          :href="company.phones[0].href"
          class="mr-2 hidden text-sm font-semibold whitespace-nowrap lg:block"
        >
          {{ company.phones[0].label }}
        </a>

        <Button
          variant="ghost"
          size="icon"
          :aria-label="mode === 'dark' ? 'Увімкнути світлу тему' : 'Увімкнути темну тему'"
          @click="toggle"
        >
          <Sun v-if="mode === 'dark'" />
          <Moon v-else />
        </Button>

        <Sheet v-model:open="menuOpen">
          <SheetTrigger as-child>
            <Button variant="ghost" size="icon" class="md:hidden" aria-label="Меню">
              <Menu />
            </Button>
          </SheetTrigger>
          <SheetContent side="right" class="p-6">
            <SheetHeader class="p-0">
              <SheetTitle>Меню</SheetTitle>
              <SheetDescription class="sr-only">Розділи сайту та телефони</SheetDescription>
            </SheetHeader>
            <nav class="flex flex-col gap-1" aria-label="Мобільна навігація">
              <RouterLink
                v-for="link in links"
                :key="link.to"
                :to="link.to"
                class="rounded-md px-3 py-3 text-base font-medium hover:bg-accent"
                @click="menuOpen = false"
              >
                {{ link.label }}
              </RouterLink>
            </nav>
            <div class="mt-auto flex flex-col gap-2 border-t pt-4">
              <a
                v-for="phone in company.phones"
                :key="phone.href"
                :href="phone.href"
                class="text-base font-semibold"
              >
                {{ phone.label }}
              </a>
              <p class="text-sm text-muted-foreground">{{ company.hours }}</p>
            </div>
          </SheetContent>
        </Sheet>
      </div>
    </div>
  </header>
</template>
