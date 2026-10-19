// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Martin Dolez

//! When to release and restore the desktop. No I/O: the controller observes the system, hands
//! the result to [`Machine::step`] and carries out what it returns.
//!
//! ```text
//!   active ──(release)──> releasing ──(ok)──> released ──(restore)──> restoring ──(ok)──> active
//!     ^                       │                                            │
//!     └────(failed, retry_s)──┘                    released <──(failed, retry_s)
//! ```
//!
//! A release needs a reason that has lasted `release_delay` and nobody at the seat. A restore
//! needs every reason gone for `restore_delay`, or a key press, or a request by hand. Units
//! started or stopped by someone else are adopted once the change has lasted `debounce`.
//!
//! A desktop that was stopped is down while released; one that was moved to another card is
//! still up, so outside changes are only adopted for a stopped desktop. A moved desktop is
//! only moved back automatically when `auto_restore` says so.

use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Active,
    Releasing,
    Released,
    Restoring,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Releasing => "releasing",
            State::Released => "released",
            State::Restoring => "restoring",
        }
    }
}

/// Timing settings, from `[desktop]`.
#[derive(Clone, Debug)]
pub struct Knobs {
    pub release_delay: Duration,
    pub restore_delay: Duration,
    pub hold: Duration,
    pub retry: Duration,
    /// How long an outside change must last before it is adopted.
    pub debounce: Duration,
    pub input_idle: Duration,
}

/// The units as observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Units {
    /// At least one unit is active and none is changing.
    Up,
    /// Every unit is inactive or failed.
    Down,
    /// Something is starting or stopping.
    Changing,
}

/// One observation.
#[derive(Clone, Debug)]
pub struct Seen {
    pub units: Units,
    /// Reasons to release, e.g. `model qwen loaded`.
    pub wants: Vec<String>,
    /// Reasons not to restore yet besides `wants`, e.g. a lease on the card.
    pub keeps: Vec<String>,
    /// Users with a session on the seat (greeters do not count).
    pub seat_users: Vec<String>,
    /// logind could not be asked; treated as someone at the seat.
    pub seat_error: Option<String>,
    /// Last key press seen, if input is watched.
    pub last_input: Option<Instant>,
}

/// What the controller should do now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Wait,
    Release(String),
    Restore(String),
}

pub struct Machine {
    k: Knobs,
    state: State,
    since: Instant,
    /// The desktop was stopped by gpuq, so gpuq may start it again on its own.
    by_gpuq: bool,
    want_since: Option<Instant>,
    clear_since: Option<Instant>,
    /// Since when the units disagree with the state.
    flip_since: Option<Instant>,
    hold_until: Option<Instant>,
    retry_at: Option<Instant>,
    /// What the machine is waiting for, or why it is in its state.
    why: String,
    last_error: Option<String>,
    /// How the current release was done.
    how: How,
}

/// How a desktop was released.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum How {
    /// Its units were stopped: down while released.
    Stopped,
    /// It was moved to another card: up while released.
    Moved {
        /// Move it back on its own once nothing needs the VRAM.
        auto_restore: bool,
    },
}

impl Machine {
    /// Start in `Active` when the units are up (or changing), `Released` when they are down.
    /// `by_gpuq`: a down desktop was stopped by gpuq before a restart.
    /// `moved`: gpuq had moved an up desktop to another card before a restart.
    pub fn new(k: Knobs, now: Instant, units: Units, by_gpuq: bool, moved: Option<How>) -> Machine {
        let (state, by_gpuq, why, how) = match (units, moved) {
            (Units::Down, _) if by_gpuq => (
                State::Released,
                true,
                "stopped by gpuq before a restart",
                How::Stopped,
            ),
            (Units::Down, _) => (
                State::Released,
                false,
                "found stopped at start",
                How::Stopped,
            ),
            (_, Some(how @ How::Moved { .. })) => (
                State::Released,
                true,
                "moved to another card by gpuq before a restart",
                how,
            ),
            _ => (State::Active, false, "running", How::Stopped),
        };
        Machine {
            k,
            state,
            since: now,
            by_gpuq,
            want_since: None,
            clear_since: None,
            flip_since: None,
            hold_until: None,
            retry_at: None,
            why: why.into(),
            last_error: None,
            how,
        }
    }

    pub fn how(&self) -> How {
        self.how
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn since(&self) -> Instant {
        self.since
    }

    pub fn why(&self) -> &str {
        &self.why
    }

    pub fn by_gpuq(&self) -> bool {
        self.by_gpuq
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Time left on the hold after a restore by hand or by input.
    pub fn hold_left(&self, now: Instant) -> Option<Duration> {
        left(self.hold_until, now)
    }

    pub fn retry_left(&self, now: Instant) -> Option<Duration> {
        left(self.retry_at, now)
    }

    fn enter(&mut self, state: State, now: Instant, why: impl Into<String>) {
        self.state = state;
        self.since = now;
        self.why = why.into();
        self.want_since = None;
        self.clear_since = None;
        self.flip_since = None;
    }

    /// Why a release cannot happen now, ignoring delays. `manual` skips the hold and retry
    /// timers and the input check: someone asked for it.
    fn blocked(&self, now: Instant, seen: &Seen, manual: bool) -> Option<String> {
        if let Some(e) = &seen.seat_error {
            return Some(format!("cannot tell who is at the seat: {e}"));
        }
        if !seen.seat_users.is_empty() {
            return Some(format!(
                "{} logged in at the seat",
                seen.seat_users.join(", ")
            ));
        }
        if manual {
            return None;
        }
        if let Some(t) = seen.last_input {
            let ago = now.saturating_duration_since(t);
            if ago < self.k.input_idle {
                return Some(format!("input at the seat {}s ago", ago.as_secs()));
            }
        }
        if let Some(l) = self.hold_left(now) {
            return Some(format!("desktop asked for, held {}s more", l.as_secs()));
        }
        if let Some(l) = self.retry_left(now) {
            return Some(format!("last attempt failed, retry in {}s", l.as_secs()));
        }
        None
    }

    /// Adopt a change made outside gpuq once it has lasted `debounce`. True when adopted.
    fn adopt(&mut self, now: Instant, seen: &Seen) -> bool {
        let stopped = self.how == How::Stopped;
        let flipped = match (self.state, seen.units) {
            (State::Active, Units::Down) => Some((State::Released, "stopped outside gpuq")),
            (State::Released, Units::Up) if stopped => {
                Some((State::Active, "started outside gpuq"))
            }
            _ => None,
        };
        let Some((to, why)) = flipped else {
            self.flip_since = None;
            return false;
        };
        let since = *self.flip_since.get_or_insert(now);
        if now.saturating_duration_since(since) < self.k.debounce {
            return false;
        }
        self.enter(to, now, why);
        self.by_gpuq = false;
        self.retry_at = None;
        self.how = How::Stopped;
        true
    }

    /// Decide on one observation. Only acts in `Active` and `Released`.
    pub fn step(&mut self, now: Instant, seen: &Seen) -> Step {
        if self.adopt(now, seen) {
            return Step::Wait;
        }
        match self.state {
            State::Active => self.step_active(now, seen),
            State::Released => self.step_released(now, seen),
            State::Releasing | State::Restoring => Step::Wait,
        }
    }

    fn step_active(&mut self, now: Instant, seen: &Seen) -> Step {
        if seen.wants.is_empty() {
            self.want_since = None;
            self.why = "running, nothing needs its VRAM".into();
            return Step::Wait;
        }
        let since = *self.want_since.get_or_insert(now);
        let reasons = seen.wants.join(", ");
        if seen.units != Units::Up {
            self.why = format!("{reasons}; units are changing");
            return Step::Wait;
        }
        if let Some(b) = self.blocked(now, seen, false) {
            self.why = format!("{reasons}; not released: {b}");
            return Step::Wait;
        }
        let waited = now.saturating_duration_since(since);
        if waited < self.k.release_delay {
            let left = self.k.release_delay - waited;
            self.why = format!("{reasons}; release in {}s", left.as_secs());
            return Step::Wait;
        }
        Step::Release(reasons)
    }

    fn step_released(&mut self, now: Instant, seen: &Seen) -> Step {
        if let Some(e) = &seen.seat_error {
            self.why = format!("cannot tell who is at the seat: {e}");
            self.clear_since = None;
            return Step::Wait;
        }
        if !seen.seat_users.is_empty() {
            // A console login: starting the display manager could take its terminal.
            self.why = format!(
                "{} logged in at the seat; display manager left stopped",
                seen.seat_users.join(", ")
            );
            self.clear_since = None;
            return Step::Wait;
        }
        // A moved desktop is still on screen; a key press needs nothing from gpuq.
        if self.how == How::Stopped && seen.last_input.is_some_and(|t| t > self.since) {
            return Step::Restore("key pressed at the seat".into());
        }
        let mut reasons = seen.wants.clone();
        reasons.extend(seen.keeps.iter().cloned());
        if !reasons.is_empty() {
            self.clear_since = None;
            self.why = format!("kept released: {}", reasons.join(", "));
            return Step::Wait;
        }
        if !self.by_gpuq {
            self.why = "stopped outside gpuq; not started automatically".into();
            return Step::Wait;
        }
        if self.how
            == (How::Moved {
                auto_restore: false,
            })
        {
            self.why = "moved to another card; `gpuq desktop restore` moves it back".into();
            return Step::Wait;
        }
        if self.how == How::Stopped && seen.units != Units::Down {
            self.why = "units are changing".into();
            return Step::Wait;
        }
        if let Some(l) = self.retry_left(now) {
            self.why = format!("last restore failed, retry in {}s", l.as_secs());
            return Step::Wait;
        }
        let since = *self.clear_since.get_or_insert(now);
        let waited = now.saturating_duration_since(since);
        if waited < self.k.restore_delay {
            let left = self.k.restore_delay - waited;
            self.why = format!("nothing needs the VRAM; restore in {}s", left.as_secs());
            return Step::Wait;
        }
        Step::Restore(format!(
            "nothing needed the VRAM for {}s",
            self.k.restore_delay.as_secs()
        ))
    }

    /// A job does not fit: release now if allowed, without `release_delay`.
    pub fn offload(&mut self, now: Instant, seen: &Seen) -> Result<Step, String> {
        match self.state {
            State::Released | State::Releasing => return Ok(Step::Wait),
            State::Restoring => return Err("the desktop is being restored".into()),
            State::Active => {}
        }
        if seen.units != Units::Up {
            return Err("the desktop units are changing".into());
        }
        match self.blocked(now, seen, false) {
            Some(b) => Err(b),
            None => Ok(Step::Release("a job does not fit".into())),
        }
    }

    /// `gpuq desktop release`. `force` also ignores sessions at the seat.
    pub fn ask_release(&mut self, now: Instant, seen: &Seen, force: bool) -> Result<Step, String> {
        match self.state {
            State::Released => return Err("already released".into()),
            State::Releasing | State::Restoring => {
                return Err(format!("busy: {}", self.state.name()))
            }
            State::Active => {}
        }
        if !force {
            if let Some(b) = self.blocked(now, seen, true) {
                return Err(format!("{b} (--force releases anyway)"));
            }
        }
        self.hold_until = None;
        Ok(Step::Release(
            if force {
                "forced by hand"
            } else {
                "asked by hand"
            }
            .into(),
        ))
    }

    /// `gpuq desktop restore`: start it and hold off releases for `hold` (or the default).
    pub fn ask_restore(&mut self, now: Instant, hold: Option<Duration>) -> Result<Step, String> {
        let hold = hold.unwrap_or(self.k.hold);
        match self.state {
            State::Active => {
                self.hold_until = Some(now + hold);
                Err("already running; releases held off".into())
            }
            State::Releasing | State::Restoring => Err(format!("busy: {}", self.state.name())),
            State::Released => {
                self.hold_until = Some(now + hold);
                Ok(Step::Restore("asked by hand".into()))
            }
        }
    }

    pub fn begin(&mut self, now: Instant, step: &Step) {
        match step {
            Step::Release(why) => self.enter(State::Releasing, now, why.clone()),
            Step::Restore(why) => {
                if why.starts_with("key pressed") {
                    self.hold_until = Some(now + self.k.hold);
                }
                self.enter(State::Restoring, now, why.clone())
            }
            Step::Wait => {}
        }
    }

    pub fn released(&mut self, now: Instant, result: Result<(String, How), String>) {
        match result {
            Ok((note, how)) => {
                self.enter(State::Released, now, note);
                self.how = how;
                self.by_gpuq = true;
                self.retry_at = None;
                self.last_error = None;
            }
            Err(e) => {
                self.enter(State::Active, now, format!("release failed: {e}"));
                self.retry_at = Some(now + self.k.retry);
                self.last_error = Some(e);
            }
        }
    }

    pub fn restored(&mut self, now: Instant, result: Result<String, String>) {
        match result {
            Ok(note) => {
                self.enter(State::Active, now, note);
                self.how = How::Stopped;
                self.by_gpuq = false;
                self.retry_at = None;
                self.last_error = None;
            }
            Err(e) => {
                self.enter(State::Released, now, format!("restore failed: {e}"));
                self.retry_at = Some(now + self.k.retry);
                self.last_error = Some(e);
            }
        }
    }
}

fn left(t: Option<Instant>, now: Instant) -> Option<Duration> {
    t.and_then(|t| t.checked_duration_since(now))
        .filter(|d| !d.is_zero())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knobs() -> Knobs {
        Knobs {
            release_delay: Duration::from_secs(30),
            restore_delay: Duration::from_secs(600),
            hold: Duration::from_secs(1800),
            retry: Duration::from_secs(900),
            debounce: Duration::from_secs(10),
            input_idle: Duration::from_secs(300),
        }
    }

    fn seen(units: Units) -> Seen {
        Seen {
            units,
            wants: Vec::new(),
            keeps: Vec::new(),
            seat_users: Vec::new(),
            seat_error: None,
            last_input: None,
        }
    }

    fn stopped() -> Result<(String, How), String> {
        Ok(("released".into(), How::Stopped))
    }

    fn wants(units: Units, why: &str) -> Seen {
        Seen {
            wants: vec![why.into()],
            ..seen(units)
        }
    }

    fn secs(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    #[test]
    fn starts_from_what_the_units_say() {
        let t0 = Instant::now();
        let m = Machine::new(knobs(), t0, Units::Up, false, None);
        assert_eq!(m.state(), State::Active);
        let m = Machine::new(knobs(), t0, Units::Changing, false, None);
        assert_eq!(m.state(), State::Active);
        let m = Machine::new(knobs(), t0, Units::Down, false, None);
        assert_eq!(m.state(), State::Released);
        assert!(!m.by_gpuq());
        let m = Machine::new(knobs(), t0, Units::Down, true, None);
        assert!(m.by_gpuq());
    }

    #[test]
    fn releases_once_the_reason_has_lasted() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        let s = wants(Units::Up, "model big loaded");
        assert_eq!(m.step(t0, &s), Step::Wait);
        assert!(m.why().contains("release in 30s"), "{}", m.why());
        assert_eq!(m.step(secs(t0, 29), &s), Step::Wait);
        assert_eq!(
            m.step(secs(t0, 30), &s),
            Step::Release("model big loaded".into())
        );
    }

    #[test]
    fn a_reason_that_goes_away_restarts_the_delay() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        let s = wants(Units::Up, "model big loaded");
        m.step(t0, &s);
        m.step(secs(t0, 20), &seen(Units::Up));
        assert_eq!(m.step(secs(t0, 40), &s), Step::Wait);
        assert_eq!(
            m.step(secs(t0, 70), &s),
            Step::Release("model big loaded".into())
        );
    }

    #[test]
    fn never_releases_with_a_user_at_the_seat() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        let s = Seen {
            seat_users: vec!["alice".into()],
            ..wants(Units::Up, "model big loaded")
        };
        for t in [0, 100, 10_000] {
            assert_eq!(m.step(secs(t0, t), &s), Step::Wait);
        }
        assert!(m.why().contains("alice logged in"), "{}", m.why());
        assert!(m.ask_release(secs(t0, 10_001), &s, false).is_err());
        assert!(m.ask_release(secs(t0, 10_001), &s, true).is_ok());
        assert!(m.offload(secs(t0, 10_001), &s).is_err());
    }

    #[test]
    fn recent_input_holds_off_a_release() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        let s = Seen {
            last_input: Some(secs(t0, 100)),
            ..wants(Units::Up, "model big loaded")
        };
        assert_eq!(m.step(secs(t0, 120), &s), Step::Wait);
        assert!(m.why().contains("input at the seat 20s ago"), "{}", m.why());
        assert_eq!(m.step(secs(t0, 399), &s), Step::Wait);
        assert!(matches!(m.step(secs(t0, 400), &s), Step::Release(_)));
    }

    #[test]
    fn does_not_act_while_units_change() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        let s = wants(Units::Changing, "model big loaded");
        assert_eq!(m.step(t0, &s), Step::Wait);
        assert_eq!(m.step(secs(t0, 1000), &s), Step::Wait);
        assert_eq!(m.state(), State::Active);
    }

    #[test]
    fn released_stays_released_while_anything_needs_it() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        m.begin(t0, &Step::Release("x".into()));
        assert_eq!(m.state(), State::Releasing);
        m.released(t0, stopped());
        assert_eq!(m.state(), State::Released);
        let busy = Seen {
            keeps: vec!["lease batch/train".into()],
            ..seen(Units::Down)
        };
        for t in [0, 1000, 100_000] {
            assert_eq!(m.step(secs(t0, t), &busy), Step::Wait);
        }
        assert!(m.why().contains("lease batch/train"), "{}", m.why());
    }

    #[test]
    fn restores_after_the_delay_without_reasons() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        m.begin(t0, &Step::Release("x".into()));
        m.released(t0, stopped());
        let idle = seen(Units::Down);
        assert_eq!(m.step(secs(t0, 10), &idle), Step::Wait);
        // A reason in between restarts the delay: no flapping.
        m.step(secs(t0, 300), &wants(Units::Down, "model big loaded"));
        assert_eq!(m.step(secs(t0, 610), &idle), Step::Wait);
        assert_eq!(m.step(secs(t0, 1209), &idle), Step::Wait);
        assert!(matches!(m.step(secs(t0, 1210), &idle), Step::Restore(_)));
    }

    #[test]
    fn does_not_restore_a_desktop_stopped_by_someone_else() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Down, false, None);
        assert_eq!(m.step(secs(t0, 100_000), &seen(Units::Down)), Step::Wait);
        assert!(m.why().contains("outside gpuq"), "{}", m.why());
        assert!(matches!(
            m.ask_restore(secs(t0, 100_001), None),
            Ok(Step::Restore(_))
        ));
    }

    #[test]
    fn a_key_press_restores_and_holds() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        m.begin(t0, &Step::Release("x".into()));
        m.released(secs(t0, 5), stopped());
        let old_key = Seen {
            last_input: Some(secs(t0, 1)),
            ..wants(Units::Down, "model big loaded")
        };
        assert_eq!(m.step(secs(t0, 10), &old_key), Step::Wait);
        let key = Seen {
            last_input: Some(secs(t0, 20)),
            ..wants(Units::Down, "model big loaded")
        };
        let step = m.step(secs(t0, 21), &key);
        assert!(matches!(step, Step::Restore(_)), "{step:?}");
        m.begin(secs(t0, 21), &step);
        m.restored(secs(t0, 25), Ok("running".into()));
        // The model is still loaded, but the person gets the hold.
        let later = Seen {
            last_input: Some(secs(t0, 20)),
            ..wants(Units::Up, "model big loaded")
        };
        assert_eq!(m.step(secs(t0, 400), &later), Step::Wait);
        assert!(m.why().contains("held"), "{}", m.why());
        assert!(matches!(m.step(secs(t0, 1821), &later), Step::Release(_)));
    }

    #[test]
    fn a_console_login_keeps_the_display_manager_stopped() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Down, true, None);
        let s = Seen {
            seat_users: vec!["root".into()],
            last_input: Some(secs(t0, 50)),
            ..seen(Units::Down)
        };
        assert_eq!(m.step(secs(t0, 10_000), &s), Step::Wait);
        assert!(m.why().contains("left stopped"), "{}", m.why());
    }

    #[test]
    fn a_failed_release_waits_before_trying_again() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        let s = wants(Units::Up, "model big loaded");
        m.step(t0, &s);
        let step = m.step(secs(t0, 30), &s);
        m.begin(secs(t0, 30), &step);
        m.released(secs(t0, 40), Err("sddm still running".into()));
        assert_eq!(m.state(), State::Active);
        assert_eq!(m.last_error(), Some("sddm still running"));
        assert_eq!(m.step(secs(t0, 100), &s), Step::Wait);
        assert!(m.why().contains("retry in"), "{}", m.why());
        assert!(m.offload(secs(t0, 100), &s).is_err());
        assert_eq!(m.step(secs(t0, 939), &s), Step::Wait);
        assert!(matches!(m.step(secs(t0, 940), &s), Step::Release(_)));
    }

    #[test]
    fn a_failed_restore_waits_before_trying_again() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Down, true, None);
        let idle = seen(Units::Down);
        m.step(t0, &idle);
        let step = m.step(secs(t0, 600), &idle);
        assert!(matches!(step, Step::Restore(_)));
        m.begin(secs(t0, 600), &step);
        m.restored(secs(t0, 610), Err("start failed".into()));
        assert_eq!(m.state(), State::Released);
        assert_eq!(m.step(secs(t0, 1000), &idle), Step::Wait);
        assert!(m.why().contains("retry in"), "{}", m.why());
        m.step(secs(t0, 1510), &idle);
        assert!(matches!(m.step(secs(t0, 2110), &idle), Step::Restore(_)));
    }

    #[test]
    fn outside_changes_are_adopted_after_the_debounce() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        // One poll with the units down (a restart) is not enough.
        m.step(t0, &seen(Units::Down));
        m.step(secs(t0, 5), &seen(Units::Up));
        m.step(secs(t0, 20), &seen(Units::Down));
        assert_eq!(m.state(), State::Active);
        m.step(secs(t0, 30), &seen(Units::Down));
        assert_eq!(m.state(), State::Released);
        assert!(!m.by_gpuq());
        m.step(secs(t0, 40), &seen(Units::Up));
        m.step(secs(t0, 50), &seen(Units::Up));
        assert_eq!(m.state(), State::Active);
    }

    #[test]
    fn the_released_state_is_idempotent() {
        // The old script stopped the desktop again on every poll. Released never acts on
        // the reasons that got it there.
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        m.begin(t0, &Step::Release("x".into()));
        m.released(t0, stopped());
        let s = wants(Units::Down, "model big loaded");
        for t in (0..3600).step_by(5) {
            assert_eq!(m.step(secs(t0, t), &s), Step::Wait);
        }
        assert_eq!(m.offload(secs(t0, 3600), &s), Ok(Step::Wait));
        assert!(m.ask_release(secs(t0, 3600), &s, true).is_err());
    }

    #[test]
    fn offload_skips_the_release_delay_but_not_the_checks() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        assert_eq!(
            m.offload(t0, &seen(Units::Up)),
            Ok(Step::Release("a job does not fit".into()))
        );
        m.ask_restore(t0, None).unwrap_err();
        assert!(m.offload(secs(t0, 1), &seen(Units::Up)).is_err());
    }

    #[test]
    fn a_moved_desktop_is_up_while_released() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        m.begin(t0, &Step::Release("x".into()));
        let moved = How::Moved {
            auto_restore: false,
        };
        m.released(t0, Ok(("moved".into(), moved)));
        // Units up is what a moved desktop looks like: not adopted as started outside.
        for t in [0, 30, 100_000] {
            assert_eq!(m.step(secs(t0, t), &seen(Units::Up)), Step::Wait);
        }
        assert_eq!(m.state(), State::Released);
        assert!(m.why().contains("moves it back"), "{}", m.why());
        // A key press needs nothing: it is on screen.
        let key = Seen {
            last_input: Some(secs(t0, 100_001)),
            ..seen(Units::Up)
        };
        assert_eq!(m.step(secs(t0, 100_002), &key), Step::Wait);
        assert!(matches!(
            m.ask_restore(secs(t0, 100_003), None),
            Ok(Step::Restore(_))
        ));
    }

    #[test]
    fn a_moved_desktop_can_move_back_on_its_own() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        m.begin(t0, &Step::Release("x".into()));
        let moved = How::Moved { auto_restore: true };
        m.released(t0, Ok(("moved".into(), moved)));
        m.step(secs(t0, 1), &seen(Units::Up));
        assert!(matches!(
            m.step(secs(t0, 601), &seen(Units::Up)),
            Step::Restore(_)
        ));
        // After a restart gpuq still knows it moved it.
        let m = Machine::new(knobs(), t0, Units::Up, true, Some(moved));
        assert_eq!(m.state(), State::Released);
        assert_eq!(m.how(), moved);
    }

    #[test]
    fn an_unreadable_seat_counts_as_occupied() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Up, false, None);
        let s = Seen {
            seat_error: Some("loginctl timed out".into()),
            ..wants(Units::Up, "model big loaded")
        };
        assert_eq!(m.step(secs(t0, 1000), &s), Step::Wait);
        assert!(m.why().contains("cannot tell"), "{}", m.why());
        assert!(m.offload(secs(t0, 1000), &s).is_err());
    }

    #[test]
    fn a_restore_by_hand_holds_off_releases() {
        let t0 = Instant::now();
        let mut m = Machine::new(knobs(), t0, Units::Down, true, None);
        let step = m.ask_restore(t0, Some(Duration::from_secs(60))).unwrap();
        m.begin(t0, &step);
        m.restored(secs(t0, 5), Ok("running".into()));
        let s = wants(Units::Up, "model big loaded");
        assert_eq!(m.step(secs(t0, 10), &s), Step::Wait);
        assert!(m.why().contains("held"), "{}", m.why());
        // The hold ran out at 60 s; the reason has lasted since 10 s.
        assert!(matches!(m.step(secs(t0, 61), &s), Step::Release(_)));
    }
}
