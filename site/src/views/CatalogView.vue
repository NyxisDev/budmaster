<script setup>
import { computed, onMounted, ref, watch } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { ChevronLeft, ChevronRight, ListFilter, Search, X } from '@lucide/vue'
import CategoryNav from '@/components/catalog/CategoryNav.vue'
import ProductCard from '@/components/catalog/ProductCard.vue'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from '@/components/ui/sheet'
import { Skeleton } from '@/components/ui/skeleton'
import { getProducts } from '@/api'
import { useCategoriesStore } from '@/stores/categories'

const PER_PAGE = 12

const route = useRoute()
const router = useRouter()
const categories = useCategoriesStore()

const products = ref([])
const total = ref(0)
const status = ref('loading') // loading | ready | error | no_category
const filtersOpen = ref(false)

// Стан каталогу живе в адресі сторінки: посиланням можна поділитися, «назад» працює.
// ?category=a&category=b дає масив — беремо перше значення.
const first = (value) => String((Array.isArray(value) ? value[0] : value) ?? '')
const activeSlug = computed(() => first(route.query.category))
const search = computed(() => first(route.query.q))
const page = computed(() => Math.max(parseInt(first(route.query.page), 10) || 1, 1))
const pageCount = computed(() => Math.max(Math.ceil(total.value / PER_PAGE), 1))
const activeCategory = computed(() => categories.bySlug(activeSlug.value))

const searchDraft = ref(search.value)
watch(search, (value) => (searchDraft.value = value))

let requestId = 0
async function load() {
  const current = ++requestId
  status.value = 'loading'
  try {
    await categories.load()
    if (current !== requestId) return
    // Без категорій не можна чесно відфільтрувати товари: краще показати помилку, ніж увесь каталог.
    if (categories.status !== 'ready') throw new Error('categories')
    if (activeSlug.value && !activeCategory.value) {
      status.value = 'no_category'
      return
    }
    const response = await getProducts({
      category_id: activeCategory.value?.id,
      q: search.value || undefined,
      page: page.value,
      per_page: PER_PAGE,
    })
    if (current !== requestId) return // відповідь на застарілий запит
    // Сторінки з таким номером уже немає (товарів стало менше) — відкриваємо останню.
    const lastPage = Math.max(Math.ceil(response.total / PER_PAGE), 1)
    if (page.value > lastPage) {
      router.replace({ name: 'catalog', query: buildQuery({ page: lastPage > 1 ? lastPage : undefined }) })
      return
    }
    products.value = response.items
    total.value = response.total
    status.value = 'ready'
  } catch {
    if (current === requestId) status.value = 'error'
  }
}

onMounted(load)
watch(() => [activeSlug.value, search.value, page.value], load)

function buildQuery(overrides) {
  const query = {
    category: activeSlug.value || undefined,
    q: search.value || undefined,
    page: page.value > 1 ? page.value : undefined,
    ...overrides,
  }
  return Object.fromEntries(Object.entries(query).filter(([, value]) => value))
}

function submitSearch() {
  router.push({ name: 'catalog', query: buildQuery({ q: searchDraft.value.trim(), page: undefined }) })
}

function clearSearch() {
  searchDraft.value = ''
  submitSearch()
}

const title = computed(() => activeCategory.value?.name ?? 'Каталог')

watch(
  title,
  (value) => {
    document.title = `${value} — Budmaster`
  },
  { immediate: true },
)
</script>

<template>
  <div class="container-page py-8">
    <nav class="text-sm text-muted-foreground" aria-label="Хлібні крихти">
      <RouterLink to="/" class="hover:text-foreground">Головна</RouterLink>
      <span class="mx-2" aria-hidden="true">/</span>
      <RouterLink v-if="activeCategory" to="/catalog" class="hover:text-foreground">Каталог</RouterLink>
      <span v-else class="text-foreground">Каталог</span>
      <template v-if="activeCategory">
        <span class="mx-2" aria-hidden="true">/</span>
        <span class="text-foreground">{{ activeCategory.name }}</span>
      </template>
    </nav>

    <h1 class="mt-3 text-3xl font-bold sm:text-4xl">{{ title }}</h1>

    <div class="mt-6 flex flex-col gap-2 sm:flex-row">
      <form class="relative flex-1" role="search" @submit.prevent="submitSearch">
        <Search
          class="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
          aria-hidden="true"
        />
        <Input
          v-model="searchDraft"
          type="search"
          placeholder="Пошук товарів"
          maxlength="100"
          aria-label="Пошук товарів"
          class="h-11 pr-24 pl-9 text-base"
          enterkeyhint="search"
        />
        <div class="absolute top-1/2 right-1.5 flex -translate-y-1/2 gap-1">
          <Button
            v-if="searchDraft"
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="Очистити пошук"
            @click="clearSearch"
          >
            <X />
          </Button>
          <Button type="submit" size="sm">Знайти</Button>
        </div>
      </form>

      <Sheet v-model:open="filtersOpen">
        <SheetTrigger as-child>
          <Button variant="outline" class="h-11 lg:hidden">
            <ListFilter />
            Категорії
          </Button>
        </SheetTrigger>
        <SheetContent side="left" class="overflow-y-auto p-6">
          <SheetHeader class="p-0">
            <SheetTitle>Категорії</SheetTitle>
            <SheetDescription class="sr-only">Оберіть категорію товарів</SheetDescription>
          </SheetHeader>
          <CategoryNav
            :tree="categories.tree"
            :active-slug="activeSlug"
            :search="search"
            @navigate="filtersOpen = false"
          />
        </SheetContent>
      </Sheet>
    </div>

    <div class="mt-8 grid gap-8 lg:grid-cols-[15rem_1fr]">
      <aside class="hidden lg:block">
        <div class="sticky top-24">
          <h2 class="mb-2 px-3 text-xs font-semibold tracking-wider text-muted-foreground uppercase">
            Категорії
          </h2>
          <p v-if="categories.status === 'error'" class="px-3 text-sm text-muted-foreground">
            Категорії не завантажилися.
          </p>
          <div v-else-if="categories.status !== 'ready'" class="space-y-2 px-3">
            <Skeleton v-for="n in 7" :key="n" class="h-7" />
          </div>
          <CategoryNav v-else :tree="categories.tree" :active-slug="activeSlug" :search="search" />
        </div>
      </aside>

      <section :aria-busy="status === 'loading'">
        <div v-if="status === 'loading'" class="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
          <Skeleton v-for="n in 6" :key="n" class="h-80 rounded-xl" />
        </div>

        <div v-else-if="status === 'error'" class="rounded-xl border p-8 text-center">
          <p class="text-lg font-semibold">Не вдалося завантажити товари</p>
          <p class="mt-1 text-muted-foreground">Перевірте з’єднання з інтернетом і спробуйте ще раз.</p>
          <Button class="mt-4" @click="load">Спробувати ще раз</Button>
        </div>

        <div v-else-if="status === 'no_category'" class="rounded-xl border p-8 text-center">
          <p class="text-lg font-semibold">Такої категорії немає</p>
          <p class="mt-1 text-muted-foreground">Можливо, її перейменували або прибрали.</p>
          <Button variant="outline" class="mt-4" as-child>
            <RouterLink to="/catalog">Показати всі товари</RouterLink>
          </Button>
        </div>

        <div v-else-if="!products.length" class="rounded-xl border p-8 text-center" role="status">
          <p class="text-lg font-semibold">Нічого не знайдено</p>
          <p class="mt-1 text-muted-foreground">
            <template v-if="search">За запитом «{{ search }}» товарів немає. </template>
            Змініть запит або зателефонуйте нам — підкажемо, що є в наявності.
          </p>
          <Button variant="outline" class="mt-4" as-child>
            <RouterLink to="/catalog">Показати всі товари</RouterLink>
          </Button>
        </div>

        <template v-else>
          <p class="mb-4 text-sm text-muted-foreground" role="status">Знайдено товарів: {{ total }}</p>
          <ul class="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
            <li v-for="product in products" :key="product.id" class="flex">
              <ProductCard :product="product" class="w-full" />
            </li>
          </ul>

          <nav
            v-if="pageCount > 1"
            class="mt-8 flex items-center justify-center gap-3"
            aria-label="Сторінки каталогу"
          >
            <Button variant="outline" :disabled="page <= 1" as-child>
              <RouterLink
                v-if="page > 1"
                :to="{ name: 'catalog', query: buildQuery({ page: page - 1 > 1 ? page - 1 : undefined }) }"
              >
                <ChevronLeft />
                Назад
              </RouterLink>
              <span v-else class="pointer-events-none opacity-50"><ChevronLeft /> Назад</span>
            </Button>
            <span class="text-sm tabular-nums">Сторінка {{ page }} з {{ pageCount }}</span>
            <Button variant="outline" as-child>
              <RouterLink
                v-if="page < pageCount"
                :to="{ name: 'catalog', query: buildQuery({ page: page + 1 }) }"
              >
                Далі
                <ChevronRight />
              </RouterLink>
              <span v-else class="pointer-events-none opacity-50">Далі <ChevronRight /></span>
            </Button>
          </nav>
        </template>
      </section>
    </div>
  </div>
</template>
