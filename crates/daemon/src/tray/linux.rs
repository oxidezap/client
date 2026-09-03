//! StatusNotifierItem tray, via ksni.

use std::sync::Arc;

use anyhow::{Context, Result};
use ksni::{
    Category, Handle, Icon, MenuItem, Status, ToolTip, Tray as KsniTray, TrayMethods,
    menu::StandardItem,
};

use crate::state::{StateHub, TrayState};

/// The themed name the icon takes while something is waiting to be read.
///
/// A status name from the icon-naming spec, like the two below it, so it
/// follows the user's theme instead of shipping pixels. There is no count in
/// it — StatusNotifierItem has no badge, and the number is the tooltip's job.
const UNREAD_ICON: &str = "mail-unread";

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

impl Item {
    /// The count the icon and the tooltip both speak from.
    ///
    /// Zero while the connection is down: what we last heard is then a number
    /// nothing is refreshing, and an icon asking to be looked at over a stale
    /// count is worse than one saying the connection is what is wrong. One
    /// method rather than the test written twice, so the icon and the tooltip
    /// cannot disagree about what is unread.
    fn unread(&self) -> u32 {
        if self.state.connected {
            self.state.unread
        } else {
            0
        }
    }
}

impl KsniTray for Item {
    /// What kind of thing the icon is. Hosts group by this and some only
    /// honour an attention state for a communications item, which is exactly
    /// the state below.
    fn category(&self) -> Category {
        Category::Communications
    }

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
        match (self.state.connected, self.unread()) {
            (false, _) => "user-offline".into(),
            (true, 0) => "user-available".into(),
            // Said here as well as in `attention_icon_name` because a host
            // may honour one and not the other: the spec's attention icon is
            // what a host swaps to for the status below, and a host that
            // ignores the status entirely still reads this one. Both name the
            // same icon, so the two answers cannot show different things.
            (true, _) => UNREAD_ICON.into(),
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        Vec::new()
    }

    /// Unread mail is what the status is for: a host is asked to emphasise
    /// the icon, and one that hides passive items keeps this one visible.
    fn status(&self) -> Status {
        if self.unread() > 0 {
            Status::NeedsAttention
        } else {
            Status::Active
        }
    }

    /// The icon a host swaps to under [`Status::NeedsAttention`]. Read only
    /// in that state, so it can be unconditional.
    fn attention_icon_name(&self) -> String {
        UNREAD_ICON.into()
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
        let description = match (self.state.connected, self.unread()) {
            (false, _) => "Disconnected".to_string(),
            (true, 0) => "Connected".to_string(),
            (true, 1) => "1 unread message".to_string(),
            (true, n) => format!("{n} unread messages"),
        };
        // The count rides in the title too: a host that renders only the
        // first line of a tooltip is otherwise told nothing, and the number
        // is the one thing the icon itself cannot say.
        let title = match self.unread() {
            0 => "oxidezap".to_string(),
            n => format!("oxidezap ({n})"),
        };
        ToolTip {
            // Named so a tooltip that draws an icon draws the one the tray
            // is showing rather than a default.
            icon_name: self.icon_name(),
            title,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An icon over a state, with the two things the tray speaks back
    /// through left at their defaults — nothing below asks them anything.
    fn item(connected: bool, unread: u32) -> Item {
        Item {
            state: TrayState { connected, unread },
            hub: StateHub::new(),
            click: crate::window::Toggle::default(),
        }
    }

    /// The report this comes from: messages waiting, and an icon that looked
    /// exactly like an idle one. The tooltip knew; nothing a glance reaches
    /// did.
    #[test]
    fn unread_reaches_the_icon_itself() {
        let idle = item(true, 0);
        let waiting = item(true, 3);

        assert_ne!(
            waiting.icon_name(),
            idle.icon_name(),
            "an icon with something to read must not look like one without"
        );
        assert_eq!(waiting.icon_name(), UNREAD_ICON);
        assert_eq!(waiting.status(), Status::NeedsAttention);
        assert_eq!(
            waiting.attention_icon_name(),
            waiting.icon_name(),
            "a host honouring the status and one ignoring it must show the same icon"
        );
        assert_eq!(idle.status(), Status::Active);
    }

    /// A count nothing is refreshing is not news. The connection is what the
    /// icon has to say then, and the tooltip already agreed.
    #[test]
    fn a_disconnected_icon_says_the_connection_and_not_a_stale_count() {
        let offline = item(false, 3);

        assert_eq!(offline.icon_name(), "user-offline");
        assert_eq!(offline.status(), Status::Active);
        assert_eq!(offline.tool_tip().description, "Disconnected");
        assert_eq!(offline.tool_tip().title, "oxidezap");
    }

    /// The number itself has nowhere on the icon to live: SNI carries no
    /// badge, so the tooltip is where it is said — in the title as well as
    /// the description, since a host may render only the first line.
    #[test]
    fn the_tooltip_carries_the_count() {
        assert_eq!(item(true, 0).tool_tip().title, "oxidezap");
        assert_eq!(item(true, 1).tool_tip().title, "oxidezap (1)");
        assert_eq!(item(true, 1).tool_tip().description, "1 unread message");
        assert_eq!(item(true, 4).tool_tip().description, "4 unread messages");
        assert_eq!(
            item(true, 4).tool_tip().icon_name,
            item(true, 4).icon_name(),
            "a tooltip that draws an icon draws the one being shown"
        );
    }
}
