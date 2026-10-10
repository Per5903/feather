import { createVirtualizer } from "@tanstack/solid-virtual";
import { For, Show, createMemo, createEffect, on, onCleanup, onMount } from "solid-js";
import type { Message, User } from "./mock";

// Новое сообщение от того же автора в пределах 5 минут рисуется без шапки (как в Discord)
const GROUP_GAP_MS = 5 * 60_000;
// Насколько близко к низу нужно быть, чтобы список «прилипал» к новым сообщениям
const STICK_THRESHOLD_PX = 80;

const timeFmt = new Intl.DateTimeFormat("ru-RU", { hour: "2-digit", minute: "2-digit" });

type Props = {
  messages: Message[];
  // Сигнал изменения длины: массив мутируется на месте, чтобы не копировать 10k элементов
  count: () => number;
  users: User[];
};

export function MessageList(props: Props) {
  let scrollEl!: HTMLDivElement;

  const v = createVirtualizer({
    get count() {
      return props.count();
    },
    getScrollElement: () => scrollEl,
    estimateSize: () => 44,
    overscan: 6,
  });

  // Рендерим по индексам: For сопоставляет примитивы по значению,
  // поэтому при прокрутке DOM-узлы видимых строк не пересоздаются.
  const indices = createMemo(() => v.getVirtualItems().map((i) => i.index));
  const starts = createMemo(() => {
    const m = new Map<number, number>();
    for (const i of v.getVirtualItems()) m.set(i.index, i.start);
    return m;
  });

  const isHead = (index: number) => {
    const m = props.messages[index];
    const prev = props.messages[index - 1];
    return !prev || prev.authorId !== m.authorId || m.ts - prev.ts > GROUP_GAP_MS;
  };

  const nearBottom = () =>
    scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight < STICK_THRESHOLD_PX;

  onMount(() => {
    v.scrollToIndex(props.count() - 1, { align: "end" });
    // Режим бенчмарка (#autoscroll): непрерывная прокрутка, как FEATHER_AUTOSCROLL в Slint
    if (location.hash === "#autoscroll") {
      let dir = 1;
      const t = setInterval(() => {
        const max = scrollEl.scrollHeight - scrollEl.clientHeight;
        scrollEl.scrollTop = Math.max(0, Math.min(max, scrollEl.scrollTop + 40 * dir));
        if (scrollEl.scrollTop <= 0 || scrollEl.scrollTop >= max) dir = -dir;
      }, 16);
      onCleanup(() => clearInterval(t));
    }
  });

  let wasNearBottom = true;
  const onScroll = () => (wasNearBottom = nearBottom());

  createEffect(
    on(props.count, (n) => {
      if (wasNearBottom) v.scrollToIndex(n - 1, { align: "end" });
    }, { defer: true }),
  );

  return (
    <div ref={scrollEl} class="messages" onScroll={onScroll}>
      <div style={{ height: `${v.getTotalSize()}px`, position: "relative" }}>
        <For each={indices()}>
          {(index) => {
            const m = props.messages[index];
            const u = props.users[m.authorId];
            const head = isHead(index);
            return (
              <div
                data-index={index}
                // ref срабатывает до установки data-index; замеряем после вставки в DOM
                ref={(el) => queueMicrotask(() => v.measureElement(el))}
                class="msg"
                classList={{ head }}
                style={{ transform: `translateY(${starts().get(index) ?? 0}px)` }}
              >
                <Show
                  when={head}
                  fallback={<span class="msg-time-gutter">{timeFmt.format(m.ts)}</span>}
                >
                  <div class="avatar" style={{ background: u.color }}>
                    {u.name[0].toUpperCase()}
                  </div>
                </Show>
                <div class="msg-body">
                  <Show when={head}>
                    <div class="msg-meta">
                      <span class="msg-author" style={{ color: u.color }}>{u.name}</span>
                      <span class="msg-time">{timeFmt.format(m.ts)}</span>
                    </div>
                  </Show>
                  <div class="msg-text">{m.text}</div>
                  <Show when={m.image}>
                    {(img) => (
                      // Размер блока известен заранее (из метаданных вложения),
                      // поэтому высота строки не прыгает при загрузке картинки.
                      <div
                        class="msg-image"
                        style={{
                          "aspect-ratio": `${img().w} / ${img().h}`,
                          width: img().w > img().h ? "min(400px, 100%)" : "200px",
                          background: img().color,
                        }}
                      >
                        {img().w}×{img().h}
                      </div>
                    )}
                  </Show>
                </div>
              </div>
            );
          }}
        </For>
      </div>
    </div>
  );
}
