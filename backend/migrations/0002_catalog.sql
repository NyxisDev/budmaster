-- Каталог: единицы измерения, категории, товары, фото.
-- Схема описана в docs/design.md, раздел 4.2.

CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- ---------------------------------------------------------------------------
-- Единицы измерения — справочник. Названия на украинском, как весь каталог.
-- ---------------------------------------------------------------------------
CREATE TABLE units (
    code            text PRIMARY KEY CHECK (code ~ '^[a-z0-9_]{1,16}$'),
    name            text NOT NULL,
    short_name      text NOT NULL,
    -- можно ли продавать дробное количество (кг, м, м², м³ — да; штуки — нет)
    allow_fraction  boolean NOT NULL
);

INSERT INTO units (code, name, short_name, allow_fraction) VALUES
    ('pcs',   'штука',            'шт',    false),
    ('bag',   'мішок',            'мішок', false),
    ('sheet', 'лист',             'лист',  false),
    ('roll',  'рулон',            'рулон', false),
    ('set',   'комплект',         'компл', false),
    ('kg',    'кілограм',         'кг',    true),
    ('t',     'тонна',            'т',     true),
    ('l',     'літр',             'л',     true),
    ('m',     'метр',             'м',     true),
    ('m2',    'квадратний метр',  'м²',    true),
    ('m3',    'кубічний метр',    'м³',    true);

-- ---------------------------------------------------------------------------
-- Категории: дерево любой глубины. Удаляются мягко.
-- ---------------------------------------------------------------------------
CREATE TABLE categories (
    id          uuid PRIMARY KEY,
    parent_id   uuid REFERENCES categories (id),
    name        text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 100),
    -- часть адреса страницы на сайте: /catalog?category=<slug>
    slug        text NOT NULL UNIQUE CHECK (slug ~ '^[a-z0-9]+(-[a-z0-9]+)*$' AND length(slug) <= 80),
    sort_order  int NOT NULL DEFAULT 0,
    is_deleted  boolean NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    change_seq  bigint NOT NULL,
    CHECK (parent_id IS NULL OR parent_id <> id)
);
CREATE INDEX categories_parent_idx ON categories (parent_id);
CREATE INDEX categories_change_seq_idx ON categories (change_seq);
CREATE TRIGGER categories_change_seq BEFORE INSERT OR UPDATE ON categories
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

-- ---------------------------------------------------------------------------
-- Товары. Варианты (размер, цвет) — отдельные товары. Удаляются мягко.
-- ---------------------------------------------------------------------------
CREATE TABLE products (
    id                uuid PRIMARY KEY,
    category_id       uuid NOT NULL REFERENCES categories (id),
    name              text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 200),
    description       text NOT NULL DEFAULT '' CHECK (length(description) <= 5000),
    unit_code         text NOT NULL REFERENCES units (code),
    price             numeric(12,2) NOT NULL CHECK (price >= 0),
    -- фасовка: «в пачке N штук» и цена пачки; оба поля либо заданы, либо нет
    pack_size         int CHECK (pack_size > 1),
    pack_price        numeric(12,2) CHECK (pack_price >= 0),
    is_published      boolean NOT NULL DEFAULT true,
    is_deleted        boolean NOT NULL DEFAULT false,
    -- время последней правки каждого поля: нужно, чтобы при одновременной правке
    -- двумя людьми побеждала последняя по каждому полю отдельно (этап синхронизации)
    field_updated_at  jsonb NOT NULL DEFAULT '{}',
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    change_seq        bigint NOT NULL,
    CONSTRAINT products_pack_both_or_none CHECK ((pack_size IS NULL) = (pack_price IS NULL))
);
CREATE INDEX products_change_seq_idx ON products (change_seq);
-- то, что видит сайт
CREATE INDEX products_public_idx ON products (category_id, name, id) WHERE NOT is_deleted AND is_published;
-- поиск по части слова
CREATE INDEX products_name_trgm_idx ON products USING gin (name gin_trgm_ops);
CREATE INDEX products_description_trgm_idx ON products USING gin (description gin_trgm_ops);
CREATE TRIGGER products_change_seq BEFORE INSERT OR UPDATE ON products
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

-- ---------------------------------------------------------------------------
-- Фото товаров. В базе — только имена файлов; сами файлы лежат на диске сервера.
-- ---------------------------------------------------------------------------
CREATE TABLE product_images (
    id          uuid PRIMARY KEY,
    product_id  uuid NOT NULL REFERENCES products (id),
    -- имена задаёт сервер: <uuid>.jpg и <uuid>_thumb.jpg
    file_name   text NOT NULL CHECK (file_name ~ '^[0-9a-f-]{36}\.jpg$'),
    thumb_name  text NOT NULL CHECK (thumb_name ~ '^[0-9a-f-]{36}_thumb\.jpg$'),
    width       int NOT NULL CHECK (width > 0),
    height      int NOT NULL CHECK (height > 0),
    sort_order  int NOT NULL DEFAULT 0,
    is_deleted  boolean NOT NULL DEFAULT false,
    created_by  uuid REFERENCES users (id),
    created_at  timestamptz NOT NULL DEFAULT now(),
    change_seq  bigint NOT NULL
);
CREATE INDEX product_images_product_idx ON product_images (product_id, sort_order, created_at);
CREATE INDEX product_images_change_seq_idx ON product_images (change_seq);
CREATE TRIGGER product_images_change_seq BEFORE INSERT OR UPDATE ON product_images
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

-- ---------------------------------------------------------------------------
-- Наличие для сайта: 'in_stock' | 'coming_soon' | 'out_of_stock'.
-- Единственное место, где оно вычисляется. Остатков и поставок пока нет (они появятся
-- на этапе склада), поэтому сейчас у всех товаров один ответ. Функцию заменит
-- миграция этапа склада; код сервера и сайт при этом не меняются.
-- ---------------------------------------------------------------------------
CREATE FUNCTION product_availability(product uuid) RETURNS text
LANGUAGE sql STABLE AS $$
    SELECT 'out_of_stock'::text
$$;

-- ---------------------------------------------------------------------------
-- Права роли сервера: читать, добавлять, менять. Удалять нельзя ничего.
-- ---------------------------------------------------------------------------
GRANT SELECT ON units TO budmaster_app;
GRANT SELECT, INSERT, UPDATE ON categories, products, product_images TO budmaster_app;
GRANT EXECUTE ON FUNCTION product_availability(uuid) TO budmaster_app;
