//! Журнал действий. Записи только добавляются; изменить или удалить их нельзя
//! (триггер и права роли в миграции).

use serde_json::Value;
use sqlx::PgExecutor;
use time::OffsetDateTime;
use uuid::Uuid;

/// Кто совершил действие.
#[derive(Clone, Debug)]
pub struct Actor {
    pub user_id: Option<Uuid>,
    pub full_name: String,
    pub device_id: Option<Uuid>,
}

impl Actor {
    /// Действие без вошедшего сотрудника (например, неудачный вход).
    pub fn anonymous() -> Self {
        Self { user_id: None, full_name: "—".to_owned(), device_id: None }
    }

    /// Действие, выполненное командой на сервере.
    pub fn server_cli() -> Self {
        Self { user_id: None, full_name: "Сервер (CLI)".to_owned(), device_id: None }
    }
}

pub struct Entry<'a> {
    pub actor: &'a Actor,
    pub action: &'static str,
    pub warehouse_id: Option<Uuid>,
    pub entity_type: Option<&'static str>,
    pub entity_id: Option<Uuid>,
    pub details: Value,
    /// Когда действие произошло. Для действий на сервере совпадает с временем записи.
    pub occurred_at: OffsetDateTime,
}

impl<'a> Entry<'a> {
    pub fn new(actor: &'a Actor, action: &'static str) -> Self {
        Self {
            actor,
            action,
            warehouse_id: None,
            entity_type: None,
            entity_id: None,
            details: Value::Object(Default::default()),
            occurred_at: OffsetDateTime::now_utc(),
        }
    }

    pub fn entity(mut self, entity_type: &'static str, entity_id: Uuid) -> Self {
        self.entity_type = Some(entity_type);
        self.entity_id = Some(entity_id);
        self
    }

    pub fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

/// Пишет запись в журнал. Вызывается в той же транзакции, что и само действие:
/// либо сохраняются оба, либо ни одно.
pub async fn record(executor: impl PgExecutor<'_>, entry: Entry<'_>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO audit_log
            (id, user_id, user_full_name, device_id, warehouse_id, action, entity_type, entity_id, details, occurred_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(Uuid::new_v4())
    .bind(entry.actor.user_id)
    .bind(&entry.actor.full_name)
    .bind(entry.actor.device_id)
    .bind(entry.warehouse_id)
    .bind(entry.action)
    .bind(entry.entity_type)
    .bind(entry.entity_id)
    .bind(entry.details)
    .bind(entry.occurred_at)
    .execute(executor)
    .await?;
    Ok(())
}

/// Создаёт секции журнала на текущий и три следующих месяца.
/// Без секции на нужный месяц запись в журнал (а с ней и само действие) завершится ошибкой.
pub async fn ensure_partitions(executor: impl PgExecutor<'_>) -> sqlx::Result<()> {
    sqlx::query("SELECT ensure_audit_partitions(3)").execute(executor).await?;
    Ok(())
}
