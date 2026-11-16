//! Команды, которые выполняются на сервере, а не через приложение:
//! миграции, владельцы, первый склад.

use anyhow::{Context, bail};
use serde_json::json;
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

use crate::audit::{self, Actor, Entry};
use crate::auth::{password, session};
use crate::routes::users::{is_valid_login, is_valid_name};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Применяет миграции под ролью-владельцем базы и задаёт пароль роли сервера.
pub async fn migrate(owner_url: &str, app_password: Option<&str>) -> anyhow::Result<()> {
    let mut conn = PgConnection::connect(owner_url).await.context("нет соединения с базой (роль-владелец)")?;
    MIGRATOR.run(&mut conn).await.context("миграции не применились")?;

    if let Some(app_password) = app_password {
        if app_password.len() < 16 {
            bail!("APP_DB_PASSWORD слишком короткий: нужно не меньше 16 символов");
        }
        if crate::config::is_placeholder(app_password) {
            bail!("APP_DB_PASSWORD остался из образца .env.example — задайте свой пароль");
        }
        // В ALTER ROLE нельзя подставить параметр, поэтому команду собирает сама база
        // через format('%L') — значение экранируется как строковый литерал.
        let statement: String =
            sqlx::query_scalar("SELECT format('ALTER ROLE budmaster_app WITH LOGIN PASSWORD %L', $1::text)")
                .bind(app_password)
                .fetch_one(&mut conn)
                .await?;
        sqlx::query(sqlx::AssertSqlSafe(statement)).execute(&mut conn).await?;
    }
    println!("Миграции применены.");
    Ok(())
}

pub struct NewOwner {
    pub login: String,
    pub first_name: String,
    pub last_name: String,
    pub password: String,
}

pub async fn create_owner(db: &PgPool, owner: NewOwner) -> anyhow::Result<Uuid> {
    let login = owner.login.trim().to_lowercase();
    let first_name = owner.first_name.trim().to_owned();
    let last_name = owner.last_name.trim().to_owned();
    if !is_valid_login(&login) {
        bail!("логин: 3–32 символа, латинские буквы в нижнем регистре, цифры, «.», «-», «_»");
    }
    if !is_valid_name(&first_name) || !is_valid_name(&last_name) {
        bail!("имя и фамилия: от 1 до 50 символов");
    }
    if !password::is_acceptable(&owner.password) {
        bail!("пароль: от {} до {} символов", password::MIN_LEN, password::MAX_LEN);
    }

    let password_hash = password::hash(owner.password).await?;
    let id = Uuid::new_v4();

    let mut tx = db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO users (id, login, password_hash, first_name, last_name, role)
         VALUES ($1, $2, $3, $4, $5, 'owner')
         ON CONFLICT (login) DO NOTHING",
    )
    .bind(id)
    .bind(&login)
    .bind(password_hash)
    .bind(&first_name)
    .bind(&last_name)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        bail!("логин «{login}» уже занят");
    }
    audit::record(
        &mut *tx,
        Entry::new(&Actor::server_cli(), "owner.create")
            .entity("user", id)
            .details(json!({ "login": login, "first_name": first_name, "last_name": last_name })),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Меняет пароль владельца и закрывает все его сессии.
pub async fn reset_owner_password(db: &PgPool, login: &str, new_password: String) -> anyhow::Result<()> {
    if !password::is_acceptable(&new_password) {
        bail!("пароль: от {} до {} символов", password::MIN_LEN, password::MAX_LEN);
    }
    let password_hash = password::hash(new_password).await?;

    let mut tx = db.begin().await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "UPDATE users SET password_hash = $2, must_change_password = false, updated_at = now()
         WHERE login = $1::citext AND role = 'owner'
         RETURNING id",
    )
    .bind(login.trim().to_lowercase())
    .bind(password_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else { bail!("владелец с логином «{login}» не найден") };

    session::revoke_all_for_user(&mut tx, id).await?;
    audit::record(&mut *tx, Entry::new(&Actor::server_cli(), "owner.password_reset").entity("user", id)).await?;
    tx.commit().await?;
    Ok(())
}

/// Создаёт склад. Нужен для первого запуска: без склада нельзя добавить работника.
pub async fn create_warehouse(db: &PgPool, name: &str, address: Option<&str>) -> anyhow::Result<Uuid> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        bail!("название склада: от 1 до 100 символов");
    }
    let address = address.map(str::trim).filter(|address| !address.is_empty());
    if address.is_some_and(|address| address.chars().count() > 300) {
        bail!("адрес склада: до 300 символов");
    }

    let id = Uuid::new_v4();
    let mut tx = db.begin().await?;
    let inserted =
        sqlx::query("INSERT INTO warehouses (id, name, address) VALUES ($1, $2, $3) ON CONFLICT (name) DO NOTHING")
            .bind(id)
            .bind(name)
            .bind(address)
            .execute(&mut *tx)
            .await?;
    if inserted.rows_affected() == 0 {
        bail!("склад «{name}» уже есть");
    }
    audit::record(
        &mut *tx,
        Entry::new(&Actor::server_cli(), "warehouse.create")
            .entity("warehouse", id)
            .details(json!({ "name": name, "address": address })),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}
