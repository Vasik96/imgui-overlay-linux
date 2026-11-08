//! "Forest green" Dear ImGui theme (deep pine backgrounds, moss-green controls).
//!
//! The COMPLETE palette is kept on purpose (tabs, tables, plots, docking, ...), even the
//! parts the overlay doesn't use yet, so new widgets are themed from day one.
//! Docking colors are behind the `docking` cargo feature (the `#ifdef IMGUI_HAS_DOCK` of the
//! original C++ theme); enable it together with imgui's own `docking` feature.

use imgui::{Style, StyleColor};

pub const ACCENT: [f32; 4] = [0.45, 0.75, 0.45, 1.0];
pub const ACCENT_BRIGHT: [f32; 4] = [0.60, 0.90, 0.60, 1.0];

pub fn apply_forest_green(style: &mut Style) {
    // --- sizing and spacing ---
    style.window_padding = [10.0, 10.0];
    style.frame_padding = [6.0, 4.0];
    style.cell_padding = [6.0, 4.0];
    style.item_spacing = [8.0, 6.0];
    style.item_inner_spacing = [6.0, 4.0];
    style.touch_extra_padding = [0.0, 0.0];
    style.indent_spacing = 20.0;
    style.scrollbar_size = 14.0;
    style.grab_min_size = 12.0;

    // --- borders ---
    style.window_border_size = 1.0;
    style.child_border_size = 1.0;
    style.popup_border_size = 1.0;
    style.frame_border_size = 1.0;
    style.tab_border_size = 0.0;

    // --- rounding ---
    style.window_rounding = 6.0;
    style.child_rounding = 4.0;
    style.frame_rounding = 4.0;
    style.popup_rounding = 4.0;
    style.scrollbar_rounding = 4.0;
    style.grab_rounding = 4.0;
    style.log_slider_deadzone = 4.0;
    style.tab_rounding = 4.0;

    // --- colors ---
    use StyleColor as C;
    let colors: &[(StyleColor, [f32; 4])] = &[
        // text
        (C::Text, [0.85, 0.90, 0.85, 1.00]),
        (C::TextDisabled, [0.50, 0.55, 0.50, 1.00]),
        // backgrounds
        (C::WindowBg, [0.06, 0.09, 0.06, 1.00]),
        (C::ChildBg, [0.08, 0.11, 0.08, 1.00]),
        (C::PopupBg, [0.07, 0.10, 0.07, 0.96]),
        // borders
        (C::Border, [0.18, 0.28, 0.18, 0.80]),
        (C::BorderShadow, [0.00, 0.00, 0.00, 0.00]),
        // frames
        (C::FrameBg, [0.12, 0.18, 0.12, 1.00]),
        (C::FrameBgHovered, [0.18, 0.30, 0.18, 1.00]),
        (C::FrameBgActive, [0.24, 0.42, 0.24, 1.00]),
        // title bars
        (C::TitleBg, [0.09, 0.14, 0.09, 1.00]),
        (C::TitleBgActive, [0.14, 0.26, 0.14, 1.00]),
        (C::TitleBgCollapsed, [0.05, 0.08, 0.05, 1.00]),
        (C::MenuBarBg, [0.09, 0.14, 0.09, 1.00]),
        // scrollbars
        (C::ScrollbarBg, [0.05, 0.08, 0.05, 1.00]),
        (C::ScrollbarGrab, [0.18, 0.28, 0.18, 1.00]),
        (C::ScrollbarGrabHovered, [0.25, 0.38, 0.25, 1.00]),
        (C::ScrollbarGrabActive, [0.32, 0.48, 0.32, 1.00]),
        // interactables
        (C::CheckMark, [0.45, 0.75, 0.45, 1.00]),
        (C::SliderGrab, [0.35, 0.55, 0.35, 1.00]),
        (C::SliderGrabActive, [0.45, 0.70, 0.45, 1.00]),
        (C::Button, [0.18, 0.35, 0.18, 1.00]),
        (C::ButtonHovered, [0.25, 0.45, 0.25, 1.00]),
        (C::ButtonActive, [0.32, 0.55, 0.32, 1.00]),
        (C::Header, [0.18, 0.35, 0.18, 1.00]),
        (C::HeaderHovered, [0.25, 0.45, 0.25, 1.00]),
        (C::HeaderActive, [0.32, 0.55, 0.32, 1.00]),
        // separators / resizing
        (C::Separator, [0.18, 0.28, 0.18, 1.00]),
        (C::SeparatorHovered, [0.25, 0.45, 0.25, 1.00]),
        (C::SeparatorActive, [0.32, 0.55, 0.32, 1.00]),
        (C::ResizeGrip, [0.18, 0.35, 0.18, 0.80]),
        (C::ResizeGripHovered, [0.25, 0.45, 0.25, 1.00]),
        (C::ResizeGripActive, [0.32, 0.55, 0.32, 1.00]),
        // tabs
        (C::Tab, [0.12, 0.22, 0.12, 1.00]),
        (C::TabHovered, [0.25, 0.45, 0.25, 1.00]),
        (C::TabActive, [0.20, 0.38, 0.20, 1.00]),
        (C::TabUnfocused, [0.08, 0.15, 0.08, 1.00]),
        (C::TabUnfocusedActive, [0.12, 0.22, 0.12, 1.00]),
        // plots
        (C::PlotLines, [0.40, 0.70, 0.40, 1.00]),
        (C::PlotLinesHovered, [0.50, 0.85, 0.50, 1.00]),
        (C::PlotHistogram, [0.40, 0.70, 0.40, 1.00]),
        (C::PlotHistogramHovered, [0.50, 0.85, 0.50, 1.00]),
        // tables
        (C::TableHeaderBg, [0.12, 0.22, 0.12, 1.00]),
        (C::TableBorderStrong, [0.20, 0.35, 0.20, 1.00]),
        (C::TableBorderLight, [0.15, 0.25, 0.15, 1.00]),
        (C::TableRowBg, [0.00, 0.00, 0.00, 0.00]),
        (C::TableRowBgAlt, [0.08, 0.14, 0.08, 0.50]),
        // misc
        (C::TextSelectedBg, [0.25, 0.55, 0.25, 0.50]),
        (C::DragDropTarget, [0.60, 0.90, 0.60, 1.00]),
        (C::NavHighlight, [0.40, 0.80, 0.40, 1.00]),
        (C::NavWindowingHighlight, [0.85, 0.90, 0.85, 0.70]),
        (C::NavWindowingDimBg, [0.10, 0.15, 0.10, 0.50]),
        (C::ModalWindowDimBg, [0.05, 0.08, 0.05, 0.60]),
    ];
    for (c, v) in colors {
        style[*c] = *v;
    }

    // docking (docking branch only)
    #[cfg(feature = "docking")]
    {
        style[C::DockingPreview] = [0.25, 0.55, 0.25, 0.50];
        style[C::DockingEmptyBg] = [0.06, 0.09, 0.06, 1.00];
    }
}
