//! Генератор Snowflake: 41 бит времени (мс от 2026-01-01) | 10 бит узла | 12 бит счётчика.
//! До 4096 ID в миллисекунду на узел; влезает в BIGINT Postgres ещё ~69 лет.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use feather_protocol::{FEATHER_EPOCH_MS as EPOCH_MS, SNOWFLAKE_TIME_SHIFT, Snowflake};

const SEQ_BITS: u64 = 12;
const WORKER_BITS: u64 = 10;
const _: () = assert!(SEQ_BITS + WORKER_BITS == SNOWFLAKE_TIME_SHIFT as u64);

pub struct Snowflakes {
    worker: u64,
    /// (последняя миллисекунда, счётчик внутри неё)
    state: Mutex<(u64, u64)>,
}

impl Snowflakes {
    pub fn new(worker: u16) -> Self {
        assert!((worker as u64) < (1 << WORKER_BITS), "worker_id должен быть < 1024");
        Self { worker: worker as u64, state: Mutex::new((0, 0)) }
    }

    pub fn next(&self) -> Snowflake {
        let mut st = self.state.lock().unwrap();
        let (last, seq) = *st;
        // Если часы ушли назад, продолжаем с последнего значения, чтобы не выдать дубликат
        let mut now = now_ms().max(last);
        let seq = if now == last {
            let next = (seq + 1) & ((1 << SEQ_BITS) - 1);
            if next == 0 {
                while now <= last {
                    now = now_ms();
                }
            }
            next
        } else {
            0
        };
        *st = (now, seq);
        Snowflake((now << (WORKER_BITS + SEQ_BITS)) | (self.worker << SEQ_BITS) | seq)
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("часы до 1970").as_millis() as u64 - EPOCH_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_increasing() {
        let ids_gen = Snowflakes::new(3);
        let ids: Vec<u64> = (0..20_000).map(|_| ids_gen.next().0).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        assert!(ids.iter().all(|&id| id < i64::MAX as u64));
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        assert!(Snowflake(ids[0]).timestamp_ms().abs_diff(now) < 1000, "время читается обратно из ID");
    }
}
