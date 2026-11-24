<script setup>
// Логотип Budmaster (геометрія та кольори — з фірмових SVG основного проєкту budmaster/site/src/assets/brand).
// Вбудований у сторінку, а не через <img>: напис бере шрифт Outfit із файлу нижче.
// «Bud» бере колір тексту, тому один компонент покриває світлу й темну версії логотипа.
</script>

<template>
  <svg
    xmlns="http://www.w3.org/2000/svg"
    viewBox="0 0 360 64"
    role="img"
    aria-label="Budmaster"
    class="h-8 w-auto text-foreground"
  >
    <g fill="#FF8A00" transform="translate(10 0)">
      <path d="M32 7 59 28 54 34 32 17 10 34 5 28Z" />
      <rect x="13" y="34" width="17" height="9" />
      <rect x="34" y="34" width="17" height="9" />
      <rect x="13" y="47" width="10" height="10" />
      <rect x="27" y="47" width="24" height="10" />
    </g>
    <text
      x="90"
      y="49"
      font-family="'Outfit Logo', Outfit, Inter, Arial, sans-serif"
      font-size="48"
      font-weight="700"
      letter-spacing="-0.02em"
    >
      <tspan fill="currentColor">Bud</tspan><tspan fill="#FF8A00">master</tspan>
    </text>
  </svg>
</template>
