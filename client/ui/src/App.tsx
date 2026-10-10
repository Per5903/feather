import { For, Show, createSignal, onCleanup, onMount } from "solid-js";
import { MessageList } from "./MessageList";
import { LIVE_PHRASES, generate, type Message } from "./mock";

const MESSAGE_COUNT = 10_000;
// Имитация живого чата: входящее сообщение раз в N мс
const LIVE_INTERVAL_MS = 1500;

const SERVERS = ["Ф", "R", "Кл", "WoW", "CS"];
const CHANNELS = [
  { cat: "Текстовые", items: ["# общий", "# рейды", "# мемы", "# билды"] },
  { cat: "Голосовые", items: ["🔊 Лобби", "🔊 Рейд", "🔊 AFK"] },
];

export function App() {
  const { users, messages } = generate(MESSAGE_COUNT);
  const [count, setCount] = createSignal(messages.length);
  const [diag, setDiag] = createSignal(false);
  const [stats, setStats] = createSignal("");

  // Пока окно скрыто (игра на переднем плане), входящие копятся в буфере
  // и не трогают DOM. При возврате выводятся одной пачкой.
  const pending: Message[] = [];
  const flush = () => {
    if (pending.length === 0 || document.hidden) return;
    messages.push(...pending.splice(0));
    setCount(messages.length);
  };

  const push = (authorId: number, text: string) => {
    pending.push({ id: messages.length + pending.length, authorId, ts: Date.now(), text });
    flush();
  };

  onMount(() => {
    const live = setInterval(() => {
      const author = Math.floor(Math.random() * users.length);
      push(author, LIVE_PHRASES[Math.floor(Math.random() * LIVE_PHRASES.length)]);
    }, LIVE_INTERVAL_MS);
    document.addEventListener("visibilitychange", flush);

    const onKey = (e: KeyboardEvent) => {
      if (e.key === "F2") setDiag((d) => !d);
    };
    window.addEventListener("keydown", onKey);

    // Диагностика считается только когда панель открыта и окно видно
    const diagTimer = setInterval(() => {
      if (!diag() || document.hidden) return;
      const nodes = document.getElementsByTagName("*").length;
      const mem = (performance as any).memory?.usedJSHeapSize;
      setStats(
        `DOM-узлов: ${nodes} · сообщений: ${messages.length}` +
          (mem ? ` · JS heap: ${(mem / 1048576).toFixed(1)} МБ` : ""),
      );
    }, 1000);

    onCleanup(() => {
      clearInterval(live);
      clearInterval(diagTimer);
      document.removeEventListener("visibilitychange", flush);
      window.removeEventListener("keydown", onKey);
    });
  });

  let input!: HTMLTextAreaElement;
  const send = (e: KeyboardEvent) => {
    if (e.key !== "Enter" || e.shiftKey) return;
    e.preventDefault();
    const text = input.value.trim();
    if (!text) return;
    input.value = "";
    push(0, text);
  };

  return (
    <div class="app">
      <nav class="servers">
        <For each={SERVERS}>
          {(s, i) => <div class="server" classList={{ active: i() === 0 }}>{s}</div>}
        </For>
      </nav>

      <aside class="channels">
        <header class="channels-head">Феникс · гильдия</header>
        <For each={CHANNELS}>
          {(group) => (
            <section>
              <div class="cat">{group.cat}</div>
              <For each={group.items}>
                {(c, i) => (
                  <div class="channel" classList={{ active: group.cat === "Текстовые" && i() === 0 }}>
                    {c}
                  </div>
                )}
              </For>
            </section>
          )}
        </For>
        <footer class="me">
          <div class="avatar small" style={{ background: users[0].color }}>
            {users[0].name[0]}
          </div>
          <span>{users[0].name}</span>
        </footer>
      </aside>

      <main class="chat">
        <header class="chat-head"># общий</header>
        <MessageList messages={messages} count={count} users={users} />
        <div class="composer">
          <textarea ref={input} rows={1} placeholder="Написать в #общий" onKeyDown={send} />
        </div>
      </main>

      <aside class="members">
        <div class="cat">В сети — {users.length}</div>
        <For each={users}>
          {(u) => (
            <div class="member">
              <div class="avatar small" style={{ background: u.color }}>{u.name[0].toUpperCase()}</div>
              <span>{u.name}</span>
            </div>
          )}
        </For>
      </aside>

      <Show when={diag()}>
        <div class="diag">{stats() || "…"}</div>
      </Show>
    </div>
  );
}
