//! Тестовые данные. Тот же генератор и seed, что в client/ui/src/mock.ts,
//! чтобы замеры Tauri и Slint были на одинаковых данных.

pub struct User {
    pub name: &'static str,
    pub color: u32,
}

pub struct Image {
    pub w: u32,
    pub h: u32,
    pub color: u32,
}

pub struct Message {
    pub author: usize,
    pub ts_ms: i64,
    pub text: String,
    pub image: Option<Image>,
}

pub const NAMES: &[&str] = &[
    "Kerrigan", "Шторм", "nightowl", "Ратибор", "pixel_witch", "Кузя", "Vortex",
    "Лиса", "tankman", "Мирон", "eclipse", "Grom", "Аня", "headshot_pro", "Борис",
];
pub const COLORS: &[u32] = &[0xe8a14b, 0x6cb6ff, 0x7ee08a, 0xf07178, 0xc792ea, 0x5ccfe6, 0xffcb6b];
pub const PHRASES: &[&str] = &[
    "го в рейд через 10 минут",
    "кто на арену?",
    "я на респе, подождите",
    "лол",
    "только что выбил легендарку!!!",
    "у кого-нибудь лагает сервер?",
    "нужен хил в пати, пишите в лс",
    "завтра турнир в 19:00 по мск, регистрация до 18:00, не забудьте проверить микрофоны",
    "gg wp",
    "кто знает, как пройти третьего босса? уже пятый вайп подряд, танк не держит агро на второй фазе",
    "ок",
    "щас буду",
    "скиньте ссылку на билд",
    "давайте в голосовой",
    "патч вышел, качаем",
];

/// Порт mulberry32 из mock.ts
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Self {
        Self(seed)
    }
    pub fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b79f5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(1 | t);
        t = (t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t))) ^ t;
        ((t ^ (t >> 14)) as f64) / 4294967296.0
    }
    pub fn pick(&mut self, len: usize) -> usize {
        (self.next() * len as f64) as usize
    }
}

pub fn users() -> Vec<User> {
    NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| User { name, color: COLORS[i % COLORS.len()] })
        .collect()
}

pub fn generate(count: usize, now_ms: i64, rng: &mut Rng) -> Vec<Message> {
    let mut out = Vec::with_capacity(count);
    let mut ts = now_ms - count as i64 * 40_000;
    let mut author = 0;
    for _ in 0..count {
        // Часто один человек пишет несколько сообщений подряд
        if rng.next() > 0.45 {
            author = rng.pick(NAMES.len());
        }
        ts += (rng.next() * 80_000.0) as i64;
        let parts = 1 + (rng.next() * rng.next() * 4.0) as usize;
        let text = (0..parts)
            .map(|_| PHRASES[rng.pick(PHRASES.len())])
            .collect::<Vec<_>>()
            .join(". ");
        let image = (rng.next() < 0.05).then(|| {
            let landscape = rng.next() > 0.3;
            Image {
                w: if landscape { 1280 } else { 720 },
                h: if landscape { 720 } else { 1280 },
                color: COLORS[rng.pick(COLORS.len())],
            }
        });
        out.push(Message { author, ts_ms: ts, text, image });
    }
    out
}
