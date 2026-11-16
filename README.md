# Budmaster

Система обліку будматеріалів і публічний сайт-каталог.

| Папка | Що всередині | Стан |
|-------|--------------|------|
| `site/` | Публічний сайт: Vue 3, Vite, Tailwind CSS, shadcn-vue | каталог працює на демонстраційних даних |
| `backend/` | Сервер: Rust, Axum, SQLx, PostgreSQL | основа: вхід, співробітники, права, журнал дій |
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

## Сервер

Потрібен лише Docker. Rust на комп’ютері не потрібен.

```sh
cp .env.example .env        # і замініть значення на свої
docker compose up -d --build
```

Сервер відповідає на http://127.0.0.1:8080 (перевірка: `/health`).

Перший запуск — створіть склад і власника. Пароль власника команда запитає в терміналі:

```sh
docker compose run --rm backend create-warehouse --name "Склад 1"
docker compose run --rm backend create-owner --login ivan --first-name Іван --last-name Петренко
```

Тести й перевірка коду (запускаються в контейнері, потрібна запущена база):

```sh
PW=$(grep '^POSTGRES_PASSWORD=' .env | cut -d= -f2)
CARGO_NETWORK=budmaster_default TEST_DATABASE_URL="postgres://budmaster_owner:$PW@postgres:5432/budmaster" ./dev-cargo.sh test
```
