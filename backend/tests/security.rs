//! Тесты на сценарии атак и гонок: то, что должно быть невозможно.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{OWNER_PASSWORD, TestApp, WORKER_PASSWORD};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

async fn refresh(app: &TestApp, token: &str, device_id: Uuid) -> common::Response {
    app.request(
        "POST",
        "/api/v1/auth/refresh",
        None,
        Some(json!({ "refresh_token": token, "device_id": device_id })),
    )
    .await
}

// --- Access-токен умирает вместе с сессией ----------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn stolen_access_token_dies_when_password_is_changed(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    let changed = app
        .post(
            "/api/v1/auth/change-password",
            &owner.access,
            json!({ "current_password": OWNER_PASSWORD, "new_password": "owner-password-2" }),
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);

    // Токен, выданный до смены пароля (его мог скопировать вор), больше не принимается —
    // хотя устройство то же и срок токена ещё не вышел.
    let old = app.get("/api/v1/me", &owner.access).await;
    assert_eq!(old.status, StatusCode::UNAUTHORIZED);
    assert_eq!(old.code(), "invalid_token");
    assert_eq!(app.get("/api/v1/me", changed.body["access_token"].as_str().unwrap()).await.status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn old_access_token_is_not_revived_by_a_new_login(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let worker = app.active_worker(&owner, "ivan").await;

    // Выход и повторный вход с того же устройства.
    app.post("/api/v1/auth/logout", &worker.access, json!({})).await;
    let again = app.try_login("ivan", "worker-password-2", worker.device_id).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(app.get("/api/v1/me", &worker.access).await.status, StatusCode::UNAUTHORIZED, "после выхода");
    let second_access = again.body["access_token"].as_str().unwrap().to_owned();

    // Сброс пароля владельцем и вход с временным паролем на том же устройстве.
    let path = format!("/api/v1/users/{}", worker.user_id);
    app.post(&format!("{path}/reset-password"), &owner.access, json!({ "new_password": "temporary-pass-1" })).await;
    assert_eq!(app.get("/api/v1/me", &second_access).await.status, StatusCode::UNAUTHORIZED, "сразу после сброса");
    let third = app.try_login("ivan", "temporary-pass-1", worker.device_id).await;
    assert_eq!(third.status, StatusCode::OK);
    assert_eq!(app.get("/api/v1/me", &second_access).await.status, StatusCode::UNAUTHORIZED, "после нового входа");
    let third_access = third.body["access_token"].as_str().unwrap().to_owned();

    // Блокировка и разблокировка: токен, выданный до блокировки, не возвращается к жизни.
    app.post(&format!("{path}/block"), &owner.access, json!({})).await;
    app.post(&format!("{path}/unblock"), &owner.access, json!({})).await;
    assert_eq!(app.get("/api/v1/me", &third_access).await.status, StatusCode::UNAUTHORIZED, "после разблокировки");
}

// --- Права ------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn full_permission_mask_does_not_make_a_worker_an_owner(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    let worker_id = app.create_worker(&owner, "ivan", 31).await;
    let first = app.login("ivan", WORKER_PASSWORD).await;
    let changed = app
        .post(
            "/api/v1/auth/change-password",
            &first.access,
            json!({ "current_password": WORKER_PASSWORD, "new_password": "worker-password-2" }),
        )
        .await;
    let access = changed.body["access_token"].as_str().unwrap();
    assert_eq!(changed.body["user"]["permissions"], 31);

    for (method, path) in [
        ("GET", "/api/v1/users".to_owned()),
        ("GET", "/api/v1/audit-log".to_owned()),
        ("POST", format!("/api/v1/users/{worker_id}/block")),
        ("PATCH", format!("/api/v1/users/{worker_id}")),
    ] {
        let response = app.request(method, &path, Some(access), Some(json!({ "permissions": 31 }))).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{method} {path}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn server_role_cannot_change_role_or_login(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;
    app.create_worker(&owner, "ivan", 0).await;

    // Даже если бы в коде сервера нашлась ошибка, база не даст сделать работника владельцем.
    for statement in [
        "UPDATE users SET role = 'owner' WHERE login = 'ivan'",
        "UPDATE users SET login = 'owner2' WHERE login = 'ivan'",
        "UPDATE users SET id = gen_random_uuid() WHERE login = 'ivan'",
    ] {
        let error = sqlx::query(statement).execute(&app.app_db).await.unwrap_err();
        assert!(error.to_string().contains("permission denied"), "{statement}: {error}");
    }
    // А маску с неизвестными битами не примет и сама таблица.
    let error = sqlx::query("UPDATE users SET permissions = 32 WHERE login = 'ivan'")
        .execute(&app.app_db)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("check constraint"), "{error}");
}

// --- Одновременные запросы --------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn burst_of_wrong_passwords_cannot_outrun_the_limit(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    app.create_owner("owner").await;

    let attempts = (0..20).map(|_| app.try_login("owner", "wrong-password", Uuid::new_v4()));
    let responses = futures_join_all(attempts).await;

    let checked = responses.iter().filter(|r| r.status == StatusCode::UNAUTHORIZED).count();
    let limited = responses.iter().filter(|r| r.status == StatusCode::TOO_MANY_REQUESTS).count();
    assert_eq!((checked, limited), (5, 15), "до проверки пароля дошли только 5 запросов из 20");
}

#[sqlx::test(migrations = "./migrations")]
async fn simultaneous_refreshes_leave_exactly_one_live_token(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    let responses = futures_join_all((0..8).map(|_| refresh(&app, &owner.refresh, owner.device_id))).await;
    assert!(responses.iter().all(|r| r.status == StatusCode::OK), "быстрые повторы допустимы");

    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM refresh_tokens WHERE used_at IS NULL AND revoked_at IS NULL")
            .fetch_one(&app.admin)
            .await
            .unwrap();
    assert_eq!(live, 1, "ответвлений сессии нет");
}

#[sqlx::test(migrations = "./migrations")]
async fn refresh_racing_with_password_change_does_not_survive_it(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    for round in 0..5 {
        let worker_login = format!("worker{round}");
        let worker = app.active_worker(&owner, &worker_login).await;
        let path = format!("/api/v1/users/{}/reset-password", worker.user_id);

        // Сброс пароля и обновление токена идут одновременно.
        let (reset, rotated) = tokio::join!(
            app.post(&path, &owner.access, json!({ "new_password": "temporary-pass-1" })),
            refresh(&app, &worker.refresh, worker.device_id),
        );
        assert_eq!(reset.status, StatusCode::OK);

        // Кто бы ни успел первым, после сброса у работника не остаётся ни одной живой сессии.
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM refresh_tokens WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(worker.user_id)
        .fetch_one(&app.admin)
        .await
        .unwrap();
        assert_eq!(live, 0, "раунд {round}");
        if rotated.status == StatusCode::OK {
            let access = rotated.body["access_token"].as_str().unwrap();
            assert_eq!(app.get("/api/v1/me", access).await.status, StatusCode::UNAUTHORIZED, "раунд {round}");
        }
    }
}

async fn futures_join_all<F: Future>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut set = Vec::new();
    for future in futures {
        set.push(Box::pin(future));
    }
    let mut results = Vec::with_capacity(set.len());
    // Все запросы стартуют до того, как первый завершится: poll каждого по кругу.
    let mut pending: Vec<_> = set.into_iter().map(Some).collect();
    let mut outputs: Vec<Option<F::Output>> = (0..pending.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut all_done = true;
        for (slot, output) in pending.iter_mut().zip(outputs.iter_mut()) {
            if let Some(future) = slot {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => {
                        *output = Some(value);
                        *slot = None;
                    }
                    std::task::Poll::Pending => all_done = false,
                }
            }
        }
        if all_done { std::task::Poll::Ready(()) } else { std::task::Poll::Pending }
    })
    .await;
    results.extend(outputs.into_iter().map(|output| output.expect("все завершились")));
    results
}

// --- Журнал и ввод ----------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn failed_login_does_not_store_arbitrary_text_in_the_log(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    app.create_owner("owner").await;

    // Человек по ошибке набрал пароль в поле логина.
    app.try_login("my-secret-password", "whatever-1", Uuid::new_v4()).await;
    app.try_login("OWNER", "wrong-password", Uuid::new_v4()).await;

    let logins: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT details->>'login' FROM audit_log WHERE action = 'auth.login_failed' ORDER BY synced_at",
    )
    .fetch_all(&app.admin)
    .await
    .unwrap();
    assert_eq!(logins, vec![None, Some("owner".to_owned())], "сохраняется только логин существующего сотрудника");
}

#[sqlx::test(migrations = "./migrations")]
async fn malformed_requests_get_uniform_errors(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    let cases: [(&str, &str, Option<serde_json::Value>, StatusCode, &str); 7] = [
        ("POST", "/api/v1/auth/login", Some(json!({})), StatusCode::BAD_REQUEST, "invalid_body"),
        ("POST", "/api/v1/auth/login", Some(json!({ "login": 1, "password": [], "device_id": "x" })), StatusCode::BAD_REQUEST, "invalid_body"),
        ("POST", "/api/v1/auth/login", None, StatusCode::BAD_REQUEST, "json_expected"),
        ("GET", "/api/v1/auth/login", None, StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed"),
        ("PATCH", "/api/v1/users/not-a-uuid", Some(json!({ "first_name": "X" })), StatusCode::NOT_FOUND, "not_found"),
        ("PATCH", &format!("/api/v1/users/{}", owner.user_id), Some(json!({})), StatusCode::BAD_REQUEST, "nothing_to_update"),
        ("GET", "/api/v1/audit-log?page=abc", None, StatusCode::BAD_REQUEST, "invalid_query"),
    ];
    for (method, path, body, status, code) in cases {
        let response = app.request(method, path, Some(&owner.access), body).await;
        assert_eq!(response.status, status, "{method} {path}: {}", response.body);
        assert_eq!(response.code(), code, "{method} {path}");
    }

    // Битый JSON и слишком большое тело.
    for (body, status, code) in [
        ("{ not json".to_owned(), StatusCode::BAD_REQUEST, "invalid_json"),
        (format!(r#"{{"login":"{}"}}"#, "a".repeat(100_000)), StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
    ] {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = app.raw(request).await;
        assert_eq!(response.status, status);
        assert_eq!(response.code(), code);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn control_characters_never_cause_a_server_error(pool: PgPoolOptions, conn: PgConnectOptions) {
    let app = TestApp::new(pool, conn).await;
    let owner = app.owner().await;

    let nul_login = app.try_login("ow\u{0}ner", OWNER_PASSWORD, Uuid::new_v4()).await;
    assert_eq!(nul_login.status, StatusCode::UNAUTHORIZED);

    // Нулевой байт в модели устройства отбрасывается, вход проходит.
    let nul_model = app
        .request(
            "POST",
            "/api/v1/auth/login",
            None,
            Some(json!({ "login": "owner", "password": OWNER_PASSWORD, "device_id": Uuid::new_v4(), "device_model": "Pi\u{0}xel" })),
        )
        .await;
    assert_eq!(nul_model.status, StatusCode::OK, "{}", nul_model.body);

    let nul_action = app.get("/api/v1/audit-log?action=%00", &owner.access).await;
    assert_eq!(nul_action.status, StatusCode::UNPROCESSABLE_ENTITY);

    let nul_name = app
        .post(
            "/api/v1/users",
            &owner.access,
            json!({
                "login": "ivan", "password": "long-password-1", "first_name": "Iв\u{0}ан", "last_name": "Б",
                "default_warehouse_id": app.warehouse_id,
            }),
        )
        .await;
    assert_eq!(nul_name.status, StatusCode::UNPROCESSABLE_ENTITY);
}
