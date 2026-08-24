//! StatusNotifierItem tray, via ksni.

use anyhow::{Context, Result};
use ksni::{Handle, Icon, MenuItem, ToolTip, Tray as KsniTray, TrayMethods, menu::StandardItem};

use crate::state::TrayState;

/// The icon's model. ksni renders from this; the daemon updates it.
struct Item {
    state: TrayState,
}

impl KsniTray for Item {
    fn id(&self) -> String {
        // Stable across restarts so the host can keep the icon's position.
        "oxidezap".into()
    }

    fn title(&self) -> String {
        "oxidezap".into()
    }

    /// Named from the icon theme rather than shipping pixels: a themed name
    /// follows the user's icon set and dark/light switch for free.
    fn icon_name(&self) -> String {
        if self.state.connected {
            "user-available".into()
        } else {
            "user-offline".into()
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        Vec::new()
    }

    fn tool_tip(&self) -> ToolTip {
        let description = match (self.state.connected, self.state.unread) {
            (false, _) => "Disconnected".to_string(),
            (true, 0) => "Connected".to_string(),
            (true, 1) => "1 unread message".to_string(),
            (true, n) => format!("{n} unread messages"),
        };
        ToolTip {
            title: "oxidezap".into(),
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Quit".into(),
                // The daemon owns shutdown, so the menu asks rather than
                // exits: tearing the process down from a D-Bus callback would
                // skip the session teardown.
                activate: Box::new(|_: &mut Self| {
                    log::info!("tray requested quit");
                    // SIGTERM to ourselves reuses the one shutdown path the
                    // daemon already has, instead of adding a second one.
                    #[cfg(unix)]
                    unsafe {
                        libc_raise_sigterm();
                    }
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[cfg(unix)]
unsafe fn libc_raise_sigterm() {
    unsafe extern "C" {
        fn raise(sig: i32) -> i32;
    }
    const SIGTERM: i32 = 15;
    // SAFETY: raising a signal at our own process is always defined; the
    // daemon installs a SIGTERM handler at startup.
    unsafe {
        raise(SIGTERM);
    }
}

/// A live ksni item, updated through its handle.
struct LinuxTray {
    handle: Handle<Item>,
}

#[async_trait::async_trait]
impl super::Tray for LinuxTray {
    async fn update(&mut self, state: &TrayState) {
        let state = state.clone();
        // Awaited rather than spawned: the watcher calls this in order, and
        // letting each D-Bus update race the next allows an older state to
        // land last and stick.
        self.handle
            .update(move |item: &mut Item| item.state = state)
            .await;
    }
}

pub async fn start() -> Result<Box<dyn super::Tray>> {
    let item = Item {
        state: TrayState {
            connected: false,
            unread: 0,
        },
    };
    let handle = item
        .spawn()
        .await
        .context("registering a StatusNotifierItem (is a tray host running?)")?;
    Ok(Box::new(LinuxTray { handle }))
}
