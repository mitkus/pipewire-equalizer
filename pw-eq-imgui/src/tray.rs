use ksni::{
    Category, Status, ToolTip,
    menu::{CheckmarkItem, MenuItem, StandardItem},
};
use winit::event_loop::EventLoopProxy;

use crate::UserEvent;

pub const APP_ID: &str = "pw-eq-imgui";
pub const APP_TITLE: &str = "PipeWire Equalizer";
pub const ICON_NAME: &str = "audio-headphones";

#[derive(Debug, Clone, Copy)]
pub enum TrayEvent {
    ToggleWindow,
    ToggleBypass,
    Quit,
}

pub struct Tray {
    proxy: EventLoopProxy<UserEvent>,
    pub window_shown: bool,
    pub bypass: bool,
    pub eq_name: String,
}

impl Tray {
    pub fn new(proxy: EventLoopProxy<UserEvent>, window_shown: bool) -> Self {
        Self {
            proxy,
            window_shown,
            bypass: false,
            eq_name: String::new(),
        }
    }

    fn send(&self, event: TrayEvent) {
        if let Err(err) = self.proxy.send_event(UserEvent::Tray(event)) {
            tracing::warn!(error = ?err, "event loop closed, dropping tray event");
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        APP_ID.into()
    }

    fn title(&self) -> String {
        APP_TITLE.into()
    }

    fn icon_name(&self) -> String {
        ICON_NAME.into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn tool_tip(&self) -> ToolTip {
        let description = match (self.eq_name.is_empty(), self.bypass) {
            (true, _) => "No EQ loaded".to_string(),
            (false, true) => format!("{} (bypassed)", self.eq_name),
            (false, false) => self.eq_name.clone(),
        };
        ToolTip {
            icon_name: ICON_NAME.into(),
            title: APP_TITLE.into(),
            description,
            ..Default::default()
        }
    }

    /// Left click.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayEvent::ToggleWindow);
    }

    /// Middle click.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayEvent::ToggleBypass);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: if self.window_shown { "Hide" } else { "Show" }.into(),
                activate: Box::new(|this: &mut Self| this.send(TrayEvent::ToggleWindow)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Bypass".into(),
                checked: self.bypass,
                activate: Box::new(|this: &mut Self| this.send(TrayEvent::ToggleBypass)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| this.send(TrayEvent::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}
