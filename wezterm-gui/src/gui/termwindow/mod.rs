#![cfg_attr(feature = "cargo-clippy", allow(clippy::range_plus_one))]
use super::quad::*;
use super::renderstate::*;
use super::utilsprites::RenderMetrics;
use crate::gui::glium::texture::SrgbTexture2d;
use crate::gui::overlay::{
    confirm_close_pane, confirm_close_tab, confirm_close_window, confirm_quit_program, launcher,
    start_overlay, start_overlay_pane, tab_navigator, CopyOverlay, SearchOverlay,
};
use crate::gui::scrollbar::*;
use crate::gui::selection::*;
use crate::gui::shapecache::*;
use crate::gui::tabbar::TabBarState;
use crate::scripting::guiwin::GuiWin;
use crate::scripting::pane::PaneObject;
use ::wezterm_term::input::MouseButton as TMB;
use ::window::*;
use anyhow::{anyhow, ensure};
use config::keyassignment::{
    ClipboardCopyDestination, ClipboardPasteSource, InputMap, KeyAssignment, SpawnCommand,
};
use config::{configuration, ConfigHandle, WindowCloseConfirmation};
use lru::LruCache;
use mux::activity::Activity;
use mux::domain::{DomainId, DomainState};
use mux::pane::{Pane, PaneId};
use mux::renderable::RenderableDimensions;
use mux::tab::{PositionedPane, PositionedSplit, SplitDirection, TabId};
use mux::window::WindowId as MuxWindowId;
use mux::Mux;
use portable_pty::PtySize;
use std::any::Any;
use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::ops::Add;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use termwiz::hyperlink::Hyperlink;
use termwiz::image::ImageData;
use wezterm_font::FontConfiguration;
use wezterm_term::color::ColorPalette;
use wezterm_term::input::LastMouseClick;
use wezterm_term::{StableRowIndex, TerminalConfiguration};

pub mod clipboard;
mod keyevent;
mod mouseevent;
mod prevcursor;
mod render;
pub mod spawn;
use clipboard::ClipboardHelper;
use prevcursor::PrevCursorPos;
use spawn::SpawnWhere;

const ATLAS_SIZE: usize = 128;

lazy_static::lazy_static! {
    static ref WINDOW_CLASS: Mutex<String> = Mutex::new("org.wezfurlong.wezterm".to_owned());
}

pub const ICON_DATA: &'static [u8] = include_bytes!("../../../../assets/icon/terminal.png");

pub fn set_window_class(cls: &str) {
    *WINDOW_CLASS.lock().unwrap() = cls.to_owned();
}

#[derive(Debug, Clone, Copy)]
struct RowsAndCols {
    rows: usize,
    cols: usize,
}

#[derive(Default, Clone)]
pub struct PaneState {
    /// If is_some(), the top row of the visible screen.
    /// Otherwise, the viewport is at the bottom of the
    /// scrollback.
    viewport: Option<StableRowIndex>,
    selection: Selection,
    /// If is_some(), rather than display the actual tab
    /// contents, we're overlaying a little internal application
    /// tab.  We'll also route input to it.
    pub overlay: Option<Rc<dyn Pane>>,
}

#[derive(Default, Clone)]
pub struct TabState {
    /// If is_some(), rather than display the actual tab
    /// contents, we're overlaying a little internal application
    /// tab.  We'll also route input to it.
    pub overlay: Option<Rc<dyn Pane>>,
}

pub struct TermWindow {
    pub window: Option<Window>,
    pub config: ConfigHandle,
    pub config_overrides: serde_json::Value,
    /// When we most recently received keyboard focus
    focused: Option<Instant>,
    fonts: Rc<FontConfiguration>,
    /// Window dimensions and dpi
    dimensions: Dimensions,
    /// Terminal dimensions
    terminal_size: PtySize,
    pub mux_window_id: MuxWindowId,
    pub render_metrics: RenderMetrics,
    render_state: Option<RenderState>,
    input_map: InputMap,
    /// If is_some, the LEADER modifier is active until the specified instant.
    leader_is_down: Option<std::time::Instant>,
    show_tab_bar: bool,
    show_scroll_bar: bool,
    tab_bar: TabBarState,
    last_mouse_coords: (usize, i64),
    last_mouse_terminal_coords: (usize, StableRowIndex),
    scroll_drag_start: Option<isize>,
    split_drag_start: Option<PositionedSplit>,
    prev_cursor: PrevCursorPos,
    last_scroll_info: RenderableDimensions,

    tab_state: RefCell<HashMap<TabId, TabState>>,
    pane_state: RefCell<HashMap<PaneId, PaneState>>,

    window_background: Option<Arc<ImageData>>,

    /// Gross workaround for managing async keyboard fetching
    /// just for middle mouse button paste function
    clipboard_contents: Arc<Mutex<Option<String>>>,

    current_mouse_button: Option<MousePress>,

    /// Keeps track of double and triple clicks
    last_mouse_click: Option<LastMouseClick>,

    /// The URL over which we are currently hovering
    current_highlight: Option<Arc<Hyperlink>>,

    shape_cache:
        RefCell<LruCache<ShapeCacheKey, anyhow::Result<Rc<Vec<ShapedInfo<SrgbTexture2d>>>>>>,

    last_blink_paint: Instant,

    palette: Option<ColorPalette>,
}

fn mouse_press_to_tmb(press: &MousePress) -> TMB {
    match press {
        MousePress::Left => TMB::Left,
        MousePress::Right => TMB::Right,
        MousePress::Middle => TMB::Middle,
    }
}

impl WindowCallbacks for TermWindow {
    fn can_close(&mut self) -> bool {
        let mux = Mux::get().unwrap();
        match self.config.window_close_confirmation {
            WindowCloseConfirmation::NeverPrompt => {
                // Immediately kill the tabs and allow the window to close
                mux.kill_window(self.mux_window_id);
                true
            }
            WindowCloseConfirmation::AlwaysPrompt => {
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return true,
                };

                let mux_window_id = self.mux_window_id;

                let can_close = mux
                    .get_window(mux_window_id)
                    .map_or(false, |w| w.can_close_without_prompting());
                if can_close {
                    mux.kill_window(self.mux_window_id);
                    return true;
                }
                let window = self.window.clone().unwrap();
                let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
                    confirm_close_window(term, mux_window_id, window, tab_id)
                });
                self.assign_overlay(tab.tab_id(), overlay);
                promise::spawn::spawn(future).detach();

                // Don't close right now; let the close happen from
                // the confirmation overlay
                false
            }
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn focus_change(&mut self, focused: bool) {
        log::trace!("Setting focus to {:?}", focused);
        self.focused = if focused { Some(Instant::now()) } else { None };

        if self.focused.is_none() {
            self.last_mouse_click = None;
            self.current_mouse_button = None;
        }

        // Reset the cursor blink phase
        self.prev_cursor.bump();

        // force cursor to be repainted
        self.window.as_ref().unwrap().invalidate();

        if let Some(pane) = self.get_active_pane_or_overlay() {
            pane.focus_changed(focused);
        }
    }

    fn mouse_event(&mut self, event: &MouseEvent, context: &dyn WindowOps) {
        self.mouse_event_impl(event, context)
    }

    fn resize(&mut self, dimensions: Dimensions) {
        log::trace!(
            "resize event, current cells: {:?}, new dims: {:?}",
            self.current_cell_dimensions(),
            dimensions
        );
        if dimensions.pixel_width == 0 || dimensions.pixel_height == 0 {
            // on windows, this can happen when minimizing the window.
            // NOP!
            return;
        }
        self.scaling_changed(dimensions, self.fonts.get_font_scale());
    }

    fn key_event(&mut self, window_key: &KeyEvent, context: &dyn WindowOps) -> bool {
        self.key_event_impl(window_key, context)
    }

    fn opengl_context_lost(&mut self, prior_window: &dyn WindowOps) -> anyhow::Result<()> {
        log::error!("context was lost, set up a new window");
        let activity = Activity::new();

        let render_state = None;

        let clipboard_contents = Arc::clone(&self.clipboard_contents);
        let dimensions = self.dimensions.clone();
        let mux_window_id = self.mux_window_id;

        let guts = Box::new(Self {
            window: None,
            config: self.config.clone(),
            config_overrides: self.config_overrides.clone(),
            window_background: self.window_background.clone(),
            palette: None,
            focused: None,
            mux_window_id,
            fonts: Rc::clone(&self.fonts),
            render_metrics: self.render_metrics.clone(),
            dimensions,
            terminal_size: self.terminal_size.clone(),
            render_state,
            input_map: InputMap::new(),
            leader_is_down: None,
            show_tab_bar: self.show_tab_bar,
            show_scroll_bar: self.show_scroll_bar,
            tab_bar: self.tab_bar.clone(),
            last_mouse_coords: self.last_mouse_coords.clone(),
            last_mouse_terminal_coords: self.last_mouse_terminal_coords.clone(),
            scroll_drag_start: self.scroll_drag_start.clone(),
            split_drag_start: self.split_drag_start.clone(),
            prev_cursor: self.prev_cursor.clone(),
            last_scroll_info: self.last_scroll_info.clone(),
            clipboard_contents: Arc::clone(&clipboard_contents),
            tab_state: RefCell::new(self.tab_state.borrow().clone()),
            pane_state: RefCell::new(self.pane_state.borrow().clone()),
            current_mouse_button: self.current_mouse_button.clone(),
            last_mouse_click: self.last_mouse_click.clone(),
            current_highlight: self.current_highlight.clone(),
            shape_cache: RefCell::new(LruCache::new(65536)),
            last_blink_paint: Instant::now(),
        });
        prior_window.close();

        promise::spawn::spawn(async move {
            smol::Timer::after(Duration::from_millis(300)).await;
            log::error!("now try making that new window");
            let window = Window::new_window(
                &*WINDOW_CLASS.lock().unwrap(),
                "wezterm",
                dimensions.pixel_width,
                dimensions.pixel_height,
                guts,
            )?;

            Self::apply_icon(&window)?;
            Self::start_periodic_maintenance(window.clone());
            Self::setup_clipboard(&window, mux_window_id, clipboard_contents);

            drop(activity); // Keep the activity outstanding until we get here
            Ok::<(), anyhow::Error>(())
        })
        .detach();

        Ok(())
    }

    fn created(
        &mut self,
        window: &Window,
        ctx: std::rc::Rc<glium::backend::Context>,
    ) -> anyhow::Result<()> {
        self.window.replace(window.clone());

        self.render_state = None;

        match RenderState::new(
            ctx,
            &self.fonts,
            &self.render_metrics,
            ATLAS_SIZE,
            self.dimensions.pixel_width,
            self.dimensions.pixel_height,
        ) {
            Ok(gl) => {
                log::info!(
                    "OpenGL initialized! {} {} is_context_loss_possible={} wezterm version: {}",
                    gl.context.get_opengl_renderer_string(),
                    gl.context.get_opengl_version_string(),
                    gl.context.is_context_loss_possible(),
                    config::wezterm_version(),
                );
                self.render_state.replace(gl);
            }
            Err(err) => {
                log::error!("failed to create OpenGLRenderState: {}", err);
            }
        }

        window.show();

        if self.render_state.is_none() {
            panic!("No OpenGL");
        }

        Ok(())
    }

    fn paint(&mut self, frame: &mut glium::Frame) {
        self.paint_impl(frame)
    }
}

/// Computes the effective padding for the RHS.
/// This is needed because the default is 0, but if the user has
/// enabled the scroll bar then they will expect it to have a reasonable
/// size unless they've specified differently.
pub fn effective_right_padding(config: &ConfigHandle, render_metrics: &RenderMetrics) -> u16 {
    if config.enable_scroll_bar && config.window_padding.right == 0 {
        render_metrics.cell_size.width as u16
    } else {
        config.window_padding.right as u16
    }
}

fn load_background_image(config: &ConfigHandle) -> Option<Arc<ImageData>> {
    match &config.window_background_image {
        Some(p) => match std::fs::read(p) {
            Ok(data) => {
                log::error!("loaded {}", p.display());
                Some(Arc::new(ImageData::with_raw_data(data)))
            }
            Err(err) => {
                log::error!(
                    "Failed to load window_background_image {}: {}",
                    p.display(),
                    err
                );
                None
            }
        },
        None => None,
    }
}

fn reload_background_image(
    config: &ConfigHandle,
    image: &Option<Arc<ImageData>>,
) -> Option<Arc<ImageData>> {
    match &config.window_background_image {
        Some(p) => match std::fs::read(p) {
            Ok(data) => {
                if let Some(existing) = image {
                    if existing.data() == &*data {
                        return Some(Arc::clone(existing));
                    }
                }
                Some(Arc::new(ImageData::with_raw_data(data)))
            }
            Err(err) => {
                log::error!(
                    "Failed to load window_background_image {}: {}",
                    p.display(),
                    err
                );
                None
            }
        },
        None => None,
    }
}

impl TermWindow {
    pub fn new_window(mux_window_id: MuxWindowId) -> anyhow::Result<()> {
        let config = configuration();

        let window_background = load_background_image(&config);

        let fontconfig = Rc::new(FontConfiguration::new(Some(config.clone()))?);
        let mux = Mux::get().expect("to be main thread with mux running");
        let size = match mux.get_active_tab_for_window(mux_window_id) {
            Some(tab) => tab.get_size(),
            None => {
                log::error!("new_window has no tabs... yet?");
                Default::default()
            }
        };
        let physical_rows = size.rows as usize;
        let physical_cols = size.cols as usize;

        let render_metrics = RenderMetrics::new(&fontconfig)?;
        log::trace!("using render_metrics {:#?}", render_metrics);

        let terminal_size = PtySize {
            rows: physical_rows as u16,
            cols: physical_cols as u16,
            pixel_width: (render_metrics.cell_size.width as usize * physical_cols) as u16,
            pixel_height: (render_metrics.cell_size.height as usize * physical_rows) as u16,
        };

        // Initially we have only a single tab, so take that into account
        // for the tab bar state.
        let show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;

        let rows_with_tab_bar = if show_tab_bar { 1 } else { 0 } + terminal_size.rows;

        let dimensions = Dimensions {
            pixel_width: ((terminal_size.cols * render_metrics.cell_size.width as u16)
                + config.window_padding.left
                + effective_right_padding(&config, &render_metrics))
                as usize,
            pixel_height: ((rows_with_tab_bar * render_metrics.cell_size.height as u16)
                + config.window_padding.top
                + config.window_padding.bottom) as usize,
            dpi: config.dpi.unwrap_or(::window::DEFAULT_DPI) as usize,
        };

        log::trace!(
            "TermWindow::new_window called with mux_window_id {} {:?} {:?}",
            mux_window_id,
            terminal_size,
            dimensions
        );

        let render_state = None;

        let clipboard_contents = Arc::new(Mutex::new(None));

        let window = Window::new_window(
            &*WINDOW_CLASS.lock().unwrap(),
            "wezterm",
            dimensions.pixel_width,
            dimensions.pixel_height,
            Box::new(Self {
                window: None,
                window_background,
                config: config.clone(),
                config_overrides: serde_json::Value::default(),
                palette: None,
                focused: None,
                mux_window_id,
                fonts: fontconfig,
                render_metrics,
                dimensions,
                terminal_size,
                render_state,
                input_map: InputMap::new(),
                leader_is_down: None,
                show_tab_bar,
                show_scroll_bar: config.enable_scroll_bar,
                tab_bar: TabBarState::default(),
                last_mouse_coords: (0, -1),
                last_mouse_terminal_coords: (0, 0),
                scroll_drag_start: None,
                split_drag_start: None,
                prev_cursor: PrevCursorPos::new(),
                last_scroll_info: RenderableDimensions::default(),
                clipboard_contents: Arc::clone(&clipboard_contents),
                tab_state: RefCell::new(HashMap::new()),
                pane_state: RefCell::new(HashMap::new()),
                current_mouse_button: None,
                last_mouse_click: None,
                current_highlight: None,
                shape_cache: RefCell::new(LruCache::new(65536)),
                last_blink_paint: Instant::now(),
            }),
        )?;

        Self::apply_icon(&window)?;
        Self::start_periodic_maintenance(window.clone());
        Self::setup_clipboard(&window, mux_window_id, clipboard_contents);

        crate::update::start_update_checker();
        Ok(())
    }

    fn apply_icon(window: &Window) -> anyhow::Result<()> {
        let icon_image = image::load_from_memory(ICON_DATA)?;
        let image = icon_image.to_bgra8();
        let (width, height) = image.dimensions();
        window.set_icon(Image::from_raw(
            width as usize,
            height as usize,
            image.into_raw(),
        ));
        Ok(())
    }

    fn start_periodic_maintenance(window: Window) {
        Connection::get().unwrap().schedule_timer(
            std::time::Duration::from_millis(35),
            move || {
                window.apply(move |myself, window| {
                    if let Some(myself) = myself.downcast_mut::<Self>() {
                        myself.periodic_window_maintenance(window)?;
                    }
                    Ok(())
                });
            },
        );
    }

    fn periodic_window_maintenance(&mut self, _window: &dyn WindowOps) -> anyhow::Result<()> {
        let mux = Mux::get().unwrap();

        let mut needs_invalidate = false;
        // If the config was reloaded, ask the window to apply
        // and render any changes
        self.check_for_config_reload();

        let panes = self.get_panes_to_render();
        if panes.is_empty() {
            self.window.as_ref().unwrap().close();
            return Ok(());
        }

        for pos in panes {
            // If blinking is permitted, and the cursor shape is set
            // to a blinking variant, and it's been longer than the
            // blink rate interval, then invalidate and redraw
            // so that we will re-evaluate the cursor visibility.
            // This is pretty heavyweight: it would be nice to only invalidate
            // the line on which the cursor resides, and then only if the cursor
            // is within the viewport.
            if self.config.cursor_blink_rate != 0 && pos.is_active && self.focused.is_some() {
                let shape = self
                    .config
                    .default_cursor_style
                    .effective_shape(pos.pane.get_cursor_position().shape);
                if shape.is_blinking() {
                    let now = Instant::now();
                    if now.duration_since(self.last_blink_paint)
                        > Duration::from_millis(self.config.cursor_blink_rate)
                    {
                        needs_invalidate = true;
                        self.last_blink_paint = now;
                    }
                }
            }

            // If the model is dirty, arrange to re-paint
            let dims = pos.pane.get_dimensions();
            let viewport = self
                .get_viewport(pos.pane.pane_id())
                .unwrap_or(dims.physical_top);
            let visible_range = viewport..viewport + dims.viewport_rows as StableRowIndex;
            let dirty = pos.pane.get_dirty_lines(visible_range);

            if !dirty.is_empty() {
                if pos.pane.downcast_ref::<SearchOverlay>().is_none()
                    && pos.pane.downcast_ref::<CopyOverlay>().is_none()
                {
                    // If any of the changed lines intersect with the
                    // selection, then we need to clear the selection, but not
                    // when the search overlay is active; the search overlay
                    // marks lines as dirty to force invalidate them for
                    // highlighting purpose but also manipulates the selection
                    // and we want to allow it to retain the selection it made!

                    let clear_selection = if let Some(selection_range) =
                        self.selection(pos.pane.pane_id()).range.as_ref()
                    {
                        let selection_rows = selection_range.rows();
                        selection_rows.into_iter().any(|row| dirty.contains(row))
                    } else {
                        false
                    };

                    if clear_selection {
                        self.selection(pos.pane.pane_id()).range.take();
                        self.selection(pos.pane.pane_id()).start.take();
                    }
                }

                needs_invalidate = true;
            }
        }

        if let Some(mut mux_window) = mux.get_window_mut(self.mux_window_id) {
            if mux_window.check_and_reset_invalidated() {
                needs_invalidate = true;
            }
        }

        if needs_invalidate {
            if let Some(ref win) = self.window {
                win.invalidate();
            }
        }

        Ok(())
    }
}

impl TermWindow {
    fn check_for_config_reload(&mut self) {
        if self.config.generation() != configuration().generation() {
            self.config_was_reloaded();
        }
    }

    fn palette(&mut self) -> &ColorPalette {
        if self.palette.is_none() {
            self.palette.replace(config::TermConfig.color_palette());
        }
        self.palette.as_ref().unwrap()
    }

    pub fn config_was_reloaded(&mut self) {
        log::debug!(
            "config was reloaded, overrides: {:?}",
            self.config_overrides
        );
        let config = match config::overridden_config(&self.config_overrides) {
            Ok(config) => config,
            Err(err) => {
                log::error!(
                    "Failed to apply config overrides to window: {:#}: {:?}",
                    err,
                    self.config_overrides
                );
                configuration()
            }
        };
        self.config = config.clone();
        self.palette.take();

        self.window_background = reload_background_image(&config, &self.window_background);

        let mux = Mux::get().unwrap();
        let window = match mux.get_window(self.mux_window_id) {
            Some(window) => window,
            _ => return,
        };
        if window.len() == 1 {
            self.show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;
        } else {
            self.show_tab_bar = config.enable_tab_bar;
        }

        self.show_scroll_bar = config.enable_scroll_bar;
        self.shape_cache.borrow_mut().clear();
        self.input_map = InputMap::new();
        self.leader_is_down = None;
        let dimensions = self.dimensions;
        let cell_dims = self.current_cell_dimensions();

        if let Err(err) = self.fonts.config_changed(&config) {
            log::error!("Failed to load font configuration: {:#}", err);
        }
        self.apply_scale_change(&dimensions, self.fonts.get_font_scale());
        self.apply_dimensions(&dimensions, Some(cell_dims));
        if let Some(window) = self.window.as_ref() {
            window.config_did_change();
            window.invalidate();
        }
    }

    fn update_scrollbar(&mut self) {
        if !self.show_scroll_bar {
            return;
        }

        let tab = match self.get_active_pane_or_overlay() {
            Some(tab) => tab,
            None => return,
        };

        let render_dims = tab.get_dimensions();
        if render_dims == self.last_scroll_info {
            return;
        }

        self.last_scroll_info = render_dims;

        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn update_title(&mut self) {
        let mux = Mux::get().unwrap();
        let window = match mux.get_window(self.mux_window_id) {
            Some(window) => window,
            _ => return,
        };

        let new_tab_bar = TabBarState::new(
            self.terminal_size.cols as usize,
            if self.last_mouse_coords.1 == 0 {
                Some(self.last_mouse_coords.0)
            } else {
                None
            },
            &window,
            self.config.colors.as_ref().and_then(|c| c.tab_bar.as_ref()),
            &self.config,
        );
        if new_tab_bar != self.tab_bar {
            self.tab_bar = new_tab_bar;
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }

        let num_tabs = window.len();

        if num_tabs == 0 {
            return;
        }

        let tab_no = window.get_active_idx();
        drop(window);

        let panes = self.get_panes_to_render();
        if let Some(pos) = panes.iter().find(|p| p.is_active) {
            let title = pos.pane.get_title();

            if let Some(window) = self.window.as_ref() {
                let show_tab_bar;
                if num_tabs == 1 {
                    window.set_title(&format!(
                        "{}{}",
                        if pos.is_zoomed { "[Z] " } else { "" },
                        title
                    ));
                    show_tab_bar =
                        self.config.enable_tab_bar && !self.config.hide_tab_bar_if_only_one_tab;
                } else {
                    window.set_title(&format!(
                        "{}[{}/{}] {}",
                        if pos.is_zoomed { "[Z] " } else { "" },
                        tab_no + 1,
                        num_tabs,
                        title
                    ));
                    show_tab_bar = self.config.enable_tab_bar;
                }

                // If the number of tabs changed and caused the tab bar to
                // hide/show, then we'll need to resize things.  It is simplest
                // to piggy back on the config reloading code for that, so that
                // is what we're doing.
                if show_tab_bar != self.show_tab_bar {
                    self.config_was_reloaded();
                }
            }
        }
    }

    fn update_text_cursor(&mut self, pane: &Rc<dyn Pane>) {
        let cursor = pane.get_cursor_position();
        if let Some(win) = self.window.as_ref() {
            let config = &self.config;
            let top = pane.get_dimensions().physical_top + if self.show_tab_bar { -1 } else { 0 };
            let r = Rect::new(
                Point::new(
                    (cursor.x.max(0) as isize * self.render_metrics.cell_size.width)
                        .add(config.window_padding.left as isize),
                    ((cursor.y - top).max(0) as isize * self.render_metrics.cell_size.height)
                        .add(config.window_padding.top as isize),
                ),
                self.render_metrics.cell_size,
            );
            win.set_text_cursor_position(r);
        }
    }

    fn activate_tab(&mut self, tab_idx: isize) -> anyhow::Result<()> {
        if let Some(tab) = self.get_active_pane_or_overlay() {
            tab.focus_changed(false);
        }

        let mux = Mux::get().unwrap();
        let mut window = mux
            .get_window_mut(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();

        let tab_idx = if tab_idx < 0 {
            max.saturating_sub(tab_idx.abs() as usize)
        } else {
            tab_idx as usize
        };

        if tab_idx < max {
            window.set_active(tab_idx);

            drop(window);

            if let Some(tab) = self.get_active_pane_or_overlay() {
                tab.focus_changed(true);
            }

            self.update_title();
            self.update_scrollbar();
        }
        Ok(())
    }

    fn activate_tab_relative(&mut self, delta: isize) -> anyhow::Result<()> {
        let mux = Mux::get().unwrap();
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        let active = window.get_active_idx() as isize;
        let tab = active + delta;
        let tab = if tab < 0 { max as isize + tab } else { tab };
        drop(window);
        self.activate_tab((tab as usize % max) as isize)
    }

    fn move_tab(&mut self, tab_idx: usize) -> anyhow::Result<()> {
        let mux = Mux::get().unwrap();
        let mut window = mux
            .get_window_mut(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        let active = window.get_active_idx();

        ensure!(tab_idx < max, "cannot move a tab out of range");

        let tab_inst = window.remove_by_idx(active);
        window.insert(tab_idx, &tab_inst);
        window.set_active(tab_idx);

        drop(window);
        self.update_title();
        self.update_scrollbar();

        Ok(())
    }

    fn show_tab_navigator(&mut self) {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let window = mux
            .get_window(self.mux_window_id)
            .expect("to resolve my own window_id");

        // Ideally we'd resolve the tabs on the fly once we've started the
        // overlay, but since the overlay runs in a different thread, accessing
        // the mux list is a bit awkward.  To get the ball rolling we capture
        // the list of tabs up front and live with a static list.
        let tabs: Vec<(String, TabId, usize)> = window
            .iter()
            .map(|tab| {
                (
                    tab.get_active_pane()
                        .expect("tab to have a pane")
                        .get_title(),
                    tab.tab_id(),
                    tab.count_panes(),
                )
            })
            .collect();

        let mux_window_id = self.mux_window_id;
        let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
            tab_navigator(tab_id, term, tabs, mux_window_id)
        });
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
    }

    fn show_launcher(&mut self) {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let mux_window_id = self.mux_window_id;

        let clipboard = ClipboardHelper {
            window: self.window.as_ref().unwrap().clone(),
            clipboard_contents: Arc::clone(&self.clipboard_contents),
        };

        let mut domains = mux.iter_domains();
        domains.sort_by(|a, b| {
            let a_state = a.state();
            let b_state = b.state();
            if a_state != b_state {
                use std::cmp::Ordering;
                return if a_state == DomainState::Attached {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            a.domain_id().cmp(&b.domain_id())
        });
        domains.retain(|dom| dom.spawnable());
        let domains: Vec<(DomainId, DomainState, String)> = domains
            .iter()
            .map(|dom| {
                let name = dom.domain_name();
                let label = dom.domain_label();
                let label = if name == label || label == "" {
                    format!("domain `{}`", name)
                } else {
                    format!("domain `{}` - {}", name, label)
                };
                (dom.domain_id(), dom.state(), label)
            })
            .collect();

        let domain_id_of_current_pane = tab
            .get_active_pane()
            .expect("tab has no panes!")
            .domain_id();
        let size = self.terminal_size;

        let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
            launcher(
                tab_id,
                domain_id_of_current_pane,
                term,
                mux_window_id,
                domains,
                clipboard,
                size,
            )
        });
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
    }

    fn scroll_to_prompt(&mut self, amount: isize) -> anyhow::Result<()> {
        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return Ok(()),
        };
        let dims = pane.get_dimensions();
        let position = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top);
        let mut zones = pane.get_semantic_zones()?;
        zones.retain(|zone| zone.semantic_type == wezterm_term::SemanticType::Prompt);
        let idx = match zones.binary_search_by(|zone| zone.start_y.cmp(&position)) {
            Ok(idx) | Err(idx) => idx,
        };
        let idx = ((idx as isize) + amount).max(0) as usize;
        if let Some(zone) = zones.get(idx) {
            self.set_viewport(pane.pane_id(), Some(zone.start_y), dims);
        }

        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    fn scroll_by_page(&mut self, amount: isize) -> anyhow::Result<()> {
        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return Ok(()),
        };
        let dims = pane.get_dimensions();
        let position = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top)
            .saturating_add(amount * dims.viewport_rows as isize);
        self.set_viewport(pane.pane_id(), Some(position), dims);
        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    fn scroll_by_line(&mut self, amount: isize) -> anyhow::Result<()> {
        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return Ok(()),
        };
        let dims = pane.get_dimensions();
        let position = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top)
            .saturating_add(amount);
        self.set_viewport(pane.pane_id(), Some(position), dims);
        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    fn move_tab_relative(&mut self, delta: isize) -> anyhow::Result<()> {
        let mux = Mux::get().unwrap();
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        let active = window.get_active_idx();
        let tab = active as isize + delta;
        let tab = if tab < 0 {
            0usize
        } else if tab >= max as isize {
            max - 1
        } else {
            tab as usize
        };

        drop(window);
        self.move_tab(tab)
    }

    fn selection_text(&self, pane: &Rc<dyn Pane>) -> String {
        let mut s = String::new();
        if let Some(sel) = self
            .selection(pane.pane_id())
            .range
            .as_ref()
            .map(|r| r.normalize())
        {
            let mut last_was_wrapped = false;
            let (first_row, lines) = pane.get_lines(sel.rows());
            for (idx, line) in lines.iter().enumerate() {
                let cols = sel.cols_for_row(first_row + idx as StableRowIndex);
                let last_col_idx = cols.end.min(line.cells().len()).saturating_sub(1);
                if !s.is_empty() && !last_was_wrapped {
                    s.push('\n');
                }
                s.push_str(line.columns_as_str(cols).trim_end());

                let last_cell = &line.cells()[last_col_idx];
                // TODO: should really test for any unicode whitespace
                last_was_wrapped = last_cell.attrs().wrapped() && last_cell.str() != " ";
            }
        }

        s
    }

    pub fn perform_key_assignment(
        &mut self,
        pane: &Rc<dyn Pane>,
        assignment: &KeyAssignment,
    ) -> anyhow::Result<()> {
        use KeyAssignment::*;
        match assignment {
            SpawnTab(spawn_where) => {
                self.spawn_tab(spawn_where);
            }
            SpawnWindow => {
                self.spawn_command(&SpawnCommand::default(), SpawnWhere::NewWindow);
            }
            SpawnCommandInNewTab(spawn) => {
                self.spawn_command(spawn, SpawnWhere::NewTab);
            }
            SpawnCommandInNewWindow(spawn) => {
                self.spawn_command(spawn, SpawnWhere::NewWindow);
            }
            SplitHorizontal(spawn) => {
                log::trace!("SplitHorizontal {:?}", spawn);
                self.spawn_command(spawn, SpawnWhere::SplitPane(SplitDirection::Horizontal));
            }
            SplitVertical(spawn) => {
                log::trace!("SplitVertical {:?}", spawn);
                self.spawn_command(spawn, SpawnWhere::SplitPane(SplitDirection::Vertical));
            }
            ToggleFullScreen => {
                self.window.as_ref().unwrap().toggle_fullscreen();
            }
            Copy => {
                let text = self.selection_text(pane);
                self.copy_to_clipboard(
                    ClipboardCopyDestination::ClipboardAndPrimarySelection,
                    text,
                );
            }
            CopyTo(dest) => {
                let text = self.selection_text(pane);
                self.copy_to_clipboard(*dest, text);
            }
            Paste => {
                self.paste_from_clipboard(pane, ClipboardPasteSource::Clipboard);
            }
            PastePrimarySelection => {
                self.paste_from_clipboard(pane, ClipboardPasteSource::PrimarySelection);
            }
            PasteFrom(source) => {
                self.paste_from_clipboard(pane, *source);
            }
            ActivateTabRelative(n) => {
                self.activate_tab_relative(*n)?;
            }
            DecreaseFontSize => self.decrease_font_size(),
            IncreaseFontSize => self.increase_font_size(),
            ResetFontSize => self.reset_font_size(),
            ResetFontAndWindowSize => self.reset_font_and_window_size()?,
            ActivateTab(n) => {
                self.activate_tab(*n)?;
            }
            SendString(s) => pane.writer().write_all(s.as_bytes())?,
            Hide => {
                if let Some(w) = self.window.as_ref() {
                    w.hide();
                }
            }
            Show => {
                if let Some(w) = self.window.as_ref() {
                    w.show();
                }
            }
            CloseCurrentTab { confirm } => self.close_current_tab(*confirm),
            CloseCurrentPane { confirm } => self.close_current_pane(*confirm),
            Nop | DisableDefaultAssignment => {}
            ReloadConfiguration => config::reload(),
            MoveTab(n) => self.move_tab(*n)?,
            MoveTabRelative(n) => self.move_tab_relative(*n)?,
            ScrollByPage(n) => self.scroll_by_page(*n)?,
            ScrollByLine(n) => self.scroll_by_line(*n)?,
            ScrollToPrompt(n) => self.scroll_to_prompt(*n)?,
            ShowTabNavigator => self.show_tab_navigator(),
            ShowLauncher => self.show_launcher(),
            HideApplication => {
                let con = Connection::get().expect("call on gui thread");
                con.hide_application();
            }
            QuitApplication => {
                let mux = Mux::get().unwrap();
                let config = &self.config;

                match config.window_close_confirmation {
                    WindowCloseConfirmation::NeverPrompt => {
                        let con = Connection::get().expect("call on gui thread");
                        con.terminate_message_loop();
                    }
                    WindowCloseConfirmation::AlwaysPrompt => {
                        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                            Some(tab) => tab,
                            None => anyhow::bail!("no active tab!?"),
                        };

                        let window = self.window.clone().unwrap();
                        let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
                            confirm_quit_program(term, window, tab_id)
                        });
                        self.assign_overlay(tab.tab_id(), overlay);
                        promise::spawn::spawn(future).detach();
                    }
                }
            }
            SelectTextAtMouseCursor(mode) => self.select_text_at_mouse_cursor(*mode, pane),
            ExtendSelectionToMouseCursor(mode) => {
                self.extend_selection_at_mouse_cursor(*mode, pane)
            }
            OpenLinkAtMouseCursor => {
                // They clicked on a link, so let's open it!
                // We need to ensure that we spawn the `open` call outside of the context
                // of our window loop; on Windows it can cause a panic due to
                // triggering our WndProc recursively.
                // We get that assurance for free as part of the async dispatch that we
                // perform below; here we allow the user to define an `open-uri` event
                // handler that can bypass the normal `open::that` functionality.
                if let Some(link) = self.current_highlight.as_ref().cloned() {
                    let window = GuiWin::new(self);
                    let pane = PaneObject::new(pane);

                    async fn open_uri(
                        lua: Option<Rc<mlua::Lua>>,
                        window: GuiWin,
                        pane: PaneObject,
                        link: String,
                    ) -> anyhow::Result<()> {
                        let default_click = match lua {
                            Some(lua) => {
                                let args = lua.pack_multi((window, pane, link.clone()))?;
                                config::lua::emit_event(&lua, ("open-uri".to_string(), args))
                                    .await
                                    .map_err(|e| {
                                        log::error!("while processing open-uri event: {:#}", e);
                                        e
                                    })?
                            }
                            None => true,
                        };
                        if default_click {
                            log::info!("clicking {}", link);
                            if let Err(err) = open::that(&link) {
                                log::error!("failed to open {}: {:?}", link, err);
                            }
                        }
                        Ok(())
                    }

                    promise::spawn::spawn(config::with_lua_config_on_main_thread(move |lua| {
                        open_uri(lua, window, pane, link.uri().to_string())
                    }))
                    .detach();
                }
            }
            EmitEvent(name) => {
                let window = GuiWin::new(self);
                let pane = PaneObject::new(pane);

                async fn emit_event(
                    lua: Option<Rc<mlua::Lua>>,
                    name: String,
                    window: GuiWin,
                    pane: PaneObject,
                ) -> anyhow::Result<()> {
                    if let Some(lua) = lua {
                        let args = lua.pack_multi((window, pane))?;
                        config::lua::emit_event(&lua, (name.clone(), args))
                            .await
                            .map_err(|e| {
                                log::error!("while processing EmitEvent({}): {:#}", name, e);
                                e
                            })?;
                    }
                    Ok(())
                }

                let name = name.to_string();
                promise::spawn::spawn(config::with_lua_config_on_main_thread(move |lua| {
                    emit_event(lua, name, window, pane)
                }))
                .detach();
            }
            CompleteSelectionOrOpenLinkAtMouseCursor(dest) => {
                let text = self.selection_text(pane);
                if !text.is_empty() {
                    self.copy_to_clipboard(*dest, text);
                    let window = self.window.as_ref().unwrap();
                    window.invalidate();
                } else {
                    return self
                        .perform_key_assignment(pane, &KeyAssignment::OpenLinkAtMouseCursor);
                }
            }
            CompleteSelection(dest) => {
                let text = self.selection_text(pane);
                if !text.is_empty() {
                    self.copy_to_clipboard(*dest, text);
                    let window = self.window.as_ref().unwrap();
                    window.invalidate();
                }
            }
            ClearScrollback(erase_mode) => {
                pane.erase_scrollback(*erase_mode);
                let window = self.window.as_ref().unwrap();
                window.invalidate();
            }
            Search(pattern) => {
                if let Some(pane) = self.get_active_pane_no_overlay() {
                    let search = SearchOverlay::with_pane(self, &pane, pattern.clone());
                    self.assign_overlay_for_pane(pane.pane_id(), search);
                }
            }
            ActivateCopyMode => {
                if let Some(pane) = self.get_active_pane_no_overlay() {
                    let copy = CopyOverlay::with_pane(self, &pane);
                    self.assign_overlay_for_pane(pane.pane_id(), copy);
                }
            }
            AdjustPaneSize(direction, amount) => {
                let mux = Mux::get().unwrap();
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(()),
                };

                let tab_id = tab.tab_id();

                if self.tab_state(tab_id).overlay.is_none() {
                    tab.adjust_pane_size(*direction, *amount);
                }
            }
            ActivatePaneDirection(direction) => {
                let mux = Mux::get().unwrap();
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(()),
                };

                let tab_id = tab.tab_id();

                if self.tab_state(tab_id).overlay.is_none() {
                    tab.activate_pane_direction(*direction);
                }
            }
            TogglePaneZoomState => {
                let mux = Mux::get().unwrap();
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(()),
                };
                tab.toggle_zoom();
            }
        };
        Ok(())
    }

    fn apply_scale_change(&mut self, dimensions: &Dimensions, font_scale: f64) {
        let config = &self.config;
        let font_size = config.font_size * font_scale;
        let theoretical_height = font_size * dimensions.dpi as f64 / 72.0;

        if theoretical_height < 2.0 {
            log::warn!(
                "refusing to go to an unreasonably small font scale {:?}
                       font_scale={} would yield font_height {}",
                dimensions,
                font_scale,
                theoretical_height
            );
            return;
        }

        let (prior_font, prior_dpi) = self
            .fonts
            .change_scaling(font_scale, dimensions.dpi as f64 / ::window::DEFAULT_DPI);
        match RenderMetrics::new(&self.fonts) {
            Ok(metrics) => {
                self.render_metrics = metrics;
            }
            Err(err) => {
                log::error!(
                    "{:#} while attempting to scale font to {} with {:?}",
                    err,
                    font_scale,
                    dimensions
                );
                // Restore prior scaling factors
                self.fonts.change_scaling(prior_font, prior_dpi);
            }
        }
        self.shape_cache.borrow_mut().clear();

        if let Some(render_state) = self.render_state.as_mut() {
            render_state.glyph_cache.borrow_mut().clear();
            render_state
                .recreate_texture_atlas(&self.fonts, &self.render_metrics, None)
                .unwrap();
        }
    }

    fn apply_dimensions(
        &mut self,
        dimensions: &Dimensions,
        mut scale_changed_cells: Option<RowsAndCols>,
    ) {
        let orig_dimensions = self.dimensions;

        self.dimensions = *dimensions;

        // Technically speaking, we should compute the rows and cols
        // from the new dimensions and apply those to the tabs, and
        // then for the scaling changed case, try to re-apply the
        // original rows and cols, but if we do that we end up
        // double resizing the tabs, so we speculatively apply the
        // final size, which in that case should result in a NOP
        // change to the tab size.

        let config = &self.config;

        let (size, dims) = if let Some(cell_dims) = scale_changed_cells {
            // Scaling preserves existing terminal dimensions, yielding a new
            // overall set of window dimensions
            let size = PtySize {
                rows: cell_dims.rows as u16,
                cols: cell_dims.cols as u16,
                pixel_height: cell_dims.rows as u16 * self.render_metrics.cell_size.height as u16,
                pixel_width: cell_dims.cols as u16 * self.render_metrics.cell_size.width as u16,
            };

            let rows = size.rows + if self.show_tab_bar { 1 } else { 0 };
            let cols = size.cols;

            let pixel_height = (rows * self.render_metrics.cell_size.height as u16)
                + (config.window_padding.top + config.window_padding.bottom);

            let pixel_width = (cols * self.render_metrics.cell_size.width as u16)
                + (config.window_padding.left + self.effective_right_padding(&config));

            let dims = Dimensions {
                pixel_width: pixel_width as usize,
                pixel_height: pixel_height as usize,
                dpi: dimensions.dpi,
            };

            (size, dims)
        } else {
            // Resize of the window dimensions may result in changed terminal dimensions
            let avail_width = dimensions.pixel_width.saturating_sub(
                (config.window_padding.left + self.effective_right_padding(&config)) as usize,
            );
            let avail_height = dimensions.pixel_height.saturating_sub(
                (config.window_padding.top + config.window_padding.bottom) as usize,
            );

            let rows = (avail_height / self.render_metrics.cell_size.height as usize)
                .saturating_sub(if self.show_tab_bar { 1 } else { 0 });
            let cols = avail_width / self.render_metrics.cell_size.width as usize;

            let size = PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_height: avail_height as u16,
                pixel_width: avail_width as u16,
            };

            (size, *dimensions)
        };

        if let Some(render_state) = self.render_state.as_mut() {
            if let Err(err) = render_state.advise_of_window_size_change(
                &self.render_metrics,
                dimensions.pixel_width,
                dimensions.pixel_height,
            ) {
                log::error!(
                    "failed to advise of resize from {:?} -> {:?}: {:?}",
                    orig_dimensions,
                    dimensions,
                    err
                );
                // Try to restore the original dimensions
                self.dimensions = orig_dimensions;
                // Avoid the inner resize below
                scale_changed_cells.take();
            } else {
                self.terminal_size = size;
            }
        }

        let mux = Mux::get().unwrap();
        if let Some(window) = mux.get_window(self.mux_window_id) {
            for tab in window.iter() {
                tab.resize(size);
            }
        };
        self.update_title();

        // Queue up a speculative resize in order to preserve the number of rows+cols
        if let Some(cell_dims) = scale_changed_cells {
            if let Some(window) = self.window.as_ref() {
                log::trace!("scale changed so resize to {:?} {:?}", cell_dims, dims);
                window.set_inner_size(dims.pixel_width, dims.pixel_height);
            }
        }
    }

    fn current_cell_dimensions(&self) -> RowsAndCols {
        RowsAndCols {
            rows: self.terminal_size.rows as usize,
            cols: self.terminal_size.cols as usize,
        }
    }

    #[allow(clippy::float_cmp)]
    fn scaling_changed(&mut self, dimensions: Dimensions, font_scale: f64) {
        let scale_changed =
            dimensions.dpi != self.dimensions.dpi || font_scale != self.fonts.get_font_scale();

        let scale_changed_cells = if scale_changed {
            let cell_dims = self.current_cell_dimensions();
            self.apply_scale_change(&dimensions, font_scale);
            Some(cell_dims)
        } else {
            None
        };

        self.apply_dimensions(&dimensions, scale_changed_cells);
    }

    // Used for applying font size changes only; this takes into account
    // the `adjust_window_size_when_changing_font_size` configuration and
    // revises the scaling/resize change accordingly
    fn adjust_font_scale(&mut self, font_scale: f64) {
        if self.config.adjust_window_size_when_changing_font_size {
            self.scaling_changed(self.dimensions, font_scale);
        } else {
            let dimensions = self.dimensions;
            // Compute new font metrics
            self.apply_scale_change(&dimensions, font_scale);
            // Now revise the pty size to fit the window
            self.apply_dimensions(&dimensions, None);
        }
    }

    fn decrease_font_size(&mut self) {
        self.adjust_font_scale(self.fonts.get_font_scale() * 0.9);
    }
    fn increase_font_size(&mut self) {
        self.adjust_font_scale(self.fonts.get_font_scale() * 1.1);
    }
    fn reset_font_size(&mut self) {
        self.adjust_font_scale(1.0);
    }

    fn reset_font_and_window_size(&mut self) -> anyhow::Result<()> {
        let config = &self.config;
        let size = config.initial_size();
        let fontconfig = Rc::new(FontConfiguration::new(Some(config.clone()))?);
        let render_metrics = RenderMetrics::new(&fontconfig)?;

        let terminal_size = PtySize {
            rows: size.rows as u16,
            cols: size.cols as u16,
            pixel_width: (render_metrics.cell_size.width as u16 * size.cols),
            pixel_height: (render_metrics.cell_size.height as u16 * size.rows),
        };

        let show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;

        let rows_with_tab_bar = if show_tab_bar { 1 } else { 0 } + terminal_size.rows;
        let dimensions = Dimensions {
            pixel_width: ((terminal_size.cols * render_metrics.cell_size.width as u16)
                + config.window_padding.left
                + effective_right_padding(&config, &render_metrics))
                as usize,
            pixel_height: ((rows_with_tab_bar * render_metrics.cell_size.height as u16)
                + config.window_padding.top
                + config.window_padding.bottom) as usize,
            dpi: config.dpi.unwrap_or(::window::DEFAULT_DPI) as usize,
        };

        self.apply_scale_change(&dimensions, 1.0);
        self.apply_dimensions(
            &dimensions,
            Some(RowsAndCols {
                rows: size.rows as usize,
                cols: size.cols as usize,
            }),
        );
        Ok(())
    }

    fn close_current_pane(&mut self, confirm: bool) {
        let mux_window_id = self.mux_window_id;
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        let pane = match tab.get_active_pane() {
            Some(p) => p,
            None => return,
        };

        let pane_id = pane.pane_id();
        if confirm && !pane.can_close_without_prompting() {
            let window = self.window.clone().unwrap();
            let (overlay, future) = start_overlay_pane(self, &pane, move |pane_id, term| {
                confirm_close_pane(pane_id, term, mux_window_id, window)
            });
            self.assign_overlay_for_pane(pane_id, overlay);
            promise::spawn::spawn(future).detach();
        } else {
            tab.kill_pane(pane_id);
        }
    }

    fn close_current_tab(&mut self, confirm: bool) {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        let tab_id = tab.tab_id();
        let mux_window_id = self.mux_window_id;
        if confirm && !tab.can_close_without_prompting() {
            let window = self.window.clone().unwrap();
            let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
                confirm_close_tab(tab_id, term, mux_window_id, window)
            });
            self.assign_overlay(tab_id, overlay);
            promise::spawn::spawn(future).detach();
        } else {
            mux.remove_tab(tab_id);
        }
    }

    fn close_tab_idx(&mut self, idx: usize) -> anyhow::Result<()> {
        let mux = Mux::get().unwrap();
        if let Some(mut win) = mux.get_window_mut(self.mux_window_id) {
            let tab = win.remove_by_idx(idx);
            drop(win);
            mux.remove_tab(tab.tab_id());
        }
        self.activate_tab_relative(0)
    }

    fn effective_right_padding(&self, config: &ConfigHandle) -> u16 {
        effective_right_padding(config, &self.render_metrics)
    }

    pub fn pane_state(&self, pane_id: PaneId) -> RefMut<PaneState> {
        RefMut::map(self.pane_state.borrow_mut(), |state| {
            state.entry(pane_id).or_insert_with(PaneState::default)
        })
    }

    pub fn tab_state(&self, tab_id: TabId) -> RefMut<TabState> {
        RefMut::map(self.tab_state.borrow_mut(), |state| {
            state.entry(tab_id).or_insert_with(TabState::default)
        })
    }

    pub fn selection(&self, pane_id: PaneId) -> RefMut<Selection> {
        RefMut::map(self.pane_state(pane_id), |state| &mut state.selection)
    }

    pub fn get_viewport(&self, pane_id: PaneId) -> Option<StableRowIndex> {
        self.pane_state(pane_id).viewport
    }

    pub fn set_viewport(
        &mut self,
        pane_id: PaneId,
        position: Option<StableRowIndex>,
        dims: RenderableDimensions,
    ) {
        let pos = match position {
            Some(pos) => {
                // Drop out of scrolling mode if we're off the bottom
                if pos >= dims.physical_top {
                    None
                } else {
                    Some(pos.max(dims.scrollback_top))
                }
            }
            None => None,
        };

        let mut state = self.pane_state(pane_id);
        if pos != state.viewport {
            state.viewport = pos;

            // This is a bit gross.  If we add other overlays that need this information,
            // this should get extracted out into a trait
            if let Some(overlay) = state.overlay.as_ref() {
                if let Some(search_overlay) = overlay.downcast_ref::<SearchOverlay>() {
                    search_overlay.viewport_changed(pos);
                } else if let Some(copy) = overlay.downcast_ref::<CopyOverlay>() {
                    copy.viewport_changed(pos);
                }
            }
            self.window.as_ref().unwrap().invalidate();
        }
    }

    fn extend_selection_at_mouse_cursor(
        &mut self,
        mode: Option<SelectionMode>,
        pane: &Rc<dyn Pane>,
    ) {
        let mode = mode.unwrap_or(SelectionMode::Cell);
        let (x, y) = self.last_mouse_terminal_coords;
        match mode {
            SelectionMode::Cell => {
                let end = SelectionCoordinate { x, y };
                let selection_range = self.selection(pane.pane_id()).range.take();
                let sel = match selection_range {
                    None => {
                        SelectionRange::start(self.selection(pane.pane_id()).start.unwrap_or(end))
                            .extend(end)
                    }
                    Some(sel) => sel.extend(end),
                };
                self.selection(pane.pane_id()).range = Some(sel);
            }
            SelectionMode::Word => {
                let end_word = SelectionRange::word_around(SelectionCoordinate { x, y }, &**pane);

                let start_coord = self
                    .selection(pane.pane_id())
                    .start
                    .clone()
                    .unwrap_or(end_word.start);
                let start_word = SelectionRange::word_around(start_coord, &**pane);

                let selection_range = start_word.extend_with(end_word);
                self.selection(pane.pane_id()).range = Some(selection_range);
            }
            SelectionMode::Line => {
                let end_line = SelectionRange::line_around(SelectionCoordinate { x, y });

                let start_coord = self
                    .selection(pane.pane_id())
                    .start
                    .clone()
                    .unwrap_or(end_line.start);
                let start_line = SelectionRange::line_around(start_coord);

                let selection_range = start_line.extend_with(end_line);
                self.selection(pane.pane_id()).range = Some(selection_range);
            }
            SelectionMode::SemanticZone => {
                let end_word = SelectionRange::zone_around(SelectionCoordinate { x, y }, &**pane);

                let start_coord = self
                    .selection(pane.pane_id())
                    .start
                    .clone()
                    .unwrap_or(end_word.start);
                let start_word = SelectionRange::zone_around(start_coord, &**pane);

                let selection_range = start_word.extend_with(end_word);
                self.selection(pane.pane_id()).range = Some(selection_range);
            }
        }

        // When the mouse gets close enough to the top or bottom then scroll
        // the viewport so that we can see more in that direction and are able
        // to select more than fits in the viewport.

        // This is similar to the logic in the copy mode overlay, but the gap
        // is smaller because it feels more natural for mouse selection to have
        // a smaller gpa.
        const VERTICAL_GAP: isize = 2;
        let dims = pane.get_dimensions();
        let top = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top);
        let vertical_gap = if dims.physical_top <= VERTICAL_GAP {
            1
        } else {
            VERTICAL_GAP
        };
        let top_gap = y - top;
        if top_gap < vertical_gap {
            // Increase the gap so we can "look ahead"
            self.set_viewport(pane.pane_id(), Some(y.saturating_sub(vertical_gap)), dims);
        } else {
            let bottom_gap = (dims.viewport_rows as isize).saturating_sub(top_gap);
            if bottom_gap < vertical_gap {
                self.set_viewport(pane.pane_id(), Some(top + vertical_gap - bottom_gap), dims);
            }
        }

        self.window.as_ref().unwrap().invalidate();
    }

    fn select_text_at_mouse_cursor(&mut self, mode: SelectionMode, pane: &Rc<dyn Pane>) {
        let (x, y) = self.last_mouse_terminal_coords;
        match mode {
            SelectionMode::Line => {
                let start = SelectionCoordinate { x, y };
                let selection_range = SelectionRange::line_around(start);

                self.selection(pane.pane_id()).start = Some(start);
                self.selection(pane.pane_id()).range = Some(selection_range);
            }
            SelectionMode::Word => {
                let selection_range =
                    SelectionRange::word_around(SelectionCoordinate { x, y }, &**pane);

                self.selection(pane.pane_id()).start = Some(selection_range.start);
                self.selection(pane.pane_id()).range = Some(selection_range);
            }
            SelectionMode::SemanticZone => {
                let selection_range =
                    SelectionRange::zone_around(SelectionCoordinate { x, y }, &**pane);

                self.selection(pane.pane_id()).start = Some(selection_range.start);
                self.selection(pane.pane_id()).range = Some(selection_range);
            }
            SelectionMode::Cell => {
                self.selection(pane.pane_id())
                    .begin(SelectionCoordinate { x, y });
            }
        }

        self.window.as_ref().unwrap().invalidate();
    }

    fn maybe_scroll_to_bottom_for_input(&mut self, pane: &Rc<dyn Pane>) {
        if self.config.scroll_to_bottom_on_input {
            self.scroll_to_bottom(pane);
        }
    }

    fn scroll_to_bottom(&mut self, pane: &Rc<dyn Pane>) {
        self.pane_state(pane.pane_id()).viewport = None;
    }

    fn get_active_pane_no_overlay(&self) -> Option<Rc<dyn Pane>> {
        let mux = Mux::get().unwrap();
        mux.get_active_tab_for_window(self.mux_window_id)
            .and_then(|tab| tab.get_active_pane())
    }

    /// Returns a Pane that we can interact with; this will typically be
    /// the active tab for the window, but if the window has a tab-wide
    /// overlay (such as the launcher / tab navigator),
    /// then that will be returned instead.  Otherwise, if the pane has
    /// an active overlay (such as search or copy mode) then that will
    /// be returned.
    fn get_active_pane_or_overlay(&self) -> Option<Rc<dyn Pane>> {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return None,
        };

        let tab_id = tab.tab_id();

        if let Some(tab_overlay) = self.tab_state(tab_id).overlay.clone() {
            Some(tab_overlay)
        } else {
            let pane = tab.get_active_pane()?;
            let pane_id = pane.pane_id();
            self.pane_state(pane_id)
                .overlay
                .clone()
                .or_else(|| Some(pane))
        }
    }

    fn get_splits(&mut self) -> Vec<PositionedSplit> {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return vec![],
        };

        let tab_id = tab.tab_id();

        if let Some(_) = self.tab_state(tab_id).overlay.clone() {
            vec![]
        } else {
            tab.iter_splits()
        }
    }

    fn get_panes_to_render(&mut self) -> Vec<PositionedPane> {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return vec![],
        };

        let tab_id = tab.tab_id();

        if let Some(pane) = self.tab_state(tab_id).overlay.clone() {
            let size = tab.get_size();
            vec![PositionedPane {
                index: 0,
                is_active: true,
                is_zoomed: false,
                left: 0,
                top: 0,
                width: size.cols as _,
                height: size.rows as _,
                pixel_width: size.cols as usize * self.render_metrics.cell_size.width as usize,
                pixel_height: size.rows as usize * self.render_metrics.cell_size.height as usize,
                pane,
            }]
        } else {
            let mut panes = tab.iter_panes();
            for p in &mut panes {
                if let Some(overlay) = self.pane_state(p.pane.pane_id()).overlay.as_ref() {
                    p.pane = Rc::clone(overlay);
                }
            }
            panes
        }
    }

    /// if pane_id.is_none(), removes any overlay for the specified tab.
    /// Otherwise: if the overlay is the specified pane for that tab, remove it.
    fn cancel_overlay_for_tab(&self, tab_id: TabId, pane_id: Option<PaneId>) {
        if pane_id.is_some() {
            let current = self.tab_state(tab_id).overlay.as_ref().map(|o| o.pane_id());
            if current != pane_id {
                return;
            }
        }
        if let Some(pane) = self.tab_state(tab_id).overlay.take() {
            Mux::get().unwrap().remove_pane(pane.pane_id());
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn schedule_cancel_overlay(window: Window, tab_id: TabId, pane_id: Option<PaneId>) {
        window.apply(move |myself, _| {
            if let Some(myself) = myself.downcast_mut::<Self>() {
                myself.cancel_overlay_for_tab(tab_id, pane_id);
            }
            Ok(())
        });
    }

    fn cancel_overlay_for_pane(&self, pane_id: PaneId) {
        if let Some(pane) = self.pane_state(pane_id).overlay.take() {
            Mux::get().unwrap().remove_pane(pane.pane_id());
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn schedule_cancel_overlay_for_pane(window: Window, pane_id: PaneId) {
        window.apply(move |myself, _| {
            if let Some(myself) = myself.downcast_mut::<Self>() {
                myself.cancel_overlay_for_pane(pane_id);
            }
            Ok(())
        });
    }

    pub fn assign_overlay_for_pane(&mut self, pane_id: PaneId, overlay: Rc<dyn Pane>) {
        if let Some(prior) = self.pane_state(pane_id).overlay.replace(overlay) {
            Mux::get().unwrap().remove_pane(prior.pane_id());
        }
        self.update_title();
    }

    pub fn assign_overlay(&mut self, tab_id: TabId, overlay: Rc<dyn Pane>) {
        if let Some(prior) = self.tab_state(tab_id).overlay.replace(overlay) {
            Mux::get().unwrap().remove_pane(prior.pane_id());
        }
        self.update_title();
    }
}
