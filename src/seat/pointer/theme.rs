use std::{
    cell::RefCell,
    ops::Deref,
    rc::{Rc, Weak},
};
use wayland_client::{
    protocol::{wl_compositor, wl_pointer, wl_seat, wl_shm, wl_surface},
    Attached, DispatchData,
};
use wayland_cursor::{is_available, load_theme, Cursor, CursorTheme};

/// Wrapper managing a system theme for pointer images
///
/// You can use it to initialize new pointers in order
/// to theme them.
///
/// Is is also clone-able in case you need to handle several
/// pointer theming from different places.
///
/// Note that it is however not `Send` nor `Sync`
pub struct ThemeManager {
    themes: Rc<RefCell<ScaledThemeList>>,
    compositor: Attached<wl_compositor::WlCompositor>,
}

impl ThemeManager {
    /// Load a system pointer theme
    ///
    /// Will use the default theme of the system if name is `None`.
    ///
    /// Fails if `libwayland-cursor` is not available.
    pub fn init(
        name: Option<&str>,
        compositor: Attached<wl_compositor::WlCompositor>,
        shm: Attached<wl_shm::WlShm>,
    ) -> Result<ThemeManager, ()> {
        if !is_available() {
            return Err(());
        }

        Ok(ThemeManager {
            compositor,
            themes: Rc::new(RefCell::new(ScaledThemeList::new(
                name.map(Into::into),
                shm,
            ))),
        })
    }

    /// Wrap a pointer to theme it
    pub fn theme_pointer(&self, pointer: wl_pointer::WlPointer) -> ThemedPointer {
        let surface = self.compositor.create_surface();
        let inner = Rc::new(RefCell::new(PointerInner {
            surface: (**surface).clone(),
            themes: self.themes.clone(),
            last_serial: 0,
            current_cursor: "left_ptr".into(),
            scale_factor: 1,
        }));
        let my_pointer = pointer.clone();
        let winner = Rc::downgrade(&inner);
        crate::surface::setup_surface(
            surface,
            Some(move |scale_factor, _, _: DispatchData| {
                if let Some(inner) = Weak::upgrade(&winner) {
                    let mut inner = inner.borrow_mut();
                    inner.scale_factor = scale_factor;
                    // we can't handle errors here, so ignore it
                    // worst that can happen is cursor drawn with the wrong
                    // scale factor
                    let _ = inner.update_cursor(&my_pointer);
                }
            }),
        );
        ThemedPointer { pointer, inner }
    }

    /// Initialize a new pointer as a ThemedPointer with an adapter implementation
    ///
    /// You need to provide an implementation as if implementing a `wl_pointer`, but
    /// it will receive as `meta` argument a `ThemedPointer` wrapping your pointer,
    /// rather than a `WlPointer`.
    pub fn theme_pointer_with_impl<F>(
        &self,
        seat: &Attached<wl_seat::WlSeat>,
        mut callback: F,
    ) -> ThemedPointer
    where
        F: FnMut(wl_pointer::Event, ThemedPointer, DispatchData) + 'static,
    {
        let surface = self.compositor.create_surface();
        surface.quick_assign(|_, _, _| {});

        let inner = Rc::new(RefCell::new(PointerInner {
            surface: (*surface).clone().detach(),
            themes: self.themes.clone(),
            last_serial: 0,
            current_cursor: "left_ptr".into(),
            scale_factor: 1,
        }));
        let inner2 = inner.clone();

        let pointer = seat.get_pointer();
        pointer.quick_assign(move |ptr, event, ddata| {
            callback(
                event,
                ThemedPointer {
                    pointer: (*ptr).clone().detach(),
                    inner: inner.clone(),
                },
                ddata,
            )
        });

        ThemedPointer {
            pointer: (*pointer).clone().detach(),
            inner: inner2,
        }
    }
}

struct ScaledThemeList {
    shm: Attached<wl_shm::WlShm>,
    name: Option<String>,
    themes: Vec<(u32, CursorTheme)>,
}

impl ScaledThemeList {
    fn new(name: Option<String>, shm: Attached<wl_shm::WlShm>) -> ScaledThemeList {
        ScaledThemeList {
            shm,
            name,
            themes: vec![],
        }
    }

    fn get_cursor<'a>(&'a mut self, name: &str, scale: u32) -> Option<Cursor<'a>> {
        // Check if we already loaded the theme for this scale factor
        let opt_index = self.themes.iter().position(|&(s, _)| s == scale);
        if let Some(idx) = opt_index {
            self.themes[idx].1.get_cursor(name)
        } else {
            let new_theme = load_theme(self.name.as_ref().map(|s| &s[..]), 16 * scale, &self.shm);
            self.themes.push((scale, new_theme));
            self.themes.last().unwrap().1.get_cursor(name)
        }
    }
}

struct PointerInner {
    surface: wl_surface::WlSurface,
    themes: Rc<RefCell<ScaledThemeList>>,
    current_cursor: String,
    last_serial: u32,
    scale_factor: i32,
}

impl PointerInner {
    fn update_cursor(&self, pointer: &wl_pointer::WlPointer) -> Result<(), ()> {
        let mut themes = self.themes.borrow_mut();
        let cursor = themes
            .get_cursor(&self.current_cursor, self.scale_factor as u32)
            .ok_or(())?;
        let buffer = cursor.frame_buffer(0).ok_or(())?;
        let (w, h, hx, hy) = cursor
            .frame_info(0)
            .map(|(w, h, hx, hy, _)| (w as i32, h as i32, hx as i32, hy as i32))
            .unwrap_or((0, 0, 0, 0));

        self.surface.set_buffer_scale(self.scale_factor);
        self.surface.attach(Some(&buffer), 0, 0);
        if self.surface.as_ref().version() >= 4 {
            self.surface.damage_buffer(0, 0, w, h);
        } else {
            // surface is old and does not support damage_buffer, so we damage
            // in surface coordinates and hope it is not rescaled
            self.surface.damage(0, 0, w, h);
        }
        self.surface.commit();
        pointer.set_cursor(self.last_serial, Some(&self.surface), hx, hy);
        Ok(())
    }
}

/// Wrapper of a themed pointer
///
/// You can access the underlying `wl_pointer::WlPointer` via
/// deref. It will *not* release the proxy when dropped.
///
/// Just like `Proxy`, this is a `Rc`-like wrapper. You can clone it
/// to have several handles to the same theming machinery of a pointer.
pub struct ThemedPointer {
    pointer: wl_pointer::WlPointer,
    inner: Rc<RefCell<PointerInner>>,
}

// load_theme(name, 16, &shm)

impl ThemedPointer {
    /// Change the cursor to the given cursor name
    ///
    /// Possible names depend on the theme. Does nothing and returns
    /// `Err(())` if given name is not available.
    ///
    /// If this is done as an answer to an input event, you need to provide
    /// the associated serial otherwise the server may ignore the request.
    pub fn set_cursor(&self, name: &str, serial: Option<u32>) -> Result<(), ()> {
        let mut inner = self.inner.borrow_mut();
        if let Some(s) = serial {
            inner.last_serial = s;
        }
        inner.current_cursor = name.into();
        inner.update_cursor(&self.pointer)
    }
}

impl Clone for ThemedPointer {
    fn clone(&self) -> ThemedPointer {
        ThemedPointer {
            pointer: self.pointer.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl Deref for ThemedPointer {
    type Target = wl_pointer::WlPointer;
    fn deref(&self) -> &wl_pointer::WlPointer {
        &self.pointer
    }
}

impl Drop for PointerInner {
    fn drop(&mut self) {
        self.surface.destroy();
    }
}
