-- Архивация (docs/design.md, раздел 11).

-- Реестр архивных файлов.
CREATE TABLE audit_archives (
    id            uuid PRIMARY KEY,
    -- что внутри: журнал, реестр синхронизации или данные учёта
    dataset       text NOT NULL CHECK (dataset IN ('audit_log', 'sync_operations', 'operations')),
    period_kind   text NOT NULL CHECK (period_kind IN ('month', 'year')),
    period_start  date NOT NULL,
    -- имя файла в папке архива (без пути)
    file_name     text NOT NULL CHECK (file_name ~ '^[a-z_]+-[0-9]{4}(-[0-9]{2})?\.jsonl\.zst$'),
    row_count     bigint NOT NULL CHECK (row_count >= 0),
    sha256        bytea NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (dataset, period_kind, period_start)
);

-- Остаток на начало хранимого периода: сумма движений, ушедших в архив.
-- Остаток = остаток на начало + движения, оставшиеся в базе.
CREATE TABLE stock_opening_balances (
    product_id    uuid NOT NULL REFERENCES products (id),
    warehouse_id  uuid NOT NULL REFERENCES warehouses (id),
    qty_units     numeric(14,3) NOT NULL DEFAULT 0,
    qty_packs     numeric(14,3) NOT NULL DEFAULT 0,
    -- движения до этой даты (по киевскому времени) — в архиве
    as_of         date NOT NULL,
    PRIMARY KEY (product_id, warehouse_id)
);

-- Итоги архивных месяцев для отчётов: по каналу, складу и товару.
-- Канал NULL — списания (у них нет канала).
CREATE TABLE report_monthly_totals (
    month               date NOT NULL CHECK (extract(day FROM month) = 1),
    channel             sale_channel,
    warehouse_id        uuid NOT NULL REFERENCES warehouses (id),
    product_id          uuid NOT NULL REFERENCES products (id),
    turnover            numeric(14,2) NOT NULL DEFAULT 0,
    returns             numeric(14,2) NOT NULL DEFAULT 0,
    -- часть оборота и возвратов по строкам с неизвестной себестоимостью: в прибыль не входит
    turnover_unknown_cost  numeric(14,2) NOT NULL DEFAULT 0,
    returns_unknown_cost   numeric(14,2) NOT NULL DEFAULT 0,
    sale_cost           numeric(18,4) NOT NULL DEFAULT 0,
    return_cost         numeric(18,4) NOT NULL DEFAULT 0,
    losses              numeric(18,4) NOT NULL DEFAULT 0,
    unknown_cost_lines  integer NOT NULL DEFAULT 0,
    unknown_loss_lines  integer NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX report_monthly_totals_key
    ON report_monthly_totals (month, channel, warehouse_id, product_id) NULLS NOT DISTINCT;

-- Запрет изменения неизменяемых таблиц. Единственное исключение — удаление при архивации:
-- его делает команда сервера под ролью-владельцем базы, включив `budmaster.archiving`.
-- Роль сервера не может ни удалять из этих таблиц (у неё нет права DELETE), ни
-- воспользоваться исключением (проверяется и сама роль).
CREATE OR REPLACE FUNCTION forbid_change() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE'
       AND current_setting('budmaster.archiving', true) = 'on'
       AND current_user <> 'budmaster_app' THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'записи таблицы % нельзя изменять или удалять', TG_TABLE_NAME
        USING ERRCODE = 'insufficient_privilege';
END
$$;

GRANT SELECT ON audit_archives, stock_opening_balances, report_monthly_totals TO budmaster_app;

-- Строки заказа до выдачи заменяются целиком (удаляются и записываются заново) — менять их не нужно.
REVOKE UPDATE ON order_items FROM budmaster_app;
