//! Замеры для логов: сколько раз за период, сколько времени и пикселей.

use std::time::{Duration, Instant};

/// Как часто писать итог в лог.
pub const PERIOD: Duration = Duration::from_secs(5);

pub struct Stats {
    since: Instant,
    count: u32,
    total: Duration,
    max: Duration,
    pixels: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            since: Instant::now(),
            count: 0,
            total: Duration::ZERO,
            max: Duration::ZERO,
            pixels: 0,
        }
    }
}

impl Stats {
    pub fn add(&mut self, spent: Duration, pixels: u64) {
        self.count += 1;
        self.total += spent;
        self.max = self.max.max(spent);
        self.pixels += pixels;
    }

    /// Раз в PERIOD — строка «N/с, среднее X мс, макс Y мс, Z Мпикс/с» и сброс; пусто — None.
    pub fn report(&mut self) -> Option<String> {
        let elapsed = self.since.elapsed();
        if elapsed < PERIOD {
            return None;
        }
        let line = (self.count > 0).then(|| {
            let secs = elapsed.as_secs_f64();
            format!(
                "{:.1}/с, среднее {:.2} мс, макс {:.2} мс, {:.1} Мпикс/с",
                self.count as f64 / secs,
                self.total.as_secs_f64() * 1000.0 / self.count as f64,
                self.max.as_secs_f64() * 1000.0,
                self.pixels as f64 / secs / 1_000_000.0
            )
        });
        *self = Stats::default();
        line
    }
}
