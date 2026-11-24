//! Color theme. Later stages load this from TOML; for now it is a built-in dark theme.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub const fn hex(v: u32) -> Self {
        Self::new((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    pub fn scale(self, k: f32) -> Self {
        let f = |c: u8| (c as f32 * k).round().clamp(0.0, 255.0) as u8;
        Self::new(f(self.r), f(self.g), f(self.b))
    }

    /// Linear-space RGBA for the sRGB render target.
    pub fn to_linear(self) -> [f32; 4] {
        fn lin(c: u8) -> f32 {
            let c = c as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        [lin(self.r), lin(self.g), lin(self.b), 1.0]
    }
}

impl From<alacritty_terminal::vte::ansi::Rgb> for Rgb {
    fn from(c: alacritty_terminal::vte::ansi::Rgb) -> Self {
        Self::new(c.r, c.g, c.b)
    }
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub selection_bg: Rgb,
    /// 0-7 normal, 8-15 bright.
    pub ansi: [Rgb; 16],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: Rgb::hex(0xd8dee9),
            background: Rgb::hex(0x16181d),
            cursor: Rgb::hex(0xe5a46a),
            selection_bg: Rgb::hex(0x3a4250),
            ansi: [
                Rgb::hex(0x2a2e36),
                Rgb::hex(0xe06c75),
                Rgb::hex(0x98c379),
                Rgb::hex(0xe5c07b),
                Rgb::hex(0x61afef),
                Rgb::hex(0xc678dd),
                Rgb::hex(0x56b6c2),
                Rgb::hex(0xc8ccd4),
                Rgb::hex(0x5c6370),
                Rgb::hex(0xf0838c),
                Rgb::hex(0xb5e090),
                Rgb::hex(0xf2d38f),
                Rgb::hex(0x80c4ff),
                Rgb::hex(0xdc9cf0),
                Rgb::hex(0x76d2dc),
                Rgb::hex(0xffffff),
            ],
        }
    }
}

impl Theme {
    /// Standard xterm 256-color palette, with the first 16 entries taken from the theme.
    pub fn palette256(&self) -> [Rgb; 256] {
        let mut p = [Rgb::default(); 256];
        p[..16].copy_from_slice(&self.ansi);
        let steps = [0u8, 95, 135, 175, 215, 255];
        for i in 0..216 {
            p[16 + i] = Rgb::new(steps[i / 36], steps[(i / 6) % 6], steps[i % 6]);
        }
        for i in 0..24 {
            let v = 8 + 10 * i as u8;
            p[232 + i] = Rgb::new(v, v, v);
        }
        p
    }
}
