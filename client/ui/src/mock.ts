// Тестовые данные для прототипа этапа 0. Детерминированные (seed), чтобы замеры были сравнимы.

export type User = { id: number; name: string; color: string };
export type Attachment = { w: number; h: number; color: string };
export type Message = {
  id: number;
  authorId: number;
  ts: number;
  text: string;
  image?: Attachment;
};

function mulberry32(seed: number) {
  return () => {
    seed |= 0;
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const NAMES = [
  "Kerrigan", "Шторм", "nightowl", "Ратибор", "pixel_witch", "Кузя", "Vortex",
  "Лиса", "tankman", "Мирон", "eclipse", "Grom", "Аня", "headshot_pro", "Борис",
];
const COLORS = ["#e8a14b", "#6cb6ff", "#7ee08a", "#f07178", "#c792ea", "#5ccfe6", "#ffcb6b"];
const PHRASES = [
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

export function generate(count: number, seed = 42) {
  const rnd = mulberry32(seed);
  const users: User[] = NAMES.map((name, id) => ({
    id,
    name,
    color: COLORS[id % COLORS.length],
  }));
  const messages: Message[] = [];
  let ts = Date.now() - count * 40_000;
  let author = 0;
  for (let id = 0; id < count; id++) {
    // Часто один человек пишет несколько сообщений подряд
    if (rnd() > 0.45) author = Math.floor(rnd() * users.length);
    ts += Math.floor(rnd() * 80_000);
    const parts = 1 + Math.floor(rnd() * rnd() * 4);
    const text = Array.from({ length: parts }, () => PHRASES[Math.floor(rnd() * PHRASES.length)]).join(". ");
    const msg: Message = { id, authorId: author, ts, text };
    if (rnd() < 0.05) {
      const landscape = rnd() > 0.3;
      msg.image = {
        w: landscape ? 1280 : 720,
        h: landscape ? 720 : 1280,
        color: COLORS[Math.floor(rnd() * COLORS.length)],
      };
    }
    messages.push(msg);
  }
  return { users, messages };
}

export const LIVE_PHRASES = PHRASES;
