<script setup>
import { RouterLink } from 'vue-router'
import { Button } from '@/components/ui/button'
</script>

<template>
  <div class="container-page py-24 text-center">
    <h1 class="text-4xl font-bold">Сторінку не знайдено</h1>
    <p class="mt-3 text-muted-foreground">Можливо, посилання застаріло.</p>
    <div class="mt-8 flex justify-center gap-3">
      <Button as-child><RouterLink to="/">На головну</RouterLink></Button>
    </div>
  </div>
</template>
