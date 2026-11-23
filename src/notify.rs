//! What dictation is doing, as one desktop notification it keeps updating.
//!
//! The daemon posts over its own session-bus connection: notification
//! servers only let the sender that posted a notification replace it, and a
//! helper process per update would be a new sender each time.

use futures_lite::{StreamExt, future::race};
use std::{collections::HashMap, time::Duration};
use zbus::zvariant::Value;

#[derive(Default)]
pub(crate) struct Notifier {
    connection: Option<zbus::blocking::Connection>,
    id: u32,
    /// The level the notification last showed, while it shows one.
    level: Option<u8>,
}

impl Notifier {
    /// Shows `summary` (and `body`), replacing the last notification; it
    /// closes itself after `seconds`, or stays while `seconds` is zero.
    pub(crate) fn show(&mut self, summary: &str, body: &str, seconds: u32) {
        self.level = None;
        self.post(summary, body, seconds, None);
    }

    /// Shows `summary` and `body` with a level bar (0 to 100), staying up;
    /// a level within a few points of the shown one is not sent.
    pub(crate) fn level(&mut self, summary: &str, body: &str, level: u8) {
        if self.level.is_some_and(|shown| shown.abs_diff(level) < 6) {
            return;
        }
        self.level = Some(level);
        self.post(summary, body, 0, Some(level));
    }

    /// Subscribes before posting, on the connection that owns the prompt.
    pub(crate) fn ask(&mut self, summary: &str, body: &str, actions: &[&str]) -> Option<Prompt> {
        self.level = None;
        if self.connection.is_none() {
            self.connection = zbus::blocking::Connection::session().ok();
            self.id = 0;
        }
        let connection = self.connection.as_ref()?;
        let signals = async_io::block_on(async {
            let proxy = zbus::Proxy::new(
                connection.inner(),
                "org.freedesktop.Notifications",
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
            )
            .await?;
            proxy.receive_all_signals().await
        })
        .ok()?;
        let id = notify_with(connection, self.id, summary, body, 120, None, actions).ok()?;
        self.id = id;
        Some(Prompt { id, signals })
    }

    pub(crate) fn close(&mut self) {
        if let Some(connection) = &self.connection {
            let id = std::mem::take(&mut self.id);
            let _ = connection.call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "CloseNotification",
                &(id,),
            );
        }
    }

    fn post(&mut self, summary: &str, body: &str, seconds: u32, level: Option<u8>) {
        // A dropped connection (the shell restarted) is opened again once.
        for _ in 0..2 {
            if self.connection.is_none() {
                self.connection = zbus::blocking::Connection::session().ok();
                self.id = 0;
            }
            let Some(connection) = &self.connection else {
                return;
            };
            match notify(connection, self.id, summary, body, seconds, level) {
                Ok(id) => {
                    self.id = id;
                    return;
                }
                Err(_) => self.connection = None,
            }
        }
    }
}

fn notify(
    connection: &zbus::blocking::Connection,
    replaces: u32,
    summary: &str,
    body: &str,
    seconds: u32,
    level: Option<u8>,
) -> zbus::Result<u32> {
    notify_with(connection, replaces, summary, body, seconds, level, &[])
}

fn notify_with(
    connection: &zbus::blocking::Connection,
    replaces: u32,
    summary: &str,
    body: &str,
    seconds: u32,
    level: Option<u8>,
    actions: &[&str],
) -> zbus::Result<u32> {
    let mut hints = HashMap::from([
        ("category", Value::from("device")),
        ("transient", Value::from(true)),
        ("desktop-entry", Value::from("enzimos-dictation")),
    ]);
    if let Some(level) = level {
        hints.insert("value", Value::from(i32::from(level)));
    }
    let timeout = i32::try_from(seconds.saturating_mul(1000)).unwrap_or(i32::MAX);
    connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "Dictation",
                replaces,
                "enzimos-dictation",
                summary,
                body,
                actions.to_vec(),
                hints,
                timeout,
            ),
        )?
        .body()
        .deserialize()
}

pub(crate) struct Prompt {
    id: u32,
    signals: zbus::proxy::SignalStream<'static>,
}

impl Prompt {
    pub(crate) fn allowed(mut self, cancelled: async_channel::Receiver<()>) -> bool {
        async_io::block_on(race(
            async {
                let cancelled = async {
                    let _ = cancelled.recv().await;
                };
                let expired = async {
                    async_io::Timer::after(Duration::from_secs(120)).await;
                };
                race(cancelled, expired).await;
                false
            },
            async {
                while let Some(message) = self.signals.next().await {
                    match message.header().member().map(|member| member.as_str()) {
                        Some("ActionInvoked") => {
                            if let Ok((id, key)) = message.body().deserialize::<(u32, String)>()
                                && id == self.id
                            {
                                return key == "allow";
                            }
                        }
                        Some("NotificationClosed") => {
                            if let Ok((id, _)) = message.body().deserialize::<(u32, u32)>()
                                && id == self.id
                            {
                                return false;
                            }
                        }
                        _ => {}
                    }
                }
                false
            },
        ))
    }
}
