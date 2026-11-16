<script setup>
import { onMounted } from 'vue'
import { RouterLink } from 'vue-router'
import { ArrowRight } from '@lucide/vue'
import HeroSection from '@/components/HeroSection.vue'
import FeaturesSection from '@/components/FeaturesSection.vue'
import ContactSection from '@/components/ContactSection.vue'
import { Button } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'
import { useCategoriesStore } from '@/stores/categories'

const categories = useCategoriesStore()
onMounted(() => categories.load())
</script>

<template>
  <HeroSection />

  <section class="py-20" aria-labelledby="home-catalog-title">
    <div class="container-page">
      <div class="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h2 id="home-catalog-title" class="text-3xl font-bold sm:text-4xl">Каталог</h2>
          <div class="mt-4 h-1 w-14 rounded-full bg-primary" />
        </div>
        <Button variant="outline" as-child>
          <RouterLink to="/catalog">
            Усі товари
            <ArrowRight />
          </RouterLink>
        </Button>
      </div>

      <div
        v-if="categories.status === 'loading' || categories.status === 'idle'"
        class="mt-10 grid gap-4 sm:grid-cols-2 lg:grid-cols-4"
      >
        <Skeleton v-for="n in 8" :key="n" class="h-24 rounded-xl" />
      </div>

      <ul v-else-if="categories.tree.length" class="mt-10 grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <li v-for="category in categories.tree" :key="category.id">
          <RouterLink
            :to="{ name: 'catalog', query: { category: category.slug } }"
            class="flex h-full flex-col justify-between gap-3 rounded-xl border bg-card p-5 text-card-foreground transition-colors hover:border-primary"
          >
            <span class="font-heading text-lg leading-snug font-semibold">{{ category.name }}</span>
            <span v-if="category.children.length" class="text-sm text-muted-foreground">
              {{ category.children.map((child) => child.name).join(' · ') }}
            </span>
          </RouterLink>
        </li>
      </ul>

      <p v-else class="mt-10 text-muted-foreground">
        {{ categories.status === 'error' ? 'Не вдалося завантажити категорії.' : 'Категорії ще не додано.' }}
        <RouterLink to="/catalog" class="font-medium text-brand underline underline-offset-4">
          Відкрити каталог
        </RouterLink>
      </p>
    </div>
  </section>

  <FeaturesSection />
  <ContactSection />
</template>
