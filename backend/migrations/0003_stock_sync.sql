-- Склад и синхронизация: поставщики, движения товара, остатки, поставки, перемещения,
-- средняя себестоимость, реестр операций с устройств.
-- Схема описана в docs/design.md, разделы 4.3, 4.5, 6 и 9.

CREATE TYPE stock_unit AS ENUM ('unit', 'pack');
CREATE TYPE movement_kind AS ENUM (
    'receipt', 'sale', 'return', 'write_off', 'adjustment', 'pack_open', 'transfer_out', 'transfer_in'
);
CREATE TYPE supply_status AS ENUM ('planned', 'received', 'cancelled');

-- ---------------------------------------------------------------------------
-- «Побеждает последняя правка» по каждому полю: время последней правки поля.
-- У товаров колонка уже есть (0002).
-- ---------------------------------------------------------------------------
ALTER TABLE categories ADD COLUMN field_updated_at jsonb NOT NULL DEFAULT '{}';
ALTER TABLE warehouses ADD COLUMN field_updated_at jsonb NOT NULL DEFAULT '{}';

-- Нижняя граница времени операций устройства: момент, когда устройство в последний раз
-- полностью отправило свою очередь. Операции, созданные после этого, не могут быть старше.
ALTER TABLE devices ADD COLUMN sync_floor timestamptz;

-- ---------------------------------------------------------------------------
-- Поставщики. Удаляются мягко.
-- ---------------------------------------------------------------------------
CREATE TABLE suppliers (
    id                uuid PRIMARY KEY,
    name              text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 200),
    contact_person    text CHECK (contact_person IS NULL OR length(contact_person) <= 200),
    phone             text CHECK (phone IS NULL OR length(phone) <= 50),
    note              text CHECK (note IS NULL OR length(note) <= 2000),
    is_deleted        boolean NOT NULL DEFAULT false,
    field_updated_at  jsonb NOT NULL DEFAULT '{}',
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    change_seq        bigint NOT NULL
);
CREATE INDEX suppliers_change_seq_idx ON suppliers (change_seq);
CREATE TRIGGER suppliers_change_seq BEFORE INSERT OR UPDATE ON suppliers
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

-- ---------------------------------------------------------------------------
-- Средняя себестоимость, общая по компании, за базовую единицу товара (за штуку у фасованных).
-- Отдельно от товаров, чтобы не уходить на устройства работников.
-- ---------------------------------------------------------------------------
CREATE TABLE product_costs (
    product_id  uuid PRIMARY KEY REFERENCES products (id),
    avg_cost    numeric(14,4) NOT NULL DEFAULT 0 CHECK (avg_cost >= 0),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Поставки
-- ---------------------------------------------------------------------------
CREATE SEQUENCE supply_number_seq START 1;

CREATE TABLE supplies (
    id            uuid PRIMARY KEY,
    number        bigint NOT NULL UNIQUE DEFAULT nextval('supply_number_seq'),
    supplier_id   uuid NOT NULL REFERENCES suppliers (id),
    warehouse_id  uuid NOT NULL REFERENCES warehouses (id),
    status        supply_status NOT NULL,
    expected_on   date,
    note          text CHECK (note IS NULL OR length(note) <= 2000),
    created_by    uuid NOT NULL REFERENCES users (id),
    created_at    timestamptz NOT NULL,
    received_by   uuid REFERENCES users (id),
    received_at   timestamptz,
    synced_at     timestamptz NOT NULL DEFAULT now(),
    change_seq    bigint NOT NULL,
    CHECK ((received_at IS NOT NULL) = (status = 'received')),
    CHECK ((received_by IS NOT NULL) = (status = 'received'))
);
CREATE INDEX supplies_supplier_idx ON supplies (supplier_id);
CREATE INDEX supplies_status_idx ON supplies (status, warehouse_id);
CREATE INDEX supplies_change_seq_idx ON supplies (change_seq);
CREATE TRIGGER supplies_change_seq BEFORE INSERT OR UPDATE ON supplies
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

CREATE TABLE supply_items (
    id            uuid PRIMARY KEY,
    supply_id     uuid NOT NULL REFERENCES supplies (id),
    product_id    uuid NOT NULL REFERENCES products (id),
    stock_unit    stock_unit NOT NULL,
    expected_qty  numeric(14,3) CHECK (expected_qty > 0),
    received_qty  numeric(14,3) CHECK (received_qty >= 0),
    -- цена за stock_unit (за пачку, если принимали пачками)
    unit_cost     numeric(14,4) CHECK (unit_cost >= 0),
    UNIQUE (supply_id, product_id, stock_unit),
    CHECK ((received_qty IS NULL) = (unit_cost IS NULL))
);
CREATE INDEX supply_items_product_idx ON supply_items (product_id);

-- ---------------------------------------------------------------------------
-- Перемещения между складами — одно действие.
-- ---------------------------------------------------------------------------
CREATE TABLE transfers (
    id                 uuid PRIMARY KEY,
    from_warehouse_id  uuid NOT NULL REFERENCES warehouses (id),
    to_warehouse_id    uuid NOT NULL REFERENCES warehouses (id),
    note               text CHECK (note IS NULL OR length(note) <= 2000),
    user_id            uuid NOT NULL REFERENCES users (id),
    occurred_at        timestamptz NOT NULL,
    synced_at          timestamptz NOT NULL DEFAULT now(),
    CHECK (from_warehouse_id <> to_warehouse_id)
);

CREATE TABLE transfer_items (
    id           uuid PRIMARY KEY,
    transfer_id  uuid NOT NULL REFERENCES transfers (id),
    product_id   uuid NOT NULL REFERENCES products (id),
    stock_unit   stock_unit NOT NULL,
    qty          numeric(14,3) NOT NULL CHECK (qty > 0),
    UNIQUE (transfer_id, product_id, stock_unit)
);

-- ---------------------------------------------------------------------------
-- Движения товара — главная таблица учёта. Только добавление.
-- Ссылки на строки продаж и возвратов добавит миграция этапа продаж.
-- ---------------------------------------------------------------------------
CREATE TABLE stock_movements (
    id                uuid PRIMARY KEY,
    seq               bigserial UNIQUE,
    product_id        uuid NOT NULL REFERENCES products (id),
    warehouse_id      uuid NOT NULL REFERENCES warehouses (id),
    stock_unit        stock_unit NOT NULL,
    qty               numeric(14,3) NOT NULL CHECK (qty <> 0),
    kind              movement_kind NOT NULL,
    -- приход: закупочная цена за stock_unit; расход: себестоимость за stock_unit на момент проведения
    unit_cost         numeric(14,4) CHECK (unit_cost IS NULL OR unit_cost >= 0),
    operation_id      uuid NOT NULL,
    supply_item_id    uuid REFERENCES supply_items (id),
    transfer_item_id  uuid REFERENCES transfer_items (id),
    reason            text CHECK (reason IS NULL OR length(btrim(reason)) BETWEEN 1 AND 500),
    user_id           uuid NOT NULL REFERENCES users (id),
    device_id         uuid REFERENCES devices (id),
    occurred_at       timestamptz NOT NULL,
    synced_at         timestamptz NOT NULL DEFAULT now(),
    CHECK (kind NOT IN ('write_off', 'adjustment') OR reason IS NOT NULL),
    CHECK ((kind = 'receipt') = (supply_item_id IS NOT NULL)),
    CHECK ((kind IN ('transfer_out', 'transfer_in')) = (transfer_item_id IS NOT NULL))
);
CREATE INDEX stock_movements_product_idx ON stock_movements (product_id, warehouse_id);
CREATE INDEX stock_movements_warehouse_idx ON stock_movements (warehouse_id, occurred_at);
CREATE INDEX stock_movements_kind_idx ON stock_movements (kind, occurred_at);
CREATE INDEX stock_movements_operation_idx ON stock_movements (operation_id);
-- одну строку поставки нельзя провести дважды, одну строку перемещения — дважды в одну сторону
CREATE UNIQUE INDEX stock_movements_receipt_once ON stock_movements (supply_item_id) WHERE kind = 'receipt';
CREATE UNIQUE INDEX stock_movements_transfer_once ON stock_movements (transfer_item_id, kind)
    WHERE transfer_item_id IS NOT NULL;

CREATE TRIGGER stock_movements_immutable BEFORE UPDATE OR DELETE ON stock_movements
    FOR EACH ROW EXECUTE FUNCTION forbid_change();

-- ---------------------------------------------------------------------------
-- Остатки — кэш суммы движений. Пересчитывается командой rebuild-balances.
-- Резервы заказов появятся на этапе продаж.
-- ---------------------------------------------------------------------------
CREATE TABLE stock_balances (
    product_id      uuid NOT NULL REFERENCES products (id),
    warehouse_id    uuid NOT NULL REFERENCES warehouses (id),
    qty_units       numeric(14,3) NOT NULL DEFAULT 0,
    qty_packs       numeric(14,3) NOT NULL DEFAULT 0,
    reserved_units  numeric(14,3) NOT NULL DEFAULT 0,
    reserved_packs  numeric(14,3) NOT NULL DEFAULT 0,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    change_seq      bigint NOT NULL,
    PRIMARY KEY (product_id, warehouse_id)
);
CREATE INDEX stock_balances_change_seq_idx ON stock_balances (change_seq);
CREATE TRIGGER stock_balances_change_seq BEFORE INSERT OR UPDATE ON stock_balances
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

-- ---------------------------------------------------------------------------
-- Реестр операций с устройств: повторная отправка той же операции её не повторяет.
-- ---------------------------------------------------------------------------
CREATE TABLE sync_operations (
    op_id            uuid PRIMARY KEY,
    user_id          uuid NOT NULL REFERENCES users (id),
    device_id        uuid NOT NULL REFERENCES devices (id),
    op_type          text NOT NULL CHECK (length(op_type) <= 64),
    result           text NOT NULL CHECK (result IN ('applied', 'rejected')),
    reject_code      text CHECK (reject_code IS NULL OR length(reject_code) <= 64),
    clock_offset_ms  bigint,
    -- время, как его прислало устройство, без поправок
    occurred_at      timestamptz NOT NULL,
    -- время после поправки на часы устройства и границы; по нему проведена операция
    effective_at     timestamptz NOT NULL,
    received_at      timestamptz NOT NULL DEFAULT now(),
    CHECK ((result = 'rejected') = (reject_code IS NOT NULL))
);
CREATE INDEX sync_operations_received_idx ON sync_operations (received_at);
CREATE INDEX sync_operations_device_idx ON sync_operations (device_id, received_at);

-- ---------------------------------------------------------------------------
-- Наличие для сайта: есть на складах (пачки в штуках, минус резерв) — «в наявності»;
-- нет, но товар ждут в запланированной поставке — «скоро з'явиться»; иначе — «немає».
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION product_availability(product uuid) RETURNS text
LANGUAGE sql STABLE AS $$
    SELECT CASE
        WHEN COALESCE((
            SELECT sum(b.qty_units - b.reserved_units
                       + (b.qty_packs - b.reserved_packs) * COALESCE(p.pack_size, 0))
            FROM stock_balances b
            JOIN products p ON p.id = b.product_id
            JOIN warehouses w ON w.id = b.warehouse_id AND w.is_active
            WHERE b.product_id = product
        ), 0) > 0 THEN 'in_stock'
        WHEN EXISTS (
            SELECT 1 FROM supply_items i JOIN supplies s ON s.id = i.supply_id
            WHERE i.product_id = product AND s.status = 'planned'
        ) THEN 'coming_soon'
        ELSE 'out_of_stock'
    END
$$;

-- ---------------------------------------------------------------------------
-- Права роли сервера. Движения — только чтение и добавление; удалять нельзя ничего.
-- ---------------------------------------------------------------------------
GRANT SELECT, INSERT, UPDATE ON suppliers, product_costs, supplies, supply_items, stock_balances TO budmaster_app;
GRANT SELECT, INSERT ON transfers, transfer_items, stock_movements TO budmaster_app;
GRANT SELECT, INSERT, UPDATE ON sync_operations TO budmaster_app;
GRANT USAGE ON SEQUENCE supply_number_seq, stock_movements_seq_seq TO budmaster_app;
