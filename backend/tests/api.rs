//! Тесты API на настоящей базе PostgreSQL.

mod common;

use axum::http::StatusCode;
use common::{OWNER_PASSWORD, TestApp, WORKER_PASSWORD};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

// --- Вход -------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn login_returns_tokens_and_profile(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    app.create_owner("owner").await;

    let response = app.try_login("  OWNER ", OWNER_PASSWORD, Uuid::new_v4()).await;
    assert_eq!(response.status, StatusCode::OK, "логин не зависит от регистра и пробелов");
    assert_eq!(response.body["user"]["role"], "owner");
    assert_eq!(response.body["user"]["login"], "owner");
    assert_eq!(response.body["expires_in"], 900);
    assert!(response.body["user"].get("password_hash").is_none(), "хэш пароля наружу не отдаётся");

    let access = response.body["access_token"].as_str().unwrap();
    let me = app.get("/api/v1/me", access).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["first_name"], "Олена");
    assert_eq!(app.audit_count("auth.login").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn wrong_password_and_unknown_login_look_the_same(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    app.create_owner("owner").await;

    let wrong = app.try_login("owner", "wrong-password", Uuid::new_v4()).await;
    let unknown = app.try_login("nobody", "wrong-password", Uuid::new_v4()).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.body, unknown.body);
    assert_eq!(wrong.code(), "invalid_credentials");

    assert_eq!(app.audit_count("auth.login_failed").await, 2);
    let leaked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE details::text LIKE '%wrong-password%'")
            .fetch_one(&app.admin)
            .await
            .unwrap();
    assert_eq!(leaked, 0, "пароль не попадает в журнал");
}

#[sqlx::test(migrations = "./migrations")]
async fn login_is_rate_limited_after_five_failures(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    app.create_owner("owner").await;

    for _ in 0..5 {
        assert_eq!(app.try_login("owner", "bad-password", Uuid::new_v4()).await.status, StatusCode::UNAUTHORIZED);
    }
    let blocked = app.try_login("owner", OWNER_PASSWORD, Uuid::new_v4()).await;
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS, "даже верный пароль не проходит");
    assert_eq!(blocked.code(), "rate_limited");
    assert!(blocked.retry_after.is_some_and(|v| v.parse::<u64>().is_ok_and(|secs| secs > 0)));
}

#[sqlx::test(migrations = "./migrations")]
async fn requests_without_valid_token_are_rejected(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    for path in ["/api/v1/me", "/api/v1/users", "/api/v1/audit-log", "/api/v1/warehouses"] {
        let missing = app.request("GET", path, None, None).await;
        assert_eq!(missing.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(missing.code(), "missing_token");
        let garbage = app.get(path, "not-a-token").await;
        assert_eq!(garbage.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(garbage.code(), "invalid_token");
    }
    assert_eq!(app.request("GET", "/health", None, None).await.status, StatusCode::OK);
    assert_eq!(app.request("GET", "/api/v1/nope", None, None).await.status, StatusCode::NOT_FOUND);
}

// --- Обновление токенов -----------------------------------------------------

async fn refresh(app: &TestApp, token: &str, device_id: Uuid) -> common::Response {
    app.request(
        "POST",
        "/api/v1/auth/refresh",
        None,
        Some(json!({ "refresh_token": token, "device_id": device_id })),
    )
    .await
}

#[sqlx::test(migrations = "./migrations")]
async fn refresh_rotates_and_tolerates_a_quick_retry(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    let first = refresh(&app, &owner.refresh, owner.device_id).await;
    assert_eq!(first.status, StatusCode::OK);
    let new_refresh = first.body["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(new_refresh, owner.refresh);
    assert_eq!(app.get("/api/v1/me", first.body["access_token"].as_str().unwrap()).await.status, StatusCode::OK);

    // Ответ мог потеряться в сети: быстрый повтор со старым токеном — не кража.
    let retry = refresh(&app, &owner.refresh, owner.device_id).await;
    assert_eq!(retry.status, StatusCode::OK);
    let retry_refresh = retry.body["refresh_token"].as_str().unwrap().to_owned();
    assert_eq!(app.get("/api/v1/me", retry.body["access_token"].as_str().unwrap()).await.status, StatusCode::OK);

    // У сессии всегда один действующий refresh-токен: выданный в первый раз заменён.
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM refresh_tokens WHERE used_at IS NULL AND revoked_at IS NULL",
    )
    .fetch_one(&app.admin)
    .await
    .unwrap();
    assert_eq!(live, 1);
    assert_eq!(refresh(&app, &retry_refresh, owner.device_id).await.status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn quick_retry_cannot_be_used_to_fork_a_session(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    // Настоящее устройство обновило токен; вор в течение минуты повторил запрос со старым.
    let legit = refresh(&app, &owner.refresh, owner.device_id).await;
    let legit_refresh = legit.body["refresh_token"].as_str().unwrap().to_owned();
    let thief = refresh(&app, &owner.refresh, owner.device_id).await;
    assert_eq!(thief.status, StatusCode::OK);
    let thief_refresh = thief.body["refresh_token"].as_str().unwrap().to_owned();
    let thief_access = thief.body["access_token"].as_str().unwrap().to_owned();

    // Как только настоящее устройство предъявляет свой (уже заменённый) токен, сессия
    // закрывается целиком — двух независимых веток не получается.
    let detected = refresh(&app, &legit_refresh, owner.device_id).await;
    assert_eq!(detected.status, StatusCode::UNAUTHORIZED);
    assert_eq!(detected.code(), "refresh_token_reused");
    assert_eq!(refresh(&app, &thief_refresh, owner.device_id).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.get("/api/v1/me", &thief_access).await.status, StatusCode::UNAUTHORIZED, "access-токен вора тоже");
    assert_eq!(app.audit_count("auth.token_reuse").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn late_reuse_of_refresh_token_revokes_the_whole_chain(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    let rotated = refresh(&app, &owner.refresh, owner.device_id).await;
    let new_refresh = rotated.body["refresh_token"].as_str().unwrap().to_owned();

    // Старый токен предъявлен снова, но уже спустя время.
    sqlx::query("UPDATE refresh_tokens SET used_at = now() - interval '5 minutes' WHERE used_at IS NOT NULL")
        .execute(&app.admin)
        .await
        .unwrap();
    let reused = refresh(&app, &owner.refresh, owner.device_id).await;
    assert_eq!(reused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reused.code(), "refresh_token_reused");

    // Отозвана вся цепочка, включая свежий токен.
    assert_eq!(refresh(&app, &new_refresh, owner.device_id).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.audit_count("auth.token_reuse").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn refresh_token_from_another_device_is_treated_as_stolen(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    let stolen = refresh(&app, &owner.refresh, Uuid::new_v4()).await;
    assert_eq!(stolen.status, StatusCode::UNAUTHORIZED);
    assert_eq!(stolen.code(), "refresh_token_reused");
    assert_eq!(refresh(&app, &owner.refresh, owner.device_id).await.status, StatusCode::UNAUTHORIZED);

    assert_eq!(refresh(&app, "unknown-token", owner.device_id).await.code(), "invalid_refresh_token");
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_refresh_token_is_rejected(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    sqlx::query("UPDATE refresh_tokens SET expires_at = now() - interval '1 second'")
        .execute(&app.admin)
        .await
        .unwrap();
    assert_eq!(refresh(&app, &owner.refresh, owner.device_id).await.code(), "invalid_refresh_token");
}

#[sqlx::test(migrations = "./migrations")]
async fn logout_kills_the_session_at_once(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    assert_eq!(app.post("/api/v1/auth/logout", &owner.access, json!({})).await.status, StatusCode::OK);
    assert_eq!(app.get("/api/v1/me", &owner.access).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refresh(&app, &owner.refresh, owner.device_id).await.status, StatusCode::UNAUTHORIZED);

    // Повторный вход с того же устройства снова работает.
    let again = app.try_login("owner", OWNER_PASSWORD, owner.device_id).await;
    assert_eq!(again.status, StatusCode::OK);
}

// --- Роли и права -----------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn worker_gets_403_on_every_owner_endpoint(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let worker = app.active_worker(&owner, "ivan").await;
    let other = app.create_worker(&owner, "petro", 0).await;

    let calls = [
        ("GET", "/api/v1/users".to_owned(), None),
        (
            "POST",
            "/api/v1/users".to_owned(),
            Some(json!({
                "login": "hacker", "password": "long-password-1", "first_name": "A", "last_name": "B",
                "default_warehouse_id": app.warehouse_id,
            })),
        ),
        ("PATCH", format!("/api/v1/users/{other}"), Some(json!({ "permissions": 31 }))),
        ("PATCH", format!("/api/v1/users/{}", worker.user_id), Some(json!({ "permissions": 31 }))),
        ("POST", format!("/api/v1/users/{other}/block"), Some(json!({}))),
        ("POST", format!("/api/v1/users/{other}/unblock"), Some(json!({}))),
        ("POST", format!("/api/v1/users/{other}/reset-password"), Some(json!({ "new_password": "long-password-1" }))),
        ("GET", "/api/v1/audit-log".to_owned(), None),
    ];
    for (method, path, body) in calls {
        let response = app.request(method, &path, Some(&worker.access), body).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{method} {path}");
        assert_eq!(response.code(), "owner_only", "{method} {path}");
    }

    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users").fetch_one(&app.admin).await.unwrap();
    assert_eq!(users, 3, "никто лишний не создан");
    let own_mask: i64 = sqlx::query_scalar("SELECT permissions FROM users WHERE id = $1")
        .bind(worker.user_id)
        .fetch_one(&app.admin)
        .await
        .unwrap();
    assert_eq!(own_mask, 0, "работник не выдал себе права");

    // Обычные эндпоинты работнику доступны.
    assert_eq!(app.get("/api/v1/warehouses", &worker.access).await.status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn api_cannot_create_edit_or_block_an_owner(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let second_owner = app.create_owner("owner2").await;

    // Поле role в запросе игнорируется: создаётся работник.
    let created = app
        .post(
            "/api/v1/users",
            &owner.access,
            json!({
                "login": "sneaky", "password": "long-password-1", "first_name": "A", "last_name": "B",
                "default_warehouse_id": app.warehouse_id, "role": "owner",
            }),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["role"], "worker");
    assert_eq!(created.body["must_change_password"], true);

    for target in [second_owner, owner.user_id] {
        let calls = [
            ("PATCH", format!("/api/v1/users/{target}"), json!({ "first_name": "X" })),
            ("POST", format!("/api/v1/users/{target}/block"), json!({})),
            ("POST", format!("/api/v1/users/{target}/unblock"), json!({})),
            ("POST", format!("/api/v1/users/{target}/reset-password"), json!({ "new_password": "long-password-1" })),
        ];
        for (method, path, body) in calls {
            let response = app.request(method, &path, Some(&owner.access), Some(body)).await;
            assert_eq!(response.status, StatusCode::FORBIDDEN, "{method} {path}");
            assert_eq!(response.code(), "owner_protected");
        }
    }
    let owners_ok: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE role = 'owner' AND is_active AND first_name = 'Олена'")
            .fetch_one(&app.admin)
            .await
            .unwrap();
    assert_eq!(owners_ok, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn user_input_is_validated(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let valid = json!({
        "login": "ivan", "password": "long-password-1", "first_name": "Іван", "last_name": "Петренко",
        "default_warehouse_id": app.warehouse_id, "permissions": 3,
    });

    let cases = [
        ("login", json!("Ab")),
        ("login", json!("іван")),
        ("password", json!("short")),
        ("first_name", json!("   ")),
        ("last_name", json!("x".repeat(51))),
        ("permissions", json!(32)),
        ("permissions", json!(-1)),
        ("permissions", json!(1_i64 << 40)),
        ("default_warehouse_id", json!(Uuid::new_v4())),
    ];
    for (field, value) in cases {
        let mut body = valid.clone();
        body[field] = value.clone();
        let response = app.post("/api/v1/users", &owner.access, body).await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "{field} = {value}");
        assert!(response.body["error"]["fields"].get(field).is_some(), "{field} = {value}: {}", response.body);
    }

    assert_eq!(app.post("/api/v1/users", &owner.access, valid.clone()).await.status, StatusCode::CREATED);
    let duplicate = app.post("/api/v1/users", &owner.access, valid).await;
    assert_eq!(duplicate.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(duplicate.body["error"]["fields"]["login"], "taken");
}

#[sqlx::test(migrations = "./migrations")]
async fn owner_edits_worker_and_change_is_logged(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let worker_id = app.create_worker(&owner, "ivan", 1).await;

    let response = app
        .request(
            "PATCH",
            &format!("/api/v1/users/{worker_id}"),
            Some(&owner.access),
            Some(json!({ "permissions": 18, "last_name": "Новий" })),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.body["permissions"], 18);
    assert_eq!(response.body["last_name"], "Новий");
    assert_eq!(response.body["first_name"], "Іван", "поля, которых нет в запросе, не меняются");

    let invalid = app
        .request(
            "PATCH",
            &format!("/api/v1/users/{worker_id}"),
            Some(&owner.access),
            Some(json!({ "permissions": 64 })),
        )
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);

    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM audit_log WHERE action = 'user.update'")
            .fetch_one(&app.admin)
            .await
            .unwrap();
    assert_eq!(details["before"]["permissions"], 1);
    assert_eq!(details["after"]["permissions"], 18);

    let missing = app
        .request(
            "PATCH",
            &format!("/api/v1/users/{}", Uuid::new_v4()),
            Some(&owner.access),
            Some(json!({ "first_name": "X" })),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

// --- Временный пароль, блокировка -------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn temporary_password_allows_only_changing_it(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    app.create_worker(&owner, "ivan", 0).await;
    let session = app.login("ivan", WORKER_PASSWORD).await;

    let blocked = app.get("/api/v1/warehouses", &session.access).await;
    assert_eq!(blocked.status, StatusCode::FORBIDDEN);
    assert_eq!(blocked.code(), "password_change_required");
    let me = app.get("/api/v1/me", &session.access).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["must_change_password"], true);

    let change = |current: &'static str, new: &'static str| {
        app.post(
            "/api/v1/auth/change-password",
            &session.access,
            json!({ "current_password": current, "new_password": new }),
        )
    };
    assert_eq!(change("wrong-password", "new-password-1").await.body["error"]["fields"]["current_password"], "invalid");
    assert_eq!(change(WORKER_PASSWORD, "short").await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(change(WORKER_PASSWORD, WORKER_PASSWORD).await.body["error"]["fields"]["new_password"], "same_as_current");

    let changed = change(WORKER_PASSWORD, "new-password-1").await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    assert_eq!(changed.body["user"]["must_change_password"], false);
    let access = changed.body["access_token"].as_str().unwrap();
    assert_eq!(app.get("/api/v1/warehouses", access).await.status, StatusCode::OK);

    // Старая сессия закрыта, старый пароль не подходит, новый подходит.
    assert_eq!(refresh(&app, &session.refresh, session.device_id).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.try_login("ivan", WORKER_PASSWORD, Uuid::new_v4()).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.try_login("ivan", "new-password-1", Uuid::new_v4()).await.status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn blocking_cuts_access_immediately_and_keeps_history(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let worker = app.active_worker(&owner, "ivan").await;
    let path = format!("/api/v1/users/{}", worker.user_id);

    let blocked = app.post(&format!("{path}/block"), &owner.access, json!({})).await;
    assert_eq!(blocked.status, StatusCode::OK);
    assert_eq!(blocked.body["is_active"], false);

    // Уже выданный access-токен перестаёт работать сразу, а не через 15 минут.
    let me = app.get("/api/v1/me", &worker.access).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
    assert_eq!(me.code(), "user_blocked");
    assert_eq!(refresh(&app, &worker.refresh, worker.device_id).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.try_login("ivan", "worker-password-2", Uuid::new_v4()).await.code(), "user_blocked");
    assert_eq!(app.post(&format!("{path}/block"), &owner.access, json!({})).await.code(), "already_blocked");

    let (open_blocks, live_tokens): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM user_blocks WHERE user_id = $1 AND unblocked_at IS NULL AND blocked_by = $2),
                (SELECT count(*) FROM refresh_tokens WHERE user_id = $1 AND revoked_at IS NULL)",
    )
    .bind(worker.user_id)
    .bind(owner.user_id)
    .fetch_one(&app.admin)
    .await
    .unwrap();
    assert_eq!((open_blocks, live_tokens), (1, 0));

    let unblocked = app.post(&format!("{path}/unblock"), &owner.access, json!({})).await;
    assert_eq!(unblocked.body["is_active"], true);
    assert_eq!(app.post(&format!("{path}/unblock"), &owner.access, json!({})).await.code(), "not_blocked");
    assert_eq!(app.try_login("ivan", "worker-password-2", Uuid::new_v4()).await.status, StatusCode::OK);

    // Период блокировки остался в истории закрытым.
    let closed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM user_blocks WHERE user_id = $1 AND unblocked_at IS NOT NULL AND unblocked_by = $2",
    )
    .bind(worker.user_id)
    .bind(owner.user_id)
    .fetch_one(&app.admin)
    .await
    .unwrap();
    assert_eq!(closed, 1);
    assert_eq!(app.audit_count("user.block").await, 1);
    assert_eq!(app.audit_count("user.unblock").await, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn owner_resets_worker_password(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let worker = app.active_worker(&owner, "ivan").await;

    let reset = app
        .post(
            &format!("/api/v1/users/{}/reset-password", worker.user_id),
            &owner.access,
            json!({ "new_password": "temporary-pass-1" }),
        )
        .await;
    assert_eq!(reset.status, StatusCode::OK);
    assert_eq!(reset.body["must_change_password"], true);

    assert_eq!(refresh(&app, &worker.refresh, worker.device_id).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.try_login("ivan", "worker-password-2", Uuid::new_v4()).await.status, StatusCode::UNAUTHORIZED);
    let session = app.login("ivan", "temporary-pass-1").await;
    assert_eq!(app.get("/api/v1/warehouses", &session.access).await.code(), "password_change_required");
}

#[sqlx::test(migrations = "./migrations")]
async fn second_login_on_a_device_replaces_the_first_user(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let worker = app.active_worker(&owner, "ivan").await;

    // На том же устройстве входит владелец.
    let response = app.try_login("owner", OWNER_PASSWORD, worker.device_id).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(app.get("/api/v1/me", &worker.access).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refresh(&app, &worker.refresh, worker.device_id).await.status, StatusCode::UNAUTHORIZED);
}

// --- Журнал -----------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn audit_log_is_listed_and_filtered(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let worker_id = app.create_worker(&owner, "ivan", 0).await;
    app.post(&format!("/api/v1/users/{worker_id}/block"), &owner.access, json!({})).await;

    let all = app.get("/api/v1/audit-log", &owner.access).await;
    assert_eq!(all.status, StatusCode::OK);
    let first = &all.body["items"][0];
    assert_eq!(first["action"], "user.block", "сначала самые новые записи");
    assert_eq!(first["user_full_name"], "Олена Власник");
    assert_eq!(first["entity_id"], worker_id.to_string());
    assert!(first["occurred_at"].is_string() && first["synced_at"].is_string());

    let by_prefix = app.get("/api/v1/audit-log?action=user", &owner.access).await;
    assert_eq!(by_prefix.body["total"], 2, "user.create и user.block");
    let exact = app.get("/api/v1/audit-log?action=user.block&per_page=1", &owner.access).await;
    assert_eq!(exact.body["total"], 1);
    let by_user = app.get(&format!("/api/v1/audit-log?user_id={}", owner.user_id), &owner.access).await;
    assert_eq!(by_user.body["total"], 3, "вход владельца и два его действия");
    let future = app.get("/api/v1/audit-log?from=2100-01-01T00:00:00Z", &owner.access).await;
    assert_eq!(future.body["total"], 0);

    for bad in ["page=0", "per_page=1000", "user_id=abc", "from=yesterday"] {
        let response = app.get(&format!("/api/v1/audit-log?{bad}"), &owner.access).await;
        assert!(response.status.is_client_error(), "{bad}: {}", response.status);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn audit_log_cannot_be_changed_by_anyone(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    app.owner().await;
    assert!(app.audit_count("auth.login").await > 0);

    // Роль сервера: прав на изменение и удаление журнала нет вовсе.
    for statement in ["UPDATE audit_log SET action = 'x'", "DELETE FROM audit_log", "TRUNCATE audit_log"] {
        let error = sqlx::query(statement).execute(&app.app_db).await.unwrap_err();
        assert!(error.to_string().contains("permission denied"), "{statement}: {error}");
    }
    // Роль сервера не может удалять и сотрудников, склады, историю блокировок.
    for table in ["users", "warehouses", "user_blocks", "devices"] {
        let statement = format!("DELETE FROM {table}");
        let error = sqlx::query(sqlx::AssertSqlSafe(statement)).execute(&app.app_db).await.unwrap_err();
        assert!(error.to_string().contains("permission denied"), "{table}: {error}");
    }
    // Даже владелец базы упирается в триггер.
    for statement in ["UPDATE audit_log SET action = 'x'", "DELETE FROM audit_log"] {
        let error = sqlx::query(statement).execute(&app.admin).await.unwrap_err();
        assert!(error.to_string().contains("нельзя изменять"), "{statement}: {error}");
    }
    assert!(app.audit_count("auth.login").await > 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn audit_log_accepts_rows_for_coming_months(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;

    // Повторный вызов под ролью сервера ничего не ломает.
    budmaster_server::audit::ensure_partitions(&app.app_db).await.unwrap();
    budmaster_server::audit::ensure_partitions(&app.app_db).await.unwrap();

    for months in [0, 1, 3] {
        sqlx::query(
            "INSERT INTO audit_log (id, user_full_name, action, occurred_at, synced_at)
             VALUES ($1, 'тест', 'test.future', now(), date_trunc('month', now()) + make_interval(months => $2) + interval '1 day')",
        )
        .bind(Uuid::new_v4())
        .bind(months)
        .execute(&app.app_db)
        .await
        .unwrap_or_else(|error| panic!("месяц +{months}: {error}"));
    }
    assert_eq!(app.audit_count("test.future").await, 3);

    // Роль сервера не может сама создавать таблицы — только через выделенную функцию.
    let error = sqlx::query("CREATE TABLE sneaky (id int)").execute(&app.app_db).await.unwrap_err();
    assert!(error.to_string().contains("permission denied"), "{error}");
}

// --- Схема ------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn change_seq_grows_on_every_change(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let worker_id = app.create_worker(&owner, "ivan", 0).await;

    let seq = || async {
        sqlx::query_scalar::<_, i64>("SELECT change_seq FROM users WHERE id = $1")
            .bind(worker_id)
            .fetch_one(&app.admin)
            .await
            .unwrap()
    };
    let before = seq().await;
    app.request(
        "PATCH",
        &format!("/api/v1/users/{worker_id}"),
        Some(&owner.access),
        Some(json!({ "first_name": "Петро" })),
    )
    .await;
    assert!(seq().await > before);
}

#[sqlx::test(migrations = "./migrations")]
async fn database_rejects_broken_rows(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;

    // Работник без склада.
    let no_warehouse = sqlx::query(
        "INSERT INTO users (id, login, password_hash, first_name, last_name, role) VALUES ($1, 'ivan', 'x', 'a', 'b', 'worker')",
    )
    .bind(Uuid::new_v4())
    .execute(&app.admin)
    .await;
    assert!(no_warehouse.is_err());

    // Логин с заглавными буквами или пробелами.
    for login in ["Ivan", "iv an", "ab"] {
        let result = sqlx::query(
            "INSERT INTO users (id, login, password_hash, first_name, last_name, role) VALUES ($1, $2, 'x', 'a', 'b', 'owner')",
        )
        .bind(Uuid::new_v4())
        .bind(login)
        .execute(&app.admin)
        .await;
        assert!(result.is_err(), "{login}");
    }
}

// --- Команды сервера --------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn cli_manages_owners(pool: PgPoolOptions, conn: PgConnectOptions) {
    use budmaster_server::cli::{self, NewOwner};

    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    let new_owner = |login: &str, password: &str| NewOwner {
        login: login.to_owned(),
        first_name: "А".to_owned(),
        last_name: "Б".to_owned(),
        password: password.to_owned(),
    };
    assert!(cli::create_owner(&app.app_db, new_owner("owner", OWNER_PASSWORD)).await.is_err(), "логин занят");
    assert!(cli::create_owner(&app.app_db, new_owner("owner3", "short")).await.is_err(), "короткий пароль");
    assert!(cli::create_warehouse(&app.app_db, "Склад 1", None).await.is_err(), "склад с таким названием есть");

    cli::reset_owner_password(&app.app_db, "owner", "brand-new-password".to_owned()).await.unwrap();
    assert_eq!(app.get("/api/v1/me", &owner.access).await.status, StatusCode::UNAUTHORIZED, "сессии закрыты сразу");
    assert_eq!(refresh(&app, &owner.refresh, owner.device_id).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.try_login("owner", OWNER_PASSWORD, Uuid::new_v4()).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.try_login("owner", "brand-new-password", Uuid::new_v4()).await.status, StatusCode::OK);

    // Команда для владельцев не трогает работников.
    let owner = app.login("owner", "brand-new-password").await;
    app.create_worker(&owner, "ivan", 0).await;
    assert!(cli::reset_owner_password(&app.app_db, "ivan", "brand-new-password".to_owned()).await.is_err());
    assert_eq!(app.audit_count("owner.create").await, 1);
    assert_eq!(app.audit_count("owner.password_reset").await, 1);
}
