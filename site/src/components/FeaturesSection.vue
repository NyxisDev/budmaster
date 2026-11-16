<script setup>
import { Banknote, CloudRain, MessagesSquare, PackageCheck, Truck } from '@lucide/vue'
import PhotoCarousel from './PhotoCarousel.vue'

const images = ['rainway', 'img1', 'img2', 'img3', 'img4', 'img5', 'img6'].map(
  (name) => new URL(`../assets/img/${name}.webp`, import.meta.url).href,
)

const features = [
  {
    icon: PackageCheck,
    title: 'Широкий асортимент',
    items: ['від цементу до лакофарбових матеріалів', 'від гіпсокартону до покрівлі'],
  },
  {
    icon: Truck,
    title: 'Доставка по місту та району',
    items: ['швидко', 'надійно', 'з гарантією'],
  },
  {
    icon: Banknote,
    title: 'Ціни від виробника',
    items: [
      'оплата через термінал',
      'оплата готівкою',
      'на картку',
      'по перерахунку',
      'через Е-Відновлення',
    ],
  },
  {
    icon: MessagesSquare,
    title: 'Консультація від фахівця',
    items: ['допоможемо підібрати все для вашого проєкту'],
  },
  {
    icon: CloudRain,
    title: 'Водостічні системи Rainway',
    items: [
      'сухі стіни в будь-яку зливу',
      'елементи збираються як Lego',
      'весь комплект на складі — забирай та став',
    ],
  },
]
</script>

<template>
  <section id="about" class="border-t py-20">
    <div class="container-page">
      <div class="grid items-center gap-10 lg:grid-cols-2">
        <div>
          <h2 class="text-3xl font-bold sm:text-4xl">Про нас</h2>
          <div class="mt-4 h-1 w-14 rounded-full bg-primary" />
          <p class="mt-6 text-lg text-muted-foreground">
            Ми — ваш надійний постачальник будівельних матеріалів у Чугуєві. Маємо в наявності
            водостічні системи <strong class="font-semibold text-foreground">Rainway</strong>.
            Працюємо з програмою
            <strong class="font-semibold text-foreground">Е-Відновлення</strong>.
          </p>
        </div>
        <PhotoCarousel :images="images" label="Фото магазину" />
      </div>

      <ul class="mt-14 grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <li
          v-for="feature in features"
          :key="feature.title"
          class="rounded-xl border bg-card p-6 text-card-foreground"
        >
          <div class="flex size-11 items-center justify-center rounded-lg bg-primary/15 text-brand">
            <component :is="feature.icon" class="size-6" aria-hidden="true" />
          </div>
          <h3 class="mt-4 text-lg font-semibold">{{ feature.title }}</h3>
          <ul class="mt-3 space-y-1.5 text-sm text-muted-foreground">
            <li v-for="item in feature.items" :key="item" class="flex gap-2">
              <span class="mt-2 size-1.5 shrink-0 rounded-full bg-primary" aria-hidden="true" />
              {{ item }}
            </li>
          </ul>
        </li>
      </ul>
    </div>
  </section>
</template>
