use crate::input::{Event, Inputs};
use anyhow::{Context, Result};
use skyward_ui::app::Touch;
use skyward_ui::surface::kms::KmsSurface;
use skyward_ui::{Canvas, Rgba, Surface};
use std::time::{Duration, Instant};

/// Exercise scanout and touch before entering recovery's partition and service setup.
pub fn run(card: &str, seconds: u16) -> Result<()> {
    report("display probe starting");
    let result = exercise(card, seconds);
    if let Err(error) = &result {
        report(&format!("display probe failed: {error:#}"));
    }
    result
}

fn exercise(card: &str, seconds: u16) -> Result<()> {
    let mut display = KmsSurface::open(card).context("open display for probe")?;
    let (width, height) = display.size();
    report(&format!("display: {}", display.describe()));
    let mut canvas = Canvas::new(width, height);
    draw(&mut display, &mut canvas, None)?;
    let mut inputs = Inputs::open(width, height, Vec::new())?;
    report(&format!("first frame presented; initial touch device: {}", inputs.has_panel()));

    let deadline = Instant::now() + Duration::from_secs(seconds.into());
    let mut last = None;
    let mut touches = 0;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let mut poll = libc::pollfd { fd: inputs.fd(), events: libc::POLLIN, revents: 0 };
        let millis = remaining.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(&mut poll, 1, millis) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("wait for touch");
        }
        if ready == 0 {
            break;
        }
        let mut position = last;
        for event in inputs.read()? {
            match event {
                Event::Touch(Touch::Down(x, y), _) => {
                    touches += 1;
                    report(&format!("touch {touches}: {x:.0}, {y:.0}"));
                    position = Some((x, y));
                }
                Event::Touch(Touch::Move(x, y), _) => position = Some((x, y)),
                _ => {}
            }
        }
        if last != position {
            draw(&mut display, &mut canvas, position)?;
            last = position;
        }
    }
    report(&format!("probe finished: {touches} touch presses"));
    Ok(())
}

fn report(message: &str) {
    eprintln!("{message}");
    let _ = skyward_sys::kmsg(message);
}

fn draw(display: &mut KmsSurface, canvas: &mut Canvas, touch: Option<(f32, f32)>) -> Result<()> {
    let w = canvas.w as f32;
    let h = canvas.h as f32;
    canvas.clear(Rgba::rgb(24, 28, 38));
    for (i, color) in [Rgba::rgb(238, 62, 71), Rgba::rgb(58, 200, 114), Rgba::rgb(53, 123, 241)].into_iter().enumerate() {
        canvas.fill_rect(w * (i as f32 / 3.0), h * 0.17, w / 3.0, h * 0.25, color);
    }
    if let Some((x, y)) = touch {
        canvas.fill_circle(x, y, w * 0.045, Rgba::rgb(255, 255, 255));
    }
    display.present(canvas)?;
    display.settle()
}
