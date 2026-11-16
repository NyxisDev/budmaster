<script setup>
import { Clock, MapPin, Phone } from '@lucide/vue'
import PhotoCarousel from './PhotoCarousel.vue'
import { company } from '@/content/company'

const wayImages = ['way1', 'way2', 'way3'].map(
  (name) => new URL(`../assets/img/${name}.webp`, import.meta.url).href,
)
</script>

<template>
  <section id="contact" class="border-t py-20">
    <div class="container-page">
      <h2 class="text-3xl font-bold sm:text-4xl">Зв’язатися з нами</h2>
      <div class="mt-4 h-1 w-14 rounded-full bg-primary" />

      <div class="mt-10 grid gap-8 lg:grid-cols-2">
        <div class="flex flex-col gap-6">
          <dl class="divide-y rounded-xl border bg-card text-card-foreground">
            <div class="flex gap-4 p-5">
              <MapPin class="mt-0.5 size-5 shrink-0 text-brand" aria-hidden="true" />
              <div>
                <dt class="text-xs font-semibold tracking-wider text-muted-foreground uppercase">
                  Адреса
                </dt>
                <dd class="mt-1 text-base font-medium">{{ company.address }}</dd>
              </div>
            </div>
            <div class="flex gap-4 p-5">
              <Phone class="mt-0.5 size-5 shrink-0 text-brand" aria-hidden="true" />
              <div>
                <dt class="text-xs font-semibold tracking-wider text-muted-foreground uppercase">
                  Телефон
                </dt>
                <dd class="mt-1 flex flex-col gap-1">
                  <a
                    v-for="phone in company.phones"
                    :key="phone.href"
                    :href="phone.href"
                    class="text-base font-medium tabular-nums hover:text-brand"
                  >
                    {{ phone.label }}
                  </a>
                </dd>
              </div>
            </div>
            <div class="flex gap-4 p-5">
              <Clock class="mt-0.5 size-5 shrink-0 text-brand" aria-hidden="true" />
              <div>
                <dt class="text-xs font-semibold tracking-wider text-muted-foreground uppercase">
                  Графік роботи
                </dt>
                <dd class="mt-1 text-base font-medium">{{ company.hours }}</dd>
              </div>
            </div>
          </dl>

          <div class="overflow-hidden rounded-xl border">
            <iframe
              title="Budmaster на карті"
              :src="company.mapEmbedUrl"
              class="block h-72 w-full border-0"
              allowfullscreen
              loading="lazy"
              referrerpolicy="no-referrer-when-downgrade"
            />
          </div>
        </div>

        <div>
          <h3 class="mb-3 text-lg font-semibold">Як до нас дістатися</h3>
          <PhotoCarousel :images="wayImages" label="Дорога до магазину" />
        </div>
      </div>
    </div>
  </section>
</template>
