//! Адрес категории на сайте (`/catalog?category=pylomaterialy`) из её украинского названия.

use sqlx::PgConnection;

const MAX_LEN: usize = 60;

/// «Цемент і сухі суміші» → `tsement-i-sukhi-sumishi`.
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        match transliterate(c) {
            Some(text) => slug.push_str(text),
            None if c.is_ascii_alphanumeric() => slug.push(c),
            // Любой другой символ — разделитель; подряд идущие склеиваются в один дефис.
            None if !slug.is_empty() && !slug.ends_with('-') => slug.push('-'),
            None => {}
        }
    }
    let mut slug: String = slug.chars().take(MAX_LEN).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() { "category".to_owned() } else { slug }
}

/// Украинская (и на всякий случай русская) кириллица латиницей. Апостроф и мягкий знак пропадают.
fn transliterate(c: char) -> Option<&'static str> {
    Some(match c {
        'а' => "a",
        'б' => "b",
        'в' => "v",
        'г' => "h",
        'ґ' => "g",
        'д' => "d",
        'е' | 'э' => "e",
        'є' => "ye",
        'ё' => "yo",
        'ж' => "zh",
        'з' => "z",
        'и' | 'ы' => "y",
        'і' => "i",
        'ї' => "yi",
        'й' => "y",
        'к' => "k",
        'л' => "l",
        'м' => "m",
        'н' => "n",
        'о' => "o",
        'п' => "p",
        'р' => "r",
        'с' => "s",
        'т' => "t",
        'у' => "u",
        'ф' => "f",
        'х' => "kh",
        'ц' => "ts",
        'ч' => "ch",
        'ш' => "sh",
        'щ' => "shch",
        'ю' => "yu",
        'я' => "ya",
        'ь' | 'ъ' | '\'' | '’' | 'ʼ' => "",
        _ => return None,
    })
}

/// Свободный адрес: если `tsehla` занят, пробует `tsehla-2`, `tsehla-3`…
pub async fn unique_slug(conn: &mut PgConnection, name: &str) -> sqlx::Result<String> {
    let base = slugify(name);
    for attempt in 1..=200 {
        let candidate = if attempt == 1 { base.clone() } else { format!("{base}-{attempt}") };
        let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM categories WHERE slug = $1)")
            .bind(&candidate)
            .fetch_one(&mut *conn)
            .await?;
        if !taken {
            return Ok(candidate);
        }
    }
    // Двести одноимённых категорий — не рабочая ситуация; адрес всё равно будет уникальным
    // и уложится в 80 символов.
    let short: String = base.chars().take(40).collect();
    Ok(format!("{}-{}", short.trim_end_matches('-'), uuid::Uuid::new_v4().simple()))
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn ukrainian_names_become_latin_slugs() {
        assert_eq!(slugify("Пиломатеріали"), "pylomaterialy");
        assert_eq!(slugify("Цемент і сухі суміші"), "tsement-i-sukhi-sumishi");
        assert_eq!(slugify("Гіпсокартон і профілі"), "hipsokarton-i-profili");
        assert_eq!(slugify("Кріплення"), "kriplennia".replace("ia", "ya"));
        assert_eq!(slugify("  Об’єкти / Щебінь №5  "), "obyekty-shchebin-5");
        assert_eq!(slugify("Rainway 90 мм"), "rainway-90-mm");
    }

    #[test]
    fn slug_is_never_empty_or_too_long() {
        assert_eq!(slugify("!!!"), "category");
        assert_eq!(slugify(""), "category");
        let long = slugify(&"я".repeat(200));
        assert!(long.len() <= 60 && !long.ends_with('-'));
        assert!(slugify("a - - b").eq("a-b"));
    }
}
