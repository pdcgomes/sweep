//! The look: Omarchy's default theme, Tokyo Night, for the interface and for
//! the plain-text output alike.

use ratatui::style::Color;
use std::io::IsTerminal;

pub mod hex {
    pub const BG: u32 = 0x1a1b26;
    pub const SURFACE: u32 = 0x24283b;
    pub const SELECTION: u32 = 0x283457;
    pub const FG: u32 = 0xc0caf5;
    pub const TEXT: u32 = 0xa9b1d6;
    pub const MUTED: u32 = 0x737aa2;
    pub const COMMENT: u32 = 0x565f89;
    pub const GUTTER: u32 = 0x3b4261;
    pub const BLUE: u32 = 0x7aa2f7;
    pub const CYAN: u32 = 0x7dcfff;
    pub const PURPLE: u32 = 0xbb9af7;
    pub const GREEN: u32 = 0x9ece6a;
    pub const YELLOW: u32 = 0xe0af68;
    pub const ORANGE: u32 = 0xff9e64;
    pub const RED: u32 = 0xf7768e;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub bg: Color,
    pub surface: Color,
    pub selection: Color,
    pub fg: Color,
    pub text: Color,
    pub muted: Color,
    pub comment: Color,
    pub gutter: Color,
    pub blue: Color,
    pub cyan: Color,
    pub purple: Color,
    pub green: Color,
    pub yellow: Color,
    pub orange: Color,
    pub red: Color,
}

impl Theme {
    pub fn tokyo_night() -> Theme {
        Theme::with(truecolor())
    }

    pub fn with(truecolor: bool) -> Theme {
        let c = |h: u32| color(h, truecolor);
        Theme {
            bg: c(hex::BG),
            surface: c(hex::SURFACE),
            selection: c(hex::SELECTION),
            fg: c(hex::FG),
            text: c(hex::TEXT),
            muted: c(hex::MUTED),
            comment: c(hex::COMMENT),
            gutter: c(hex::GUTTER),
            blue: c(hex::BLUE),
            cyan: c(hex::CYAN),
            purple: c(hex::PURPLE),
            green: c(hex::GREEN),
            yellow: c(hex::YELLOW),
            orange: c(hex::ORANGE),
            red: c(hex::RED),
        }
    }
}

pub fn color(h: u32, truecolor: bool) -> Color {
    let (r, g, b) = ((h >> 16) as u8, (h >> 8) as u8, h as u8);
    if truecolor { Color::Rgb(r, g, b) } else { Color::Indexed(nearest_256(r, g, b)) }
}

/// Whether the terminal takes 24-bit colour; otherwise the theme falls back
/// to the nearest of the 256 standard colours.
pub fn truecolor() -> bool {
    let var = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
    let ct = var("COLORTERM");
    if ct.contains("truecolor") || ct.contains("24bit") {
        return true;
    }
    let program = var("TERM_PROGRAM");
    if ["iterm.app", "ghostty", "wezterm", "vscode", "hyper", "tabby", "rio", "warpterminal"]
        .contains(&program.as_str())
    {
        return true;
    }
    let term = var("TERM");
    if ["kitty", "alacritty", "ghostty", "wezterm", "foot"].iter().any(|t| term.contains(t)) {
        return true;
    }
    // Terminal gained 24-bit colour in macOS 26.
    program == "apple_terminal" && macos_major() >= 26
}

fn macos_major() -> u32 {
    let name = c"kern.osproductversion";
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    let rc = unsafe {
        libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, &mut len, std::ptr::null_mut(), 0)
    };
    if rc != 0 {
        return 0;
    }
    let s = String::from_utf8_lossy(&buf[..len.saturating_sub(1)]).into_owned();
    s.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(0)
}

/// The closest colour in xterm's 6×6×6 cube or its grey ramp.
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| LEVELS.iter().enumerate().min_by_key(|(_, l)| (**l as i32 - v as i32).abs()).unwrap().0;
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let cube_rgb = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let avg = (r as u32 + g as u32 + b as u32) / 3;
    let grey_i = ((avg as i32 - 8).max(0) / 10).min(23) as u8;
    let grey = 8 + 10 * grey_i;
    let dist = |(a, b2, c): (u8, u8, u8)| {
        let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2);
        d(a, r) + d(b2, g) + d(c, b)
    };
    if dist((grey, grey, grey)) < dist(cube_rgb) { 232 + grey_i } else { cube as u8 }
}

/// `--help` in the same colours.
pub fn help_styles() -> clap::builder::Styles {
    use clap::builder::styling::{Ansi256Color, Color as C, RgbColor, Style as S};
    let tc = truecolor();
    let c = |h: u32| -> Option<C> {
        let (r, g, b) = ((h >> 16) as u8, (h >> 8) as u8, h as u8);
        Some(if tc { C::Rgb(RgbColor(r, g, b)) } else { C::Ansi256(Ansi256Color(nearest_256(r, g, b))) })
    };
    clap::builder::Styles::styled()
        .header(S::new().fg_color(c(hex::BLUE)).bold())
        .usage(S::new().fg_color(c(hex::BLUE)).bold())
        .literal(S::new().fg_color(c(hex::CYAN)))
        .placeholder(S::new().fg_color(c(hex::PURPLE)))
        .valid(S::new().fg_color(c(hex::GREEN)))
        .invalid(S::new().fg_color(c(hex::YELLOW)))
        .error(S::new().fg_color(c(hex::RED)).bold())
}

/// Colours for plain-text output: on for a terminal, off when piped or
/// when NO_COLOR is set.
#[derive(Clone, Copy, Debug)]
pub struct Paint {
    on: bool,
    truecolor: bool,
}

impl Paint {
    pub fn stdout() -> Paint {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Paint { on: std::io::stdout().is_terminal() && !no_color, truecolor: truecolor() }
    }

    #[cfg(test)]
    pub fn off() -> Paint {
        Paint { on: false, truecolor: false }
    }

    pub fn on(&self) -> bool {
        self.on
    }

    pub fn fg(&self, h: u32, text: impl AsRef<str>) -> String {
        let text = text.as_ref();
        if !self.on || text.is_empty() {
            return text.to_string();
        }
        let code = match color(h, self.truecolor) {
            Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
            Color::Indexed(i) => format!("38;5;{i}"),
            _ => "39".into(),
        };
        format!("\x1b[{code}m{text}\x1b[39m")
    }

    pub fn bold(&self, text: impl AsRef<str>) -> String {
        if self.on { format!("\x1b[1m{}\x1b[22m", text.as_ref()) } else { text.as_ref().to_string() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_colours() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert_eq!(nearest_256(0x1a, 0x1b, 0x26), 234, "Tokyo Night's background is a near-black grey");
        assert_eq!(nearest_256(0x7a, 0xa2, 0xf7), 111, "and its blue a soft blue");
    }

    #[test]
    fn plain_when_off() {
        let p = Paint::off();
        assert_eq!(p.fg(hex::GREEN, "Safe"), "Safe");
        let on = Paint { on: true, truecolor: true };
        assert_eq!(on.fg(hex::GREEN, "Safe"), "\x1b[38;2;158;206;106mSafe\x1b[39m");
        let on256 = Paint { on: true, truecolor: false };
        assert!(on256.fg(hex::GREEN, "Safe").starts_with("\x1b[38;5;"));
    }
}
