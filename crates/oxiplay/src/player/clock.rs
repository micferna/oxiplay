//! Horloge maîtresse de lecture.
//!
//! L'horloge avance en temps réel multiplié par la vitesse de lecture, et
//! peut être (ré)ancrée à tout moment : le callback audio la resynchronise
//! en continu sur le PTS réellement joué (l'audio est l'horloge maîtresse) ;
//! sans piste audio, elle tourne librement sur l'horloge murale.

use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug)]
struct ClockInner {
    /// Position média (µs) au moment de l'ancrage.
    anchor_media_us: i64,
    /// Instant mural de l'ancrage.
    anchor_at: Instant,
    /// Vitesse de lecture (0.25 à 4.0).
    speed: f64,
    paused: bool,
    /// Devient vrai dès qu'un premier échantillon audio ou une première
    /// image a fixé l'origine réelle du flux.
    started: bool,
}

/// Horloge de lecture partagée entre les threads (interne à un `Mutex`,
/// toutes les opérations sont O(1) et très courtes).
#[derive(Debug)]
pub struct PlaybackClock {
    inner: Mutex<ClockInner>,
}

impl Default for PlaybackClock {
    fn default() -> Self {
        Self {
            inner: Mutex::new(ClockInner {
                anchor_media_us: 0,
                anchor_at: Instant::now(),
                speed: 1.0,
                paused: true,
                started: false,
            }),
        }
    }
}

impl PlaybackClock {
    /// Position média courante en microsecondes.
    pub fn now_us(&self) -> i64 {
        let inner = self.inner.lock().unwrap();
        if inner.paused {
            inner.anchor_media_us
        } else {
            inner.anchor_media_us
                + (inner.anchor_at.elapsed().as_micros() as f64 * inner.speed) as i64
        }
    }

    pub fn is_paused(&self) -> bool {
        self.inner.lock().unwrap().paused
    }

    /// Vitesse courante (utilisée par les tests ; le pipeline lit la
    /// vitesse via `SharedState::speed`).
    #[allow(dead_code)]
    pub fn speed(&self) -> f64 {
        self.inner.lock().unwrap().speed
    }

    pub fn set_paused(&self, paused: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.paused == paused {
            return;
        }
        // Ré-ancre pour figer/repartir exactement de la position courante.
        let now = if inner.paused {
            inner.anchor_media_us
        } else {
            inner.anchor_media_us
                + (inner.anchor_at.elapsed().as_micros() as f64 * inner.speed) as i64
        };
        inner.anchor_media_us = now;
        inner.anchor_at = Instant::now();
        inner.paused = paused;
    }

    /// Change la vitesse en conservant la position courante.
    pub fn set_speed(&self, speed: f64) {
        let mut inner = self.inner.lock().unwrap();
        let now = if inner.paused {
            inner.anchor_media_us
        } else {
            inner.anchor_media_us
                + (inner.anchor_at.elapsed().as_micros() as f64 * inner.speed) as i64
        };
        inner.anchor_media_us = now;
        inner.anchor_at = Instant::now();
        inner.speed = speed.clamp(0.25, 4.0);
    }

    /// Force la position (seek, ou premier PTS du flux).
    pub fn set_position(&self, media_us: i64) {
        let mut inner = self.inner.lock().unwrap();
        inner.anchor_media_us = media_us;
        inner.anchor_at = Instant::now();
        inner.started = true;
    }

    /// Resynchronisation douce par l'audio : ne ré-ancre que si la dérive
    /// dépasse un seuil, pour éviter de faire trembler la vidéo.
    pub fn sync_to(&self, media_us: i64) {
        const DRIFT_TOLERANCE_US: i64 = 30_000;
        let mut inner = self.inner.lock().unwrap();
        let now = if inner.paused {
            inner.anchor_media_us
        } else {
            inner.anchor_media_us
                + (inner.anchor_at.elapsed().as_micros() as f64 * inner.speed) as i64
        };
        if (now - media_us).abs() > DRIFT_TOLERANCE_US || !inner.started {
            inner.anchor_media_us = media_us;
            inner.anchor_at = Instant::now();
            inner.started = true;
        }
    }

    /// Vrai si l'origine du flux a déjà été fixée.
    pub fn started(&self) -> bool {
        self.inner.lock().unwrap().started
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn paused_clock_is_frozen() {
        let clock = PlaybackClock::default();
        clock.set_position(1_000_000);
        let a = clock.now_us();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(a, clock.now_us());
    }

    #[test]
    fn running_clock_advances_with_speed() {
        let clock = PlaybackClock::default();
        clock.set_position(0);
        clock.set_speed(2.0);
        clock.set_paused(false);
        std::thread::sleep(Duration::from_millis(50));
        let elapsed = clock.now_us();
        // ~100 ms de temps média pour 50 ms réels à 2x (large tolérance CI).
        assert!(elapsed > 60_000 && elapsed < 400_000, "elapsed = {elapsed}");
    }

    #[test]
    fn sync_ignores_small_drift() {
        let clock = PlaybackClock::default();
        clock.set_position(1_000_000);
        clock.sync_to(1_010_000); // 10 ms : sous le seuil
        assert_eq!(clock.now_us(), 1_000_000);
        clock.sync_to(2_000_000); // 1 s : ré-ancre
        assert!((clock.now_us() - 2_000_000).abs() < 50_000);
    }

    #[test]
    fn speed_is_clamped() {
        let clock = PlaybackClock::default();
        clock.set_speed(10.0);
        assert_eq!(clock.speed(), 4.0);
        clock.set_speed(0.0);
        assert_eq!(clock.speed(), 0.25);
    }
}
