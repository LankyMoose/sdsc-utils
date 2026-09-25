use crate::controller::model::ControllerStatus;
use tray_icon::Icon;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/icon_embedded.rs"));
}

const SIZE: u32 = 32;

pub fn icon_for_controllers(controllers: &[ControllerStatus]) -> Icon {
    let connected = !controllers.is_empty();
    let rgba = if connected {
        embedded::CONNECTED_RGBA
    } else {
        embedded::DIM_RGBA
    };
    Icon::from_rgba(rgba.to_vec(), SIZE, SIZE).expect("valid 32x32 rgba icon")
}

pub fn tooltip_for_controllers(controllers: &[ControllerStatus]) -> String {
    let count = controllers.len();
    match count {
        0 => "No DualSense connected".into(),
        1 => "1 controller connected".into(),
        n => format!("{n} controllers connected"),
    }
}
