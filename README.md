# Budmaster

Система обліку будматеріалів і публічний сайт-каталог.

| Папка | Що всередині | Стан |
|-------|--------------|------|
| `site/` | Публічний сайт: Vue 3, Vite, Tailwind CSS, shadcn-vue | каталог працює на демонстраційних даних |
| `backend/` | Сервер: Rust, Axum, SQLx, PostgreSQL | ще не розпочато |
| `android/` | Застосунок обліку: Java, Room | ще не розпочато |
| `docs/` | Проєкт системи: [design.md](docs/design.md) | узгоджено |

## Сайт

```sh
cd site
npm install
npm run dev
```

Збірка: `npm run build`, результат у `site/dist`.

Поки сервера немає, сайт бере товари з `site/src/api/mock`. Щоб перейти на справжній сервер,
задайте в `site/.env`:

```
VITE_USE_MOCKS=false
VITE_API_URL=https://адреса-сервера
```
