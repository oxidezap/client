//! StatusNotifierItem tray, via ksni.

use std::sync::Arc;

use anyhow::{Context, Result};
use ksni::{Handle, Icon, MenuItem, ToolTip, Tray as KsniTray, TrayMethods, menu::StandardItem};

use crate::state::{StateHub, TrayState};

/// The icon's model. ksni renders from this; the daemon updates it.
struct Item {
    state: TrayState,
    /// Held so the menu and the icon can publish. The tray is otherwise a
    /// pure observer; its menu items and a click on it are the one place it
    /// speaks back — and the one thing it asks, whether a window is
    /// attached, is what the first item is named from.
    hub: Arc<StateHub>,
    /// What a click on the icon does, and when the last one was: a double
    /// click arrives as two, and the second must not undo the first.
    click: crate::window::Toggle,
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

    /// A click on the icon. The host sends this for a left click, and what
    /// it means depends on what is up: away if there is a window, up if
    /// there is not. The daemon has no window, so both are requests — see
    /// `crate::window::Toggle`, which is also what keeps a double click,
    /// which arrives as two of these, from hiding and then reopening.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.click.click(&self.hub);
    }

    /// The host is about to open the menu.
    ///
    /// Nothing to do, and the method exists to say so: ksni re-reads
    /// [`Self::menu`] before showing it only when this is implemented, and
    /// the first item below is named from whether a window is attached — a
    /// fact the tray is otherwise told nothing about, since it follows the
    /// hub's `TrayState` and a window attaching moves none of it. Without
    /// this the label would be whatever was true at the last unread count.
    fn menu_about_to_show(&mut self) {}

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
        // One item, named for what it would do rather than a fixed pair: the
        // menu is read when the host opens it (`menu_about_to_show`), so the
        // label follows the window. Each label does only what it says — Open
        // raises, Hide asks to go — rather than both toggling, so a label a
        // host showed stale can at worst do nothing, never the opposite.
        let window = if self.hub.windows_attached() {
            StandardItem {
                label: "Hide".into(),
                // A request, like Open: the daemon owns no window, and the
                // front end decides what going away means for it. See
                // `crate::window::hide`.
                activate: Box::new(|item: &mut Self| {
                    crate::window::hide(&item.hub);
                }),
                ..Default::default()
            }
        } else {
            StandardItem {
                label: "Open".into(),
                // The daemon has no window, so this is a request passed
                // through to whoever has one — and a front end started for it
                // when nobody is attached, which is the state the tray is
                // most often clicked in. See `crate::window::show`.
                activate: Box::new(|item: &mut Self| {
                    crate::window::show(&item.hub);
                }),
                ..Default::default()
            }
        };
        vec![
            window.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                // The daemon owns shutdown, so the menu asks rather than
                // exits: tearing the process down from a D-Bus callback would
                // skip the session teardown.
                activate: Box::new(|_: &mut Self| {
                    // Asks; `main` is what acts. Stopping the process from a
                    // D-Bus callback would skip the session teardown.
                    crate::shutdown::request("tray menu");
                }),
                ..Default::default()
            }
            .into(),
        ]
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

pub async fn start(hub: Arc<StateHub>) -> Result<Box<dyn super::Tray>> {
    let item = Item {
        state: TrayState {
            connected: false,
            unread: 0,
        },
        hub,
        click: crate::window::Toggle::default(),
    };
    let handle = item
        .spawn()
        .await
        .context("registering a StatusNotifierItem (is a tray host running?)")?;
    Ok(Box::new(LinuxTray { handle }))
}
