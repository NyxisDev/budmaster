<script setup>
import { nextTick, onMounted, reactive, ref, watch } from 'vue'
import { RouterLink } from 'vue-router'
import { CircleCheck, Trash2 } from '@lucide/vue'
import ProductPhoto from '@/components/catalog/ProductPhoto.vue'
import QuantityInput from '@/components/catalog/QuantityInput.vue'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group'
import { Textarea } from '@/components/ui/textarea'
import { createOrder, getProduct, isDemo } from '@/api'
import { formatPrice } from '@/lib/format'
import { normalizePhone } from '@/lib/phone'
import { newUuid } from '@/lib/uuid'
import { useCartStore } from '@/stores/cart'
import { company } from '@/content/company'

const cart = useCartStore()

const form = reactive({
  customer_name: '',
  customer_phone: '',
  delivery_type: 'pickup',
  delivery_address: '',
  comment: '',
  website: '', // поле-пастка для ботів, людина його не бачить
})
const errors = reactive({})
const formError = ref('')
const cartNotice = ref('')
const submitting = ref(false)
const done = ref(null) // { number, total } після успішної заявки

// Один id на один склад кошика. Якщо зв’язок обірвався й людина натиснула кнопку ще раз,
// сервер упізнає повтор і не створить другу заявку. Якщо ж кошик змінили — це вже інша
// заявка з новим id, інакше сервер повернув би стару з іншим складом.
let orderId = newUuid()
watch(
  () => cart.lines.map((line) => `${line.key}=${line.qty}`).join('|'),
  () => {
    orderId = newUuid()
  },
)
const openedAt = Date.now()

const unitLabel = (line) => (line.stock_unit === 'pack' ? 'пачка' : line.product.unit.short_name)

// Ціни й наявність у кошику могли застаріти — звіряємо із сервером і кажемо, що змінилося.
async function refreshCart() {
  const ids = [...new Set(cart.lines.map((line) => line.product.id))]
  let removed = 0
  let priceChanged = 0
  let failed = false
  await Promise.all(
    ids.map(async (id) => {
      try {
        const result = cart.refreshProduct(id, await getProduct(id))
        removed += result.removed
        priceChanged += result.priceChanged
      } catch (error) {
        if (error?.status === 404) removed += cart.refreshProduct(id, null).removed
        else failed = true
      }
    }),
  )
  const parts = []
  if (removed) parts.push(`Прибрано позицій, яких більше немає в каталозі: ${removed}.`)
  if (priceChanged) parts.push(`Оновлено ціну в позиціях: ${priceChanged}.`)
  if (failed) parts.push('Не вдалося перевірити актуальні ціни — вони можуть відрізнятися.')
  cartNotice.value = parts.join(' ')
}

onMounted(refreshCart)

const MESSAGES = {
  customer_name: 'Вкажіть ім’я (від 2 символів).',
  customer_phone: 'Вкажіть український номер, наприклад 050 123 45 67.',
  delivery_address: 'Вкажіть адресу доставки.',
  comment: 'Коментар задовгий (до 1000 символів).',
}

function clearErrors() {
  for (const key of Object.keys(errors)) delete errors[key]
}

function focusFirstError() {
  const first = Object.keys(errors)[0]
  nextTick(() => document.getElementById(`order-${first}`)?.focus())
}

function validate() {
  clearErrors()
  const name = form.customer_name.trim()
  if (name.length < 2 || name.length > 100) errors.customer_name = MESSAGES.customer_name
  if (!normalizePhone(form.customer_phone)) errors.customer_phone = MESSAGES.customer_phone
  if (form.delivery_type === 'delivery' && !form.delivery_address.trim()) {
    errors.delivery_address = MESSAGES.delivery_address
  }
  if (form.comment.length > 1000) errors.comment = MESSAGES.comment
  return Object.keys(errors).length === 0
}

async function submit() {
  formError.value = ''
  if (!validate()) {
    focusFirstError()
    return
  }

  // Необов’язкові поля не передаємо зовсім, якщо вони порожні.
  const payload = {
    id: orderId,
    customer_name: form.customer_name.trim(),
    customer_phone: normalizePhone(form.customer_phone),
    delivery_type: form.delivery_type,
    items: cart.lines.map((line) => ({
      product_id: line.product.id,
      stock_unit: line.stock_unit,
      qty: line.qty,
    })),
    website: form.website,
    elapsed_ms: Date.now() - openedAt,
  }
  if (form.comment.trim()) payload.comment = form.comment.trim()
  if (form.delivery_type === 'delivery') payload.delivery_address = form.delivery_address.trim()

  submitting.value = true
  try {
    done.value = await createOrder(payload)
    cart.clear()
    window.scrollTo({ top: 0 })
  } catch (error) {
    await handleSubmitError(error)
  } finally {
    submitting.value = false
  }
}

async function handleSubmitError(error) {
  const fields = Object.keys(error?.fields ?? {})
  if (error?.status === 422) {
    clearErrors()
    for (const field of fields) {
      if (MESSAGES[field]) errors[field] = MESSAGES[field]
    }
    if (Object.keys(errors).length) {
      formError.value = 'Перевірте виділені поля.'
      focusFirstError()
    } else if (fields.includes('items')) {
      // Товар зник із каталогу або змінилися правила кількості — оновлюємо кошик.
      await refreshCart()
      formError.value = 'Деякі товари в кошику змінилися. Перевірте кошик і надішліть заявку ще раз.'
    } else if (fields.includes('elapsed_ms')) {
      formError.value = 'Заявку надіслано надто швидко. Зачекайте кілька секунд і спробуйте ще раз.'
    } else {
      formError.value =
        'Не вдалося оформити заявку. Оновіть сторінку та спробуйте ще раз або зателефонуйте нам.'
    }
  } else if (error?.status === 429) {
    formError.value = 'Забагато заявок за короткий час. Спробуйте пізніше або зателефонуйте нам.'
  } else {
    formError.value =
      'Заявку не надіслано: немає зв’язку із сервером. Перевірте інтернет і натисніть кнопку ще раз.'
  }
}
</script>

<template>
  <div class="container-page py-8">
    <div v-if="done" class="mx-auto max-w-xl py-12 text-center" role="status">
      <CircleCheck class="mx-auto size-14 text-success" aria-hidden="true" />
      <h1 class="mt-4 text-3xl font-bold">Заявку №{{ done.number }} прийнято</h1>
      <p v-if="isDemo" class="mt-3 rounded-lg border border-destructive p-3 text-destructive">
        Це демонстраційна версія сайту: заявку нікуди не надіслано. Щоб замовити, зателефонуйте нам.
      </p>
      <p class="mt-3 text-lg text-muted-foreground">
        Сума замовлення — {{ formatPrice(done.total) }}. Ми зателефонуємо вам, щоб підтвердити
        замовлення та узгодити отримання й оплату.
      </p>
      <p class="mt-2 text-muted-foreground">Графік роботи: {{ company.hours }}.</p>
      <Button class="mt-8" size="lg" as-child>
        <RouterLink to="/catalog">Повернутися до каталогу</RouterLink>
      </Button>
    </div>

    <div v-else-if="!cart.lines.length" class="py-16 text-center">
      <h1 class="text-3xl font-bold">Кошик порожній</h1>
      <p class="mt-2 text-muted-foreground">Додайте товари з каталогу, щоб оформити заявку.</p>
      <Button class="mt-6" size="lg" as-child>
        <RouterLink to="/catalog">Перейти до каталогу</RouterLink>
      </Button>
    </div>

    <template v-else>
      <h1 class="text-3xl font-bold sm:text-4xl">Кошик</h1>

      <div class="mt-8 grid gap-10 lg:grid-cols-[1fr_24rem]">
        <section aria-label="Товари в кошику">
          <p v-if="cartNotice" class="mb-4 rounded-lg border bg-muted p-3 text-sm" role="status">
            {{ cartNotice }}
          </p>
          <ul class="divide-y rounded-xl border bg-card text-card-foreground">
            <li v-for="line in cart.lines" :key="line.key" class="flex gap-4 p-4">
              <div class="w-20 shrink-0 overflow-hidden rounded-md sm:w-24">
                <ProductPhoto :src="line.product.images[0]?.thumb_url" :alt="line.product.name" compact />
              </div>
              <div class="flex min-w-0 flex-1 flex-col gap-2">
                <div class="flex items-start justify-between gap-2">
                  <RouterLink
                    :to="{ name: 'product', params: { id: line.product.id } }"
                    class="leading-snug font-semibold hover:text-brand"
                  >
                    {{ line.product.name }}
                  </RouterLink>
                  <Button
                    variant="ghost"
                    size="icon-sm"
                    :aria-label="`Прибрати з кошика: ${line.product.name}`"
                    @click="cart.remove(line.key)"
                  >
                    <Trash2 />
                  </Button>
                </div>
                <p class="text-sm text-muted-foreground tabular-nums">
                  {{ formatPrice(cart.unitPrice(line)) }} / {{ unitLabel(line) }}
                  <template v-if="line.stock_unit === 'pack'">
                    ({{ line.product.pack.size }} {{ line.product.unit.short_name }})
                  </template>
                </p>
                <div class="flex flex-wrap items-center justify-between gap-2">
                  <QuantityInput
                    :model-value="line.qty"
                    :allow-fraction="line.stock_unit === 'unit' && line.product.unit.allow_fraction"
                    :label="`Кількість (${unitLabel(line)}): ${line.product.name}`"
                    @update:model-value="cart.setQty(line.key, $event)"
                  />
                  <p class="font-heading text-lg font-bold tabular-nums">
                    {{ formatPrice(cart.lineTotal(line)) }}
                  </p>
                </div>
              </div>
            </li>
          </ul>

          <div class="mt-4 flex items-baseline justify-between rounded-xl border bg-card p-4">
            <span class="text-lg font-semibold">Разом</span>
            <span class="font-heading text-2xl font-bold tabular-nums">
              {{ formatPrice(cart.total) }}
            </span>
          </div>
          <p class="mt-2 text-sm text-muted-foreground">
            Остаточну суму й вартість доставки підтвердить менеджер телефоном.
          </p>
        </section>

        <section aria-labelledby="order-title">
          <h2 id="order-title" class="text-2xl font-bold">Оформлення заявки</h2>
          <p class="mt-1 text-sm text-muted-foreground">
            Без онлайн-оплати. Ми зателефонуємо, щоб підтвердити замовлення.
          </p>

          <form class="mt-6 space-y-5" novalidate @submit.prevent="submit">
            <div class="space-y-2">
              <Label for="order-customer_name">Ім’я</Label>
              <Input
                id="order-customer_name"
                v-model="form.customer_name"
                autocomplete="name"
                maxlength="100"
                required
                class="h-11 text-base"
                :aria-invalid="!!errors.customer_name"
                :aria-describedby="errors.customer_name ? 'order-customer_name-error' : undefined"
              />
              <p v-if="errors.customer_name" id="order-customer_name-error" class="text-sm text-destructive">
                {{ errors.customer_name }}
              </p>
            </div>

            <div class="space-y-2">
              <Label for="order-customer_phone">Телефон</Label>
              <Input
                id="order-customer_phone"
                v-model="form.customer_phone"
                type="tel"
                inputmode="tel"
                autocomplete="tel"
                required
                placeholder="050 123 45 67"
                class="h-11 text-base"
                :aria-invalid="!!errors.customer_phone"
                :aria-describedby="errors.customer_phone ? 'order-customer_phone-error' : undefined"
              />
              <p v-if="errors.customer_phone" id="order-customer_phone-error" class="text-sm text-destructive">
                {{ errors.customer_phone }}
              </p>
            </div>

            <fieldset class="space-y-2">
              <legend class="text-sm font-medium">Спосіб отримання</legend>
              <RadioGroup v-model="form.delivery_type" class="gap-2">
                <Label
                  class="flex cursor-pointer items-center gap-3 rounded-lg border p-3 font-normal has-data-[state=checked]:border-primary"
                >
                  <RadioGroupItem value="pickup" />
                  Самовивіз
                </Label>
                <Label
                  class="flex cursor-pointer items-center gap-3 rounded-lg border p-3 font-normal has-data-[state=checked]:border-primary"
                >
                  <RadioGroupItem value="delivery" />
                  Доставка
                </Label>
              </RadioGroup>
              <p v-if="form.delivery_type === 'pickup'" class="text-sm text-muted-foreground">
                {{ company.address }}, {{ company.hours }}
              </p>
            </fieldset>

            <div v-if="form.delivery_type === 'delivery'" class="space-y-2">
              <Label for="order-delivery_address">Адреса доставки</Label>
              <Input
                id="order-delivery_address"
                v-model="form.delivery_address"
                autocomplete="street-address"
                maxlength="300"
                required
                class="h-11 text-base"
                :aria-invalid="!!errors.delivery_address"
                :aria-describedby="errors.delivery_address ? 'order-delivery_address-error' : undefined"
              />
              <p v-if="errors.delivery_address" id="order-delivery_address-error" class="text-sm text-destructive">
                {{ errors.delivery_address }}
              </p>
            </div>

            <div class="space-y-2">
              <Label for="order-comment">Коментар <span class="font-normal text-muted-foreground">(необов’язково)</span></Label>
              <Textarea
                id="order-comment"
                v-model="form.comment"
                rows="3"
                maxlength="1000"
                class="text-base"
                :aria-invalid="!!errors.comment"
                :aria-describedby="errors.comment ? 'order-comment-error' : undefined"
              />
              <p v-if="errors.comment" id="order-comment-error" class="text-sm text-destructive">
                {{ errors.comment }}
              </p>
            </div>

            <!-- Поле-пастка: приховане від людей і від екранних читачів, боти його заповнюють. -->
            <div class="absolute -left-[9999px]" aria-hidden="true">
              <label for="order-website">Не заповнюйте це поле</label>
              <input id="order-website" v-model="form.website" type="text" tabindex="-1" autocomplete="off" />
            </div>

            <p v-if="formError" class="rounded-lg border border-destructive p-3 text-sm text-destructive" role="alert">
              {{ formError }}
            </p>

            <Button type="submit" size="lg" class="h-12 w-full text-base" :disabled="submitting">
              {{ submitting ? 'Надсилаємо…' : 'Надіслати заявку' }}
            </Button>
          </form>
        </section>
      </div>
    </template>
  </div>
</template>
