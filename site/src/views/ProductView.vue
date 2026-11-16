<script setup>
import { computed, ref, watch } from 'vue'
import { RouterLink } from 'vue-router'
import { Check, ShoppingCart } from '@lucide/vue'
import AvailabilityBadge from '@/components/catalog/AvailabilityBadge.vue'
import ProductPhoto from '@/components/catalog/ProductPhoto.vue'
import QuantityInput from '@/components/catalog/QuantityInput.vue'
import { Button } from '@/components/ui/button'
import { Label } from '@/components/ui/label'
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group'
import { Skeleton } from '@/components/ui/skeleton'
import { getProduct } from '@/api'
import { formatPrice, formatQty } from '@/lib/format'
import { useCartStore } from '@/stores/cart'
import { useCategoriesStore } from '@/stores/categories'

const props = defineProps({
  id: { type: String, required: true },
})

const cart = useCartStore()
const categories = useCategoriesStore()

const product = ref(null)
const status = ref('loading') // loading | ready | not_found | error
const activeImage = ref(0)
const stockUnit = ref('unit')
const qty = ref(1)
const justAdded = ref(false)
const cartFull = ref(false)

let requestId = 0
async function load() {
  const current = ++requestId
  status.value = 'loading'
  justAdded.value = false
  try {
    const [loaded] = await Promise.all([getProduct(props.id), categories.load()])
    if (current !== requestId) return
    product.value = loaded
    activeImage.value = 0
    stockUnit.value = 'unit'
    qty.value = 1
    status.value = 'ready'
    document.title = `${loaded.name} — Budmaster`
  } catch (error) {
    if (current !== requestId) return
    status.value = error?.status === 404 ? 'not_found' : 'error'
    document.title = 'Товар — Budmaster'
  }
}

watch(() => props.id, load, { immediate: true })

const isPack = computed(() => stockUnit.value === 'pack')
const allowFraction = computed(() => !isPack.value && product.value.unit.allow_fraction)
const unitPrice = computed(() =>
  Number(isPack.value ? product.value.pack.price : product.value.price),
)
// Скорочення не відмінюється: «2 пач.», «5 пач.» — без помилок у закінченнях.
const qtyUnit = computed(() => (isPack.value ? 'пач.' : product.value.unit.short_name))
const breadcrumbs = computed(() => categories.pathTo(product.value?.category_id))

// При перемиканні «поштучно / пачкою» кількість скидається: 0,5 пачки не буває.
watch(stockUnit, () => {
  qty.value = 1
  justAdded.value = false
})
watch(qty, () => (justAdded.value = false))

function addToCart() {
  justAdded.value = cart.add(product.value, stockUnit.value, qty.value)
  cartFull.value = !justAdded.value
}
</script>

<template>
  <div class="container-page py-8">
    <div v-if="status === 'loading'" class="grid gap-8 lg:grid-cols-2" aria-busy="true">
      <Skeleton class="aspect-[4/3] rounded-xl" />
      <div class="space-y-4">
        <Skeleton class="h-10 w-3/4" />
        <Skeleton class="h-6 w-40" />
        <Skeleton class="h-12 w-48" />
        <Skeleton class="h-24" />
      </div>
    </div>

    <div v-else-if="status === 'not_found'" class="py-16 text-center">
      <h1 class="text-3xl font-bold">Товар не знайдено</h1>
      <p class="mt-2 text-muted-foreground">Можливо, його прибрали з каталогу.</p>
      <Button class="mt-6" as-child><RouterLink to="/catalog">Перейти до каталогу</RouterLink></Button>
    </div>

    <div v-else-if="status === 'error'" class="py-16 text-center">
      <h1 class="text-3xl font-bold">Не вдалося завантажити товар</h1>
      <p class="mt-2 text-muted-foreground">Перевірте з’єднання з інтернетом і спробуйте ще раз.</p>
      <Button class="mt-6" @click="load">Спробувати ще раз</Button>
    </div>

    <template v-else>
      <nav class="text-sm text-muted-foreground" aria-label="Хлібні крихти">
        <RouterLink to="/" class="hover:text-foreground">Головна</RouterLink>
        <span class="mx-2" aria-hidden="true">/</span>
        <RouterLink to="/catalog" class="hover:text-foreground">Каталог</RouterLink>
        <template v-for="category in breadcrumbs" :key="category.id">
          <span class="mx-2" aria-hidden="true">/</span>
          <RouterLink
            :to="{ name: 'catalog', query: { category: category.slug } }"
            class="hover:text-foreground"
          >
            {{ category.name }}
          </RouterLink>
        </template>
      </nav>

      <div class="mt-6 grid gap-8 lg:grid-cols-2">
        <div>
          <div class="overflow-hidden rounded-xl border">
            <ProductPhoto :src="product.images[activeImage]?.url" :alt="product.name" />
          </div>
          <ul v-if="product.images.length > 1" class="mt-3 flex gap-2">
            <li v-for="(image, index) in product.images" :key="image.url">
              <button
                type="button"
                class="size-16 overflow-hidden rounded-md border-2"
                :class="index === activeImage ? 'border-primary' : 'border-transparent'"
                :aria-label="`Фото ${index + 1}`"
                :aria-current="index === activeImage"
                @click="activeImage = index"
              >
                <img :src="image.thumb_url" alt="" class="h-full w-full object-cover" />
              </button>
            </li>
          </ul>
        </div>

        <div>
          <h1 class="text-3xl leading-tight font-bold sm:text-4xl">{{ product.name }}</h1>
          <div class="mt-3">
            <AvailabilityBadge :availability="product.availability" />
          </div>

          <p class="mt-6 font-heading text-4xl font-bold tabular-nums">
            {{ formatPrice(product.price) }}
            <span class="text-lg font-normal text-muted-foreground">
              / {{ product.unit.short_name }}
            </span>
          </p>

          <fieldset v-if="product.pack" class="mt-6">
            <legend class="mb-2 text-sm font-medium">Як купувати</legend>
            <RadioGroup v-model="stockUnit" class="gap-2">
              <Label
                class="flex cursor-pointer items-center gap-3 rounded-lg border p-3 font-normal has-data-[state=checked]:border-primary"
              >
                <RadioGroupItem value="unit" />
                <span class="flex-1">Поштучно</span>
                <span class="font-semibold tabular-nums">
                  {{ formatPrice(product.price) }} / {{ product.unit.short_name }}
                </span>
              </Label>
              <Label
                class="flex cursor-pointer items-center gap-3 rounded-lg border p-3 font-normal has-data-[state=checked]:border-primary"
              >
                <RadioGroupItem value="pack" />
                <span class="flex-1">
                  Пачкою ({{ product.pack.size }} {{ product.unit.short_name }})
                </span>
                <span class="font-semibold tabular-nums">
                  {{ formatPrice(product.pack.price) }} / пачка
                </span>
              </Label>
            </RadioGroup>
          </fieldset>

          <div class="mt-6 flex flex-wrap items-end gap-4">
            <div>
              <p class="mb-2 text-sm font-medium">Кількість ({{ qtyUnit }})</p>
              <QuantityInput
                v-model="qty"
                :allow-fraction="allowFraction"
                :label="`Кількість (${qtyUnit})`"
              />
            </div>
            <div>
              <p class="text-sm text-muted-foreground">Разом</p>
              <p class="font-heading text-2xl font-bold tabular-nums">
                {{ formatPrice(unitPrice * qty) }}
              </p>
            </div>
          </div>

          <div class="mt-6 flex flex-wrap items-center gap-3">
            <Button size="lg" class="h-12 px-7 text-base" @click="addToCart">
              <ShoppingCart />
              Додати в кошик
            </Button>
            <Button v-if="justAdded" size="lg" variant="outline" class="h-12 text-base" as-child>
              <RouterLink to="/cart">Перейти до кошика</RouterLink>
            </Button>
          </div>

          <p class="mt-3 min-h-6 text-sm text-success" role="status">
            <template v-if="justAdded">
              <Check class="mr-1 inline size-4" aria-hidden="true" />
              Додано в кошик: {{ formatQty(qty) }} {{ qtyUnit }}
            </template>
          </p>

          <p v-if="cartFull" class="mt-2 text-sm text-destructive" role="alert">
            У кошику вже 100 позицій — це максимум для однієї заявки. Оформіть її або приберіть
            зайве.
          </p>

          <p
            v-if="product.availability !== 'in_stock'"
            class="mt-2 rounded-lg border bg-muted p-3 text-sm"
          >
            Товару зараз немає на складі. Заявку можна залишити — менеджер зателефонує,
            уточнить термін або запропонує заміну.
          </p>

          <div v-if="product.description" class="mt-8 border-t pt-6">
            <h2 class="text-lg font-semibold">Опис</h2>
            <p class="mt-2 whitespace-pre-line text-muted-foreground">{{ product.description }}</p>
          </div>
        </div>
      </div>
    </template>
  </div>
</template>
