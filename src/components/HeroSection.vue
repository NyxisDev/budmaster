<script setup>
import { Phone } from '@lucide/vue'
import { company } from '@/content/company'
import { Button } from '@/components/ui/button'

// Путь с учётом адреса публикации: сайт может жить не в корне домена (например, GitHub Pages).
const heroBackground = `${import.meta.env.BASE_URL}hero_background.png`
</script>

<template>
  <!-- Фон — фото з темно-синьою підкладкою, тому текст білий в обох темах. -->
  <section
    id="home"
    class="relative isolate flex min-h-[calc(100svh-4rem)] items-center bg-[#0b1121] bg-cover bg-center"
    :style="{ backgroundImage: `url('${heroBackground}')` }"
  >
    <div class="absolute inset-0 -z-10 bg-gradient-to-br from-[#0b1121]/95 via-[#0b1121]/80 to-[#151d30]/85" />

    <div class="container-page py-20 text-white">
      <p class="mb-4 text-sm font-semibold tracking-[0.2em] text-primary uppercase">
        Чугуїв · Е-Відновлення
      </p>
      <h1 class="max-w-3xl text-4xl leading-tight font-bold text-balance sm:text-5xl lg:text-6xl">
        Ваш надійний <span class="text-primary">будівельний партнер</span>
      </h1>
      <p class="mt-6 max-w-xl text-lg text-slate-300">
        Будівельні матеріали від цементу до покрівлі. Телефонуйте або приходьте —
        підкажемо, підберемо й порахуємо.
      </p>

      <div class="mt-10 flex flex-wrap gap-3">
        <Button size="lg" class="h-12 px-7 text-base" as-child>
          <a :href="company.phones[0].href">
            <Phone />
            {{ company.phones[0].label }}
          </a>
        </Button>
        <Button
          size="lg"
          variant="outline"
          class="h-12 border-white/30 bg-transparent px-7 text-base text-white hover:bg-white/10 hover:text-white dark:border-white/30 dark:bg-transparent dark:hover:bg-white/10"
          as-child
        >
          <a href="#contact">Зв’язатися з нами</a>
        </Button>
      </div>
    </div>
  </section>
</template>
