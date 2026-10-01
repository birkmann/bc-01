//! Design system (PLAN §10.1): one set of primitives for every page.
pub mod brand;
pub mod button;
pub mod dialog;
pub mod icon;
pub mod menu;
pub mod misc;
pub mod popover;
pub mod select;
pub mod tabs;
pub mod toast;

use leptos::prelude::*;

pub use brand::{BrandLockup, BrandMark};
pub use button::{Button, Size, Variant};
pub use dialog::{ConfirmHost, Dialog, Sheet, SheetSide, confirm};
pub use icon::Icon;
pub use menu::{MenuButton, MenuCtx, MenuEntry, MenuHost, MenuItem, provide_menu};
pub use misc::*;
pub use popover::Tip;
pub use select::{Combobox, Select, SelectOption};
pub use tabs::{SegmentedControl, Tabs};
pub use toast::{ToastCentre, ToastHost, toast_err, toast_info, toast_ok, toast_warn};

/// Build a `ChildrenFn` from a closure (props like `actions=`, `footer=`).
pub fn children<V: IntoView + 'static>(f: impl Fn() -> V + Send + Sync + 'static) -> ChildrenFn {
    std::sync::Arc::new(move || f().into_any())
}

/// Reactive icon name for `Button icon=...` / `Icon name=...`.
pub fn dyn_icon(f: impl Fn() -> &'static str + Send + Sync + 'static) -> Signal<String> {
    Signal::derive(move || f().to_string())
}
