//! How long since the user last touched the keyboard or mouse, for the
//! automatic "Idle" status.
//!
//! - Linux: GNOME's Mutter IdleMonitor over D-Bus, then the freedesktop
//!   ScreenSaver interface (KDE and others). X11-based queries are wrong on
//!   Wayland, so they aren't used.
//! - macOS/Windows: the `user-idle` crate (IOKit / GetLastInputInfo).

use std::time::Duration;

/// Go Idle after this long without input (Discord uses 10 minutes).
pub const AWAY_AFTER: Duration = Duration::from_secs(10 * 60);

/// Idle time, or `None` if the system won't say.
#[cfg(target_os = "linux")]
pub async fn idle_time() -> Option<Duration> {
    let conn = zbus::Connection::session().await.ok()?;
    let mutter: Result<u64, _> = conn
        .call_method(
            Some("org.gnome.Mutter.IdleMonitor"),
            "/org/gnome/Mutter/IdleMonitor/Core",
            Some("org.gnome.Mutter.IdleMonitor"),
            "GetIdletime",
            &(),
        )
        .await
        .and_then(|reply| reply.body().deserialize::<u64>());
    if let Ok(ms) = mutter {
        return Some(Duration::from_millis(ms));
    }
    // KDE reports milliseconds here.
    let saver: Result<u32, _> = conn
        .call_method(
            Some("org.freedesktop.ScreenSaver"),
            "/org/freedesktop/ScreenSaver",
            Some("org.freedesktop.ScreenSaver"),
            "GetSessionIdleTime",
            &(),
        )
        .await
        .and_then(|reply| reply.body().deserialize::<u32>());
    saver.ok().map(|ms| Duration::from_millis(ms as u64))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub async fn idle_time() -> Option<Duration> {
    user_idle::UserIdle::get_time().ok().map(|t| t.duration())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub async fn idle_time() -> Option<Duration> {
    None
}

/// What the automatic status should do given the idle time.
#[derive(Debug, PartialEq, Eq)]
pub enum AutoAway {
    GoIdle,
    ComeBack,
    Stay,
}

/// `online`: the user's chosen status is Online; `auto`: we set Idle
/// ourselves (so we may undo it). Manual Idle/DND/Invisible are left alone.
pub fn decide(idle: Duration, online: bool, auto: bool) -> AutoAway {
    if online && idle >= AWAY_AFTER {
        AutoAway::GoIdle
    } else if auto && idle < Duration::from_secs(30) {
        AutoAway::ComeBack
    } else {
        AutoAway::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_query_does_not_panic() {
        // On a desktop this returns the real idle time; in CI (no session
        // bus) it returns None.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let idle = rt.block_on(idle_time());
        eprintln!("idle: {idle:?}");
    }

    #[test]
    fn auto_away_rules() {
        let min = |m: u64| Duration::from_secs(m * 60);
        assert_eq!(decide(min(11), true, false), AutoAway::GoIdle);
        assert_eq!(decide(min(5), true, false), AutoAway::Stay);
        assert_eq!(
            decide(Duration::from_secs(2), false, true),
            AutoAway::ComeBack
        );
        // Manual Idle/DND is never touched.
        assert_eq!(decide(Duration::from_secs(2), false, false), AutoAway::Stay);
        assert_eq!(decide(min(30), false, false), AutoAway::Stay);
    }
}
