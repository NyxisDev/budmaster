//! Ограничение попыток входа: не больше 5 неудач за 15 минут на пару «логин + IP»
//! и не больше 30 неудач за 15 минут с одного IP на любые логины.
//!
//! Попытка засчитывается в момент начала, а не после проверки пароля: иначе пачка
//! одновременных запросов вся проскочила бы проверку раньше, чем запишется первая неудача.
//! Успешный вход свою попытку снимает.
//!
//! Счётчики живут в памяти процесса: сервер один, а после перезапуска окно просто
//! начинается заново.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(15 * 60);
const MAX_PER_LOGIN: usize = 5;
const MAX_PER_IP: usize = 30;
/// Сколько ключей держим в памяти.
const MAX_KEYS: usize = 50_000;

#[derive(Default)]
pub struct LoginLimiter {
    inner: Mutex<State>,
}

#[derive(Default)]
struct State {
    by_login: HashMap<(String, IpAddr), VecDeque<Instant>>,
    by_ip: HashMap<IpAddr, VecDeque<Instant>>,
}

impl LoginLimiter {
    /// Начинает попытку входа. `Err(секунд до разблокировки)`, если лимит исчерпан.
    pub fn begin(&self, login: &str, ip: IpAddr) -> Result<(), u64> {
        self.begin_at(login, ip, Instant::now())
    }

    /// Успешный вход: счётчик пары «логин + IP» сбрасывается, попытка с IP снимается.
    pub fn record_success(&self, login: &str, ip: IpAddr) {
        let mut state = self.lock();
        state.by_login.remove(&(normalize(login), ip));
        if let Some(hits) = state.by_ip.get_mut(&ip) {
            hits.pop_back();
        }
    }

    fn begin_at(&self, login: &str, ip: IpAddr, now: Instant) -> Result<(), u64> {
        let mut state = self.lock();
        let key = (normalize(login), ip);

        let wait_login = state.by_login.get_mut(&key).and_then(|hits| wait_time(hits, MAX_PER_LOGIN, now));
        let wait_ip = state.by_ip.get_mut(&ip).and_then(|hits| wait_time(hits, MAX_PER_IP, now));
        if let Some(wait) = wait_login.max(wait_ip) {
            return Err(wait.as_secs().max(1));
        }

        if state.by_login.len() >= MAX_KEYS || state.by_ip.len() >= MAX_KEYS {
            let fresh = |hits: &mut VecDeque<Instant>| hits.back().is_some_and(|last| now.duration_since(*last) < WINDOW);
            state.by_login.retain(|_, hits| fresh(hits));
            state.by_ip.retain(|_, hits| fresh(hits));
            // Если и после чистки места нет (атака с огромного числа адресов), начинаем
            // с чистого листа: память сервера важнее точности счётчиков.
            if state.by_login.len() >= MAX_KEYS || state.by_ip.len() >= MAX_KEYS {
                state.by_login.clear();
                state.by_ip.clear();
            }
        }

        state.by_login.entry(key).or_default().push_back(now);
        state.by_ip.entry(ip).or_default().push_back(now);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // Отравленный мьютекс не должен навсегда ломать вход: данные счётчиков не критичны.
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Убирает устаревшие попытки и возвращает, сколько ждать, если лимит исчерпан.
fn wait_time(hits: &mut VecDeque<Instant>, max: usize, now: Instant) -> Option<Duration> {
    while hits.front().is_some_and(|first| now.duration_since(*first) >= WINDOW) {
        hits.pop_front();
    }
    if hits.len() < max {
        return None;
    }
    // Место освободится, когда самая старая из последних `max` попыток выйдет из окна.
    let oldest = hits[hits.len() - max];
    Some(WINDOW.saturating_sub(now.duration_since(oldest)))
}

/// Логин длиной до 64 символов в нижнем регистре: длинный мусор не раздувает память.
fn normalize(login: &str) -> String {
    login.trim().chars().take(64).flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
    const OTHER_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));

    #[test]
    fn blocks_after_five_attempts_and_recovers_after_window() {
        let limiter = LoginLimiter::default();
        let start = Instant::now();
        for i in 0..5 {
            assert!(limiter.begin_at("Ivan", IP, start).is_ok(), "попытка {i}");
        }
        assert!(limiter.begin_at("IVAN ", IP, start).is_err(), "регистр и пробелы не обходят лимит");
        assert!(limiter.begin_at("ivan", OTHER_IP, start).is_ok(), "другой IP не заблокирован");
        assert!(limiter.begin_at("petro", IP, start).is_ok(), "другой логин не заблокирован");
        assert!(limiter.begin_at("ivan", IP, start + WINDOW).is_ok(), "после окна доступ возвращается");
    }

    #[test]
    fn simultaneous_attempts_cannot_slip_past_the_limit() {
        // Попытка считается сразу при начале, ещё до проверки пароля.
        let limiter = LoginLimiter::default();
        let now = Instant::now();
        let allowed = (0..100).filter(|_| limiter.begin_at("ivan", IP, now).is_ok()).count();
        assert_eq!(allowed, MAX_PER_LOGIN);
    }

    #[test]
    fn success_resets_the_counter() {
        let limiter = LoginLimiter::default();
        let now = Instant::now();
        for _ in 0..5 {
            limiter.begin_at("ivan", IP, now).unwrap();
        }
        limiter.record_success("ivan", IP);
        assert!(limiter.begin_at("ivan", IP, now).is_ok());
    }

    #[test]
    fn one_ip_cannot_try_many_logins() {
        let limiter = LoginLimiter::default();
        let now = Instant::now();
        for i in 0..MAX_PER_IP {
            limiter.begin_at(&format!("user{i}"), IP, now).unwrap();
        }
        assert!(limiter.begin_at("someone-new", IP, now).is_err());
        assert!(limiter.begin_at("someone-new", OTHER_IP, now).is_ok());
    }
}
