use rand::seq::SliceRandom;

const SUPPORTED_DOMAINS: &[&str] = &[
    "youtu.be",
    "facebook.com",
    "x.com",
    "twitter.com",
    "tiktok.com",
    "instagram.com",
    "youtube.com",
    "reddit.com",
    "linkedin.com",
];

const GREETINGS: &[&str] = &[
    "краљу",
    "баки",
    "царе",
    "легендице",
    "друже",
    "чоче",
    "јадо",
    "мајсторе",
    "легендо",
    "брате",
    "геније",
    "душо",
    "сине",
    "мангупе",
    "фрајеру",
    "братко",
    "батко",
    "срце",
    "пријатељу",
    "комшо",
    "комшија",
];

pub fn random_greeting() -> &'static str {
    GREETINGS
        .choose(&mut rand::thread_rng())
        .copied()
        .unwrap_or("друже")
}

pub fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(ch) => ch.to_uppercase().collect::<String>() + c.as_str(),
    }
}

pub fn is_supported_url(text: &str) -> bool {
    SUPPORTED_DOMAINS.iter().any(|d| text.contains(d))
}
