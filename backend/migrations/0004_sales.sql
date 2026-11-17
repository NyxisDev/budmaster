-- Заказы, продажи, возвраты. Схема описана в docs/design.md, раздел 4.4.

CREATE TYPE order_source AS ENUM ('site', 'manual');
CREATE TYPE order_status AS ENUM ('new', 'confirmed', 'assembled', 'completed', 'cancelled');
CREATE TYPE delivery_type AS ENUM ('pickup', 'delivery');
CREATE TYPE sale_channel AS ENUM ('site', 'manual', 'pos');
CREATE TYPE payment_type AS ENUM ('cash', 'terminal', 'card_transfer', 'bank_transfer', 'e_vidnovlennia');

-- ---------------------------------------------------------------------------
-- Заказы: с сайта и внесённые работником вручную.
-- ---------------------------------------------------------------------------
CREATE SEQUENCE order_number_seq START 1001;

CREATE TABLE orders (
    id                uuid PRIMARY KEY,
    number            bigint NOT NULL UNIQUE DEFAULT nextval('order_number_seq'),
    source            order_source NOT NULL,
    status            order_status NOT NULL DEFAULT 'new',
    customer_name     text NOT NULL CHECK (length(btrim(customer_name)) BETWEEN 1 AND 100),
    customer_phone    text NOT NULL CHECK (customer_phone ~ '^\+380[1-9][0-9]{8}$'),
    comment           text CHECK (comment IS NULL OR length(comment) <= 1000),
    delivery_type     delivery_type NOT NULL,
    delivery_address  text CHECK (delivery_address IS NULL OR length(delivery_address) <= 300),
    -- склад выбирается при подтверждении, с него резерв и выдача
    warehouse_id      uuid REFERENCES warehouses (id),
    total             numeric(12,2) NOT NULL CHECK (total >= 0),
    cancel_reason     text CHECK (cancel_reason IS NULL OR length(cancel_reason) <= 500),
    created_by        uuid REFERENCES users (id),
    created_at        timestamptz NOT NULL,
    synced_at         timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    change_seq        bigint NOT NULL,
    CHECK (delivery_type = 'pickup' OR delivery_address IS NOT NULL),
    CHECK (status IN ('new', 'cancelled') OR warehouse_id IS NOT NULL),
    -- с сайта автора нет; вручную — есть
    CHECK ((source = 'site') = (created_by IS NULL))
);
CREATE INDEX orders_status_idx ON orders (status, created_at);
CREATE INDEX orders_warehouse_idx ON orders (warehouse_id, status);
CREATE INDEX orders_phone_idx ON orders (customer_phone);
CREATE INDEX orders_change_seq_idx ON orders (change_seq);
CREATE TRIGGER orders_change_seq BEFORE INSERT OR UPDATE ON orders
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

CREATE TABLE order_items (
    id          uuid PRIMARY KEY,
    order_id    uuid NOT NULL REFERENCES orders (id),
    product_id  uuid NOT NULL REFERENCES products (id),
    stock_unit  stock_unit NOT NULL,
    qty         numeric(14,3) NOT NULL CHECK (qty > 0),
    -- цена на момент заказа, за единицу учёта
    price       numeric(12,2) NOT NULL CHECK (price >= 0),
    UNIQUE (order_id, product_id, stock_unit)
);
CREATE INDEX order_items_product_idx ON order_items (product_id);

-- История статусов: только добавление.
CREATE TABLE order_status_history (
    id           uuid PRIMARY KEY,
    order_id     uuid NOT NULL REFERENCES orders (id),
    from_status  order_status,
    to_status    order_status NOT NULL,
    user_id      uuid REFERENCES users (id),
    occurred_at  timestamptz NOT NULL,
    synced_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX order_status_history_order_idx ON order_status_history (order_id, occurred_at);
CREATE TRIGGER order_status_history_immutable BEFORE UPDATE OR DELETE ON order_status_history
    FOR EACH ROW EXECUTE FUNCTION forbid_change();

-- ---------------------------------------------------------------------------
-- Продажи: с кассы или при выдаче заказа.
-- ---------------------------------------------------------------------------
CREATE SEQUENCE sale_number_seq START 1;

CREATE TABLE sales (
    id            uuid PRIMARY KEY,
    number        bigint NOT NULL UNIQUE DEFAULT nextval('sale_number_seq'),
    channel       sale_channel NOT NULL,
    -- заказ нельзя выдать дважды
    order_id      uuid UNIQUE REFERENCES orders (id),
    warehouse_id  uuid NOT NULL REFERENCES warehouses (id),
    payment_type  payment_type NOT NULL,
    total         numeric(12,2) NOT NULL CHECK (total >= 0),
    user_id       uuid NOT NULL REFERENCES users (id),
    occurred_at   timestamptz NOT NULL,
    synced_at     timestamptz NOT NULL DEFAULT now(),
    change_seq    bigint NOT NULL,
    CHECK ((order_id IS NULL) = (channel = 'pos'))
);
CREATE INDEX sales_occurred_idx ON sales (occurred_at);
CREATE INDEX sales_warehouse_idx ON sales (warehouse_id, occurred_at);
CREATE INDEX sales_channel_idx ON sales (channel, occurred_at);
CREATE INDEX sales_change_seq_idx ON sales (change_seq);
CREATE TRIGGER sales_change_seq BEFORE INSERT OR UPDATE ON sales
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

CREATE TABLE sale_items (
    id          uuid PRIMARY KEY,
    sale_id     uuid NOT NULL REFERENCES sales (id),
    product_id  uuid NOT NULL REFERENCES products (id),
    stock_unit  stock_unit NOT NULL,
    qty         numeric(14,3) NOT NULL CHECK (qty > 0),
    price       numeric(12,2) NOT NULL CHECK (price >= 0),
    -- себестоимость за единицу учёта на момент продажи; NULL — неизвестна
    -- (товар ни разу не принимали поставкой), в отчёте это видно отдельно
    unit_cost   numeric(14,4) CHECK (unit_cost IS NULL OR unit_cost >= 0),
    UNIQUE (sale_id, product_id, stock_unit)
);
CREATE INDEX sale_items_sale_idx ON sale_items (sale_id);
CREATE INDEX sale_items_product_idx ON sale_items (product_id);

-- ---------------------------------------------------------------------------
-- Возвраты. Ссылки на продажу без внешних ключей: продажа может уйти в архив раньше.
-- Канал хранится в самом возврате по той же причине.
-- ---------------------------------------------------------------------------
CREATE TABLE returns (
    id            uuid PRIMARY KEY,
    sale_id       uuid,
    channel       sale_channel NOT NULL,
    warehouse_id  uuid NOT NULL REFERENCES warehouses (id),
    reason        text NOT NULL CHECK (length(btrim(reason)) BETWEEN 1 AND 500),
    refund_total  numeric(12,2) NOT NULL CHECK (refund_total >= 0),
    payment_type  payment_type NOT NULL,
    user_id       uuid NOT NULL REFERENCES users (id),
    occurred_at   timestamptz NOT NULL,
    synced_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX returns_sale_idx ON returns (sale_id);
CREATE INDEX returns_occurred_idx ON returns (occurred_at);

CREATE TABLE return_items (
    id            uuid PRIMARY KEY,
    return_id     uuid NOT NULL REFERENCES returns (id),
    sale_item_id  uuid,
    product_id    uuid NOT NULL REFERENCES products (id),
    stock_unit    stock_unit NOT NULL,
    qty           numeric(14,3) NOT NULL CHECK (qty > 0),
    price         numeric(12,2) NOT NULL CHECK (price >= 0),
    unit_cost     numeric(14,4) CHECK (unit_cost IS NULL OR unit_cost >= 0)
);
CREATE INDEX return_items_sale_item_idx ON return_items (sale_item_id);
CREATE INDEX return_items_return_idx ON return_items (return_id);

-- ---------------------------------------------------------------------------
-- Движения продаж и возвратов
-- ---------------------------------------------------------------------------
ALTER TABLE stock_movements
    ADD COLUMN sale_item_id uuid REFERENCES sale_items (id),
    ADD COLUMN return_item_id uuid REFERENCES return_items (id),
    ADD CONSTRAINT stock_movements_sale_link CHECK ((kind = 'sale') = (sale_item_id IS NOT NULL)),
    ADD CONSTRAINT stock_movements_return_link CHECK ((kind = 'return') = (return_item_id IS NOT NULL));
CREATE UNIQUE INDEX stock_movements_sale_once ON stock_movements (sale_item_id) WHERE kind = 'sale';
CREATE UNIQUE INDEX stock_movements_return_once ON stock_movements (return_item_id) WHERE kind = 'return';

-- ---------------------------------------------------------------------------
-- Права роли сервера. Удалять нельзя ничего; историю статусов — и менять.
-- ---------------------------------------------------------------------------
GRANT SELECT, INSERT, UPDATE ON orders, order_items TO budmaster_app;
GRANT DELETE ON order_items TO budmaster_app;  -- строки заказа меняются целиком до выдачи
GRANT SELECT, INSERT ON order_status_history, sales, sale_items, returns, return_items TO budmaster_app;
-- Сумма документа считается после записи его строк; больше в продаже и возврате менять нечего.
GRANT UPDATE (total) ON sales TO budmaster_app;
GRANT UPDATE (refund_total) ON returns TO budmaster_app;
GRANT USAGE ON SEQUENCE order_number_seq, sale_number_seq TO budmaster_app;
