-- Основа: склады, сотрудники, устройства, токены, журнал действий.
-- Схема описана в docs/design.md, раздел 4.

CREATE EXTENSION IF NOT EXISTS citext;

-- Роль, под которой работает сервер. Пароль задаёт команда `migrate` из переменной окружения,
-- в миграциях его нет. Тесты создают много баз параллельно, поэтому гонку «роль уже есть» глушим.
DO $$
BEGIN
    CREATE ROLE budmaster_app LOGIN;
EXCEPTION
    WHEN duplicate_object OR unique_violation THEN NULL;
END
$$;

CREATE TYPE user_role AS ENUM ('owner', 'worker');

-- ---------------------------------------------------------------------------
-- Счётчик изменений: по нему устройство спрашивает «что изменилось после N».
-- Блокировка держится до конца транзакции, поэтому номера фиксируются в базе
-- в том же порядке, в каком выданы, и курсор загрузки не пропускает строки.
-- ---------------------------------------------------------------------------
CREATE SEQUENCE change_seq;

CREATE FUNCTION set_change_seq() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(727001);
    NEW.change_seq := nextval('change_seq');
    RETURN NEW;
END
$$;

-- ---------------------------------------------------------------------------
-- Склады
-- ---------------------------------------------------------------------------
CREATE TABLE warehouses (
    id          uuid PRIMARY KEY,
    name        text NOT NULL UNIQUE CHECK (length(btrim(name)) BETWEEN 1 AND 100),
    address     text CHECK (address IS NULL OR length(address) <= 300),
    is_active   boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    change_seq  bigint NOT NULL
);
CREATE INDEX warehouses_change_seq_idx ON warehouses (change_seq);
CREATE TRIGGER warehouses_change_seq BEFORE INSERT OR UPDATE ON warehouses
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

-- ---------------------------------------------------------------------------
-- Сотрудники. Физически не удаляются: блокировка = is_active = false.
-- ---------------------------------------------------------------------------
CREATE TABLE users (
    id                    uuid PRIMARY KEY,
    -- citext сравнивает без учёта регистра, поэтому формат проверяется на тексте
    login                 citext NOT NULL UNIQUE CHECK (login::text ~ '^[a-z0-9][a-z0-9._-]{2,31}$'),
    password_hash         text NOT NULL,
    first_name            text NOT NULL CHECK (length(btrim(first_name)) BETWEEN 1 AND 50),
    last_name             text NOT NULL CHECK (length(btrim(last_name)) BETWEEN 1 AND 50),
    role                  user_role NOT NULL,
    -- биты 0–4 (docs/design.md, раздел 7); новый бит добавляется миграцией
    permissions           bigint NOT NULL DEFAULT 0 CHECK (permissions >= 0 AND permissions < 32),
    default_warehouse_id  uuid REFERENCES warehouses (id),
    is_active             boolean NOT NULL DEFAULT true,
    must_change_password  boolean NOT NULL DEFAULT false,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    change_seq            bigint NOT NULL,
    -- работник всегда привязан к складу; владельцу склад не обязателен
    CONSTRAINT users_worker_has_warehouse CHECK (role = 'owner' OR default_warehouse_id IS NOT NULL)
);
CREATE INDEX users_change_seq_idx ON users (change_seq);
CREATE TRIGGER users_change_seq BEFORE INSERT OR UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION set_change_seq();

-- История блокировок: нужна, чтобы после разблокировки не принять операции,
-- сделанные во время блокировки.
CREATE TABLE user_blocks (
    id            uuid PRIMARY KEY,
    user_id       uuid NOT NULL REFERENCES users (id),
    blocked_at    timestamptz NOT NULL,
    blocked_by    uuid NOT NULL REFERENCES users (id),
    unblocked_at  timestamptz,
    unblocked_by  uuid REFERENCES users (id),
    CHECK ((unblocked_at IS NULL) = (unblocked_by IS NULL)),
    CHECK (unblocked_at IS NULL OR unblocked_at >= blocked_at)
);
CREATE INDEX user_blocks_user_idx ON user_blocks (user_id, blocked_at);
-- у сотрудника не больше одной незакрытой блокировки
CREATE UNIQUE INDEX user_blocks_one_open_idx ON user_blocks (user_id) WHERE unblocked_at IS NULL;

-- ---------------------------------------------------------------------------
-- Устройства и refresh-токены
-- ---------------------------------------------------------------------------
CREATE TABLE devices (
    id             uuid PRIMARY KEY,
    user_id        uuid NOT NULL REFERENCES users (id),
    model          text CHECK (model IS NULL OR length(model) <= 100),
    registered_at  timestamptz NOT NULL DEFAULT now(),
    last_sync_at   timestamptz,
    revoked_at     timestamptz
);
CREATE INDEX devices_user_idx ON devices (user_id);

CREATE TABLE refresh_tokens (
    id          uuid PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES users (id),
    device_id   uuid NOT NULL REFERENCES devices (id),
    family_id   uuid NOT NULL,
    token_hash  bytea NOT NULL UNIQUE,  -- SHA-256; сам токен не хранится
    expires_at  timestamptz NOT NULL,
    used_at     timestamptz,
    revoked_at  timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX refresh_tokens_user_idx ON refresh_tokens (user_id);
CREATE INDEX refresh_tokens_family_idx ON refresh_tokens (family_id);
CREATE INDEX refresh_tokens_device_idx ON refresh_tokens (device_id);

-- ---------------------------------------------------------------------------
-- Журнал действий: только добавление, секции по месяцам.
-- ---------------------------------------------------------------------------
CREATE TABLE audit_log (
    id              uuid NOT NULL,
    user_id         uuid REFERENCES users (id),
    user_full_name  text NOT NULL,
    device_id       uuid,
    warehouse_id    uuid,
    action          text NOT NULL CHECK (length(action) BETWEEN 1 AND 64),
    entity_type     text,
    entity_id       uuid,
    details         jsonb NOT NULL DEFAULT '{}',
    occurred_at     timestamptz NOT NULL,
    synced_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (id, synced_at)
) PARTITION BY RANGE (synced_at);

CREATE INDEX audit_log_synced_idx ON audit_log (synced_at DESC);
CREATE INDEX audit_log_user_idx ON audit_log (user_id, synced_at);
CREATE INDEX audit_log_warehouse_idx ON audit_log (warehouse_id, synced_at);
CREATE INDEX audit_log_entity_idx ON audit_log (entity_type, entity_id);

CREATE FUNCTION forbid_change() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'записи таблицы % нельзя изменять или удалять', TG_TABLE_NAME
        USING ERRCODE = 'insufficient_privilege';
END
$$;

CREATE TRIGGER audit_log_immutable BEFORE UPDATE OR DELETE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION forbid_change();

-- Создаёт секции журнала: текущий месяц и months_ahead следующих.
-- SECURITY DEFINER: сервер работает под ролью без права создавать таблицы.
CREATE FUNCTION ensure_audit_partitions(months_ahead int DEFAULT 3) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
DECLARE
    month_start date;
    part_name text;
BEGIN
    IF months_ahead < 0 OR months_ahead > 24 THEN
        RAISE EXCEPTION 'months_ahead вне диапазона 0..24';
    END IF;
    -- секции создаются по одной за раз даже при параллельных вызовах
    PERFORM pg_advisory_xact_lock(727002);
    FOR i IN 0..months_ahead LOOP
        month_start := (date_trunc('month', now() AT TIME ZONE 'UTC') + make_interval(months => i))::date;
        part_name := 'audit_log_' || to_char(month_start, 'YYYY_MM');
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS %I PARTITION OF audit_log FOR VALUES FROM (%L) TO (%L)',
            part_name,
            month_start::timestamp AT TIME ZONE 'UTC',
            (month_start + interval '1 month')::timestamp AT TIME ZONE 'UTC'
        );
    END LOOP;
END
$$;

SELECT ensure_audit_partitions(3);

-- ---------------------------------------------------------------------------
-- Права роли сервера. DELETE нет нигде, кроме просроченных refresh-токенов;
-- на журнал — только чтение и добавление.
-- ---------------------------------------------------------------------------
GRANT USAGE ON SCHEMA public TO budmaster_app;
GRANT SELECT, INSERT, UPDATE ON warehouses, user_blocks, devices TO budmaster_app;
-- У сотрудника сервер не может менять id, логин и роль: работник не станет владельцем
-- даже при ошибке в коде сервера.
GRANT SELECT, INSERT ON users TO budmaster_app;
GRANT UPDATE (password_hash, first_name, last_name, permissions, default_warehouse_id,
              is_active, must_change_password, updated_at) ON users TO budmaster_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON refresh_tokens TO budmaster_app;
GRANT SELECT, INSERT ON audit_log TO budmaster_app;
GRANT USAGE ON SEQUENCE change_seq TO budmaster_app;
REVOKE ALL ON FUNCTION ensure_audit_partitions(int) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION ensure_audit_partitions(int) TO budmaster_app;
