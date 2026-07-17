use ratatui::style::Color;

const QUERY_TIMEOUT_MS: u64 = 100;
const TINT_PERCENT: u16 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

#[derive(Clone, Debug, Default)]
pub struct TerminalPalette {
    background: Option<Rgb>,
    ansi: [Option<Rgb>; 16],
}

impl TerminalPalette {
    pub fn probe() -> Self {
        let Some(background) = query_default_background() else {
            return Self::default();
        };

        let mut ansi = [None; 16];
        for index in [2, 3, 5, 6] {
            ansi[index] = query_ansi_color(index as u8);
        }
        Self {
            background: Some(background),
            ansi,
        }
    }

    pub fn tinted_ansi(&self, index: u8) -> Color {
        self.background
            .zip(self.ansi.get(index as usize).copied().flatten())
            .map(|(background, accent)| blend(background, accent, TINT_PERCENT))
            .map_or(Color::Reset, |color| Color::Rgb(color.r, color.g, color.b))
    }
}

fn query_default_background() -> Option<Rgb> {
    let response = xterm_query::query_osc("\x1b]11;?\x1b\\", QUERY_TIMEOUT_MS).ok()?;
    parse_osc_rgb(&response, "]11;")
}

fn query_ansi_color(index: u8) -> Option<Rgb> {
    let query = format!("\x1b]4;{index};?\x1b\\");
    let response = xterm_query::query_osc(&query, QUERY_TIMEOUT_MS).ok()?;
    parse_osc_rgb(&response, &format!("]4;{index};"))
}

fn parse_osc_rgb(response: &str, prefix: &str) -> Option<Rgb> {
    let value = response
        .trim_end_matches(['\x07', '\x1b'])
        .strip_prefix(prefix)?
        .strip_prefix("rgb:")?;
    let mut channels = value.split('/');
    let color = Rgb {
        r: parse_xterm_channel(channels.next()?)?,
        g: parse_xterm_channel(channels.next()?)?,
        b: parse_xterm_channel(channels.next()?)?,
    };
    (channels.next().is_none()).then_some(color)
}

fn parse_xterm_channel(channel: &str) -> Option<u8> {
    if channel.is_empty() || channel.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(channel, 16).ok()?;
    let maximum = (1_u32 << (channel.len() * 4)) - 1;
    Some(((value * 255 + maximum / 2) / maximum) as u8)
}

fn blend(background: Rgb, accent: Rgb, accent_percent: u16) -> Rgb {
    let mix = |base: u8, overlay: u8| {
        let base_percent = 100 - accent_percent;
        ((u16::from(base) * base_percent + u16::from(overlay) * accent_percent + 50) / 100) as u8
    };
    Rgb {
        r: mix(background.r, accent.r),
        g: mix(background.g, accent.g),
        b: mix(background.b, accent.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xterm_16_bit_and_8_bit_rgb_responses() {
        assert_eq!(
            parse_osc_rgb("]11;rgb:ffff/8080/0000\x1b", "]11;"),
            Some(Rgb {
                r: 255,
                g: 128,
                b: 0
            })
        );
        assert_eq!(
            parse_osc_rgb("]4;6;rgb:00/80/ff\x07", "]4;6;"),
            Some(Rgb {
                r: 0,
                g: 128,
                b: 255
            })
        );
    }

    #[test]
    fn rejects_malformed_or_wrong_slot_responses() {
        assert_eq!(parse_osc_rgb("]4;5;rgb:00/80/ff\x07", "]4;6;"), None);
        assert_eq!(parse_osc_rgb("]4;6;rgb:00/80\x07", "]4;6;"), None);
        assert_eq!(parse_osc_rgb("]4;6;none\x07", "]4;6;"), None);
    }

    #[test]
    fn derives_a_subtle_tint_from_terminal_background_and_ansi_color() {
        let mut palette = TerminalPalette {
            background: Some(Rgb {
                r: 255,
                g: 255,
                b: 255,
            }),
            ..TerminalPalette::default()
        };
        palette.ansi[6] = Some(Rgb {
            r: 0,
            g: 128,
            b: 255,
        });

        assert_eq!(palette.tinted_ansi(6), Color::Rgb(204, 230, 255));
    }

    #[test]
    fn missing_terminal_response_falls_back_to_default_background() {
        assert_eq!(TerminalPalette::default().tinted_ansi(6), Color::Reset);
    }
}
