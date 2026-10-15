//! Offscreen frame capture: draw the dashboard without a compositor.
//!
//! The window is a real Slint window with Slint's software renderer, so what comes
//! out is what the UI code asks for, at the exact 1920x1080 the layout is written
//! against — but it needs no display server, which makes it the honest way to
//! check the grid on a headless machine (and the only way when the desktop session
//! is locked and screen capture is refused).

use std::io::Write;
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{PhysicalSize, Rgb8Pixel};

pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;

struct ShotPlatform(Rc<MinimalSoftwareWindow>);

impl Platform for ShotPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

/// Install the offscreen platform. Must run before the first window is created.
pub fn install() -> Result<Rc<MinimalSoftwareWindow>, Box<dyn std::error::Error>> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(ShotPlatform(window.clone())))?;
    window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
    Ok(window)
}

/// Render the window's current contents to a binary PPM (P6) at `path`.
///
/// PPM keeps this dependency-free; `magick frame.ppm frame.png` converts it.
pub fn capture(
    window: &MinimalSoftwareWindow,
    path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    window.request_redraw();
    let mut buffer = vec![Rgb8Pixel::default(); (WIDTH * HEIGHT) as usize];
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, WIDTH as usize);
    });

    let mut file = std::fs::File::create(path)?;
    write!(file, "P6\n{WIDTH} {HEIGHT}\n255\n")?;
    let mut bytes = Vec::with_capacity(buffer.len() * 3);
    for pixel in &buffer {
        bytes.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
    }
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(())
}
