use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use budmaster_server::auth::session;
use budmaster_server::cli::{self, NewOwner};
use budmaster_server::config::Config;
use budmaster_server::{AppState, audit, routes};
use clap::{Parser, Subcommand};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

#[derive(Parser)]
#[command(name = "budmaster-server", about = "Сервер Budmaster")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Запустить сервер
    Serve,
    /// Применить миграции (нужен MIGRATION_DATABASE_URL — роль-владелец базы)
    Migrate,
    /// Создать владельца. Пароль спрашивается в терминале или читается из stdin
    CreateOwner {
        #[arg(long)]
        login: String,
        #[arg(long)]
        first_name: String,
        #[arg(long)]
        last_name: String,
    },
    /// Сменить пароль владельца и закрыть все его сессии
    ResetOwnerPassword {
        #[arg(long)]
        login: String,
    },
    /// Импортировать каталог из файла JSON, поданного на стандартный ввод
    ImportCatalog,
    /// Пересчитать кэш остатков из движений товара
    RebuildBalances,
    /// Создать склад (для первого запуска)
    CreateWarehouse {
        #[arg(long)]
        name: String,
        #[arg(long)]
        address: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Файл .env — только для локальной разработки; на сервере переменные задаёт Docker Compose.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "budmaster_server=info,tower_http=info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Serve => serve().await,
        Command::Migrate => {
            let owner_url = env("MIGRATION_DATABASE_URL")?;
            cli::migrate(&owner_url, std::env::var("APP_DB_PASSWORD").ok().as_deref()).await
        }
        Command::CreateOwner { login, first_name, last_name } => {
            let password = read_new_password()?;
            let db = connect().await?;
            let id = cli::create_owner(&db, NewOwner { login, first_name, last_name, password }).await?;
            println!("Владелец создан: {id}");
            Ok(())
        }
        Command::ResetOwnerPassword { login } => {
            let password = read_new_password()?;
            let db = connect().await?;
            cli::reset_owner_password(&db, &login, password).await?;
            println!("Пароль изменён, все сессии владельца закрыты.");
            Ok(())
        }
        Command::ImportCatalog => {
            use std::io::Read;
            let mut json = String::new();
            std::io::stdin().read_to_string(&mut json)?;
            let file = budmaster_server::catalog::import::parse(&json)?;
            let db = connect().await?;
            let summary = budmaster_server::catalog::import::run(&db, file).await?;
            println!(
                "Категории: добавлено {}, уже были {}. Товары: добавлено {}, уже были {}.",
                summary.categories_created,
                summary.categories_skipped,
                summary.products_created,
                summary.products_skipped
            );
            Ok(())
        }
        Command::RebuildBalances => {
            let db = connect().await?;
            let fixed = budmaster_server::sync::stock::rebuild_balances(&db).await?;
            println!("Остатки пересчитаны. Исправлено строк: {fixed}.");
            Ok(())
        }
        Command::CreateWarehouse { name, address } => {
            let db = connect().await?;
            let id = cli::create_warehouse(&db, &name, address.as_deref()).await?;
            println!("Склад создан: {id}");
            Ok(())
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    let bind_addr = config.bind_addr;
    let db = connect().await?;

    audit::ensure_partitions(&db).await.context("не удалось подготовить секции журнала")?;
    tokio::spawn(maintenance(db.clone()));

    let app = routes::router(AppState::new(db, config)?);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "сервер запущен");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Раз в 12 часов: секции журнала на будущие месяцы и удаление давно истёкших токенов.
async fn maintenance(db: PgPool) {
    let mut interval = tokio::time::interval(Duration::from_secs(12 * 60 * 60));
    interval.tick().await; // первый тик срабатывает сразу, а секции уже созданы при запуске
    loop {
        interval.tick().await;
        // Секции создаются на 3 месяца вперёд, так что до настоящего отказа есть запас;
        // но без них перестанет работать всё, что пишет в журнал, включая вход.
        if let Err(error) = audit::ensure_partitions(&db).await {
            tracing::error!(?error, "ВНИМАНИЕ: не удалось создать секции журнала на будущие месяцы");
        }
        match session::purge_expired(&db).await {
            Ok(removed) if removed > 0 => tracing::info!(removed, "удалены истёкшие refresh-токены"),
            Ok(_) => {}
            Err(error) => tracing::error!(?error, "не удалось удалить истёкшие refresh-токены"),
        }
    }
}

async fn connect() -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&env("DATABASE_URL")?)
        .await
        .context("нет соединения с базой данных")
}

fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("не задана переменная окружения {name}"))
}

/// Пароль не передаётся аргументом: аргументы видны в списке процессов и в истории команд.
fn read_new_password() -> anyhow::Result<String> {
    use std::io::{IsTerminal, Read};

    if std::io::stdin().is_terminal() {
        let password = rpassword::prompt_password("Новый пароль: ")?;
        let repeat = rpassword::prompt_password("Повторите пароль: ")?;
        if password != repeat {
            bail!("пароли не совпадают");
        }
        Ok(password)
    } else {
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input)?;
        Ok(input.trim_end_matches(['\r', '\n']).to_owned())
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("получен сигнал остановки");
}
