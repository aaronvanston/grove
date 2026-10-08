//! Human-mode styling: when color is on, the ANSI codes picocolors used,
//! the symbols with their ASCII fallbacks, tables and meters.

use std::io::IsTerminal;

use crate::output::{ColorMode, Globals, Mode};

pub struct Symbols {
    pub error: &'static str,
    pub success: &'static str,
    pub warning: &'static str,
}

pub struct Ui {
    pub color: bool,
    pub unicode: bool,
    pub symbols: Symbols,
}

fn env_is(name: &str, value: &str) -> bool {
    std::env::var_os(name).is_some_and(|actual| actual == value)
}

impl Ui {
    /// Color follows --color, then NO_COLOR, FORCE_COLOR=0 and TERM=dumb,
    /// then whether stderr is a terminal. Machine modes never color.
    pub fn new(globals: &Globals) -> Self {
        let color = match globals.color {
            _ if globals.mode != Mode::Human => false,
            ColorMode::Never => false,
            ColorMode::Always => true,
            ColorMode::Auto => {
                std::env::var_os("NO_COLOR").is_none()
                    && !env_is("FORCE_COLOR", "0")
                    && !env_is("TERM", "dumb")
                    && std::io::stderr().is_terminal()
            }
        };
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .collect::<String>();
        let lower = locale.to_lowercase();
        let unicode = !env_is("TERM", "dumb")
            && (locale.is_empty() || lower.contains("utf-8") || lower.contains("utf8"));
        let symbols = if unicode {
            Symbols {
                error: "✗",
                success: "✓",
                warning: "!",
            }
        } else {
            Symbols {
                error: "[error]",
                success: "[ok]",
                warning: "[warn]",
            }
        };
        Self {
            color,
            unicode,
            symbols,
        }
    }

    /// picocolors' wrapper: a close code inside the text re-opens the style
    /// so nesting survives.
    fn paint(&self, open: &str, close: &str, value: &str) -> String {
        if !self.color {
            return value.to_owned();
        }
        let inner = value.replace(close, &format!("{close}{open}"));
        format!("{open}{inner}{close}")
    }

    pub fn brand(&self, value: &str) -> String {
        self.paint("\x1b[35m", "\x1b[39m", value)
    }
    pub fn command(&self, value: &str) -> String {
        self.paint("\x1b[1m", "\x1b[22m", value)
    }
    pub fn danger(&self, value: &str) -> String {
        self.paint("\x1b[31m", "\x1b[39m", value)
    }
    pub fn heading(&self, value: &str) -> String {
        self.paint("\x1b[1m", "\x1b[22m", value)
    }
    pub fn muted(&self, value: &str) -> String {
        self.paint("\x1b[2m", "\x1b[22m", value)
    }
    pub fn success(&self, value: &str) -> String {
        self.paint("\x1b[32m", "\x1b[39m", value)
    }
    pub fn warning(&self, value: &str) -> String {
        self.paint("\x1b[33m", "\x1b[39m", value)
    }

    /// A left-aligned table: columns two spaces apart and at most 48 wide
    /// when padded, the header bold, trailing spaces trimmed.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        let mut widths: Vec<usize> = headers.iter().map(|header| visible_width(header)).collect();
        for row in rows {
            for (column, cell) in row.iter().enumerate() {
                if let Some(current) = widths.get_mut(column) {
                    *current = (*current).max(visible_width(cell));
                }
            }
        }
        for current in &mut widths {
            *current = (*current).min(48);
        }
        let render = |cells: &[String], bold: bool| -> String {
            cells
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    let pad = widths
                        .get(column)
                        .copied()
                        .unwrap_or(0)
                        .saturating_sub(visible_width(cell));
                    let padded = format!("{cell}{}", " ".repeat(pad));
                    if bold { self.heading(&padded) } else { padded }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
        };
        let header_cells: Vec<String> = headers.iter().map(|header| (*header).to_owned()).collect();
        let mut lines = vec![render(&header_cells, true)];
        lines.extend(rows.iter().map(|row| render(row, false)));
        lines.join("\n")
    }

    /// A bar of `width` cells and the percentage beside it, colored by how
    /// full it is: warning from 60%, danger from 85%.
    pub fn meter(&self, pct: f64, width: usize) -> String {
        let clamped = pct.clamp(0.0, 100.0);
        let filled = ((clamped / 100.0) * width as f64).round() as usize;
        let (fill, rest) = if self.unicode {
            ("█", "░")
        } else {
            ("#", ".")
        };
        let bar = format!("{}{}", fill.repeat(filled), rest.repeat(width - filled));
        let bar = if clamped >= 85.0 {
            self.danger(&bar)
        } else if clamped >= 60.0 {
            self.warning(&bar)
        } else {
            self.success(&bar)
        };
        format!("{bar}{:>6}", format!("{clamped:.1}%"))
    }
}

/// Text made safe for a terminal: control characters, bidi controls and
/// every escape sequence but a color code are shown escaped rather than
/// obeyed, so a process or host name from a machine can't move the
/// cursor, rewrite a line, set the title or reach the clipboard. Newlines
/// and tabs stay.
pub fn terminal_safe(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(character) = rest.chars().next() {
        if let Some(length) = color_code(rest) {
            safe.push_str(&rest[..length]);
            rest = &rest[length..];
            continue;
        }
        let bidi = matches!(
            character,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        );
        if (character.is_control() && character != '\n' && character != '\t') || bidi {
            safe.extend(character.escape_unicode());
        } else {
            safe.push(character);
        }
        rest = &rest[character.len_utf8()..];
    }
    safe
}

/// The length of the color code (ESC [ digits and semicolons m) that
/// `text` starts with, if it starts with one.
fn color_code(text: &str) -> Option<usize> {
    let body = text.strip_prefix("\x1b[")?;
    let end = body.find(|character: char| !(character.is_ascii_digit() || character == ';'))?;
    body[end..].starts_with('m').then_some(end + 3)
}

/// Characters on screen, ignoring color codes.
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut escape = false;
    for character in text.chars() {
        if escape {
            escape = !character.is_ascii_alphabetic();
        } else if character == '\x1b' {
            escape = true;
        } else {
            width += 1;
        }
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Color codes and line breaks pass; a cursor move, a carriage
    /// return, a title or clipboard sequence, a bare escape, a C1 control
    /// and a bidi override are shown, not obeyed.
    #[test]
    fn remote_text_cannot_drive_the_terminal() {
        assert_eq!(
            terminal_safe("\x1b[1mcedar-01\x1b[22m\n\tok"),
            "\x1b[1mcedar-01\x1b[22m\n\tok"
        );
        assert_eq!(
            terminal_safe("node\x1b[2Jx\rfake\x1b]52;c;aGk=\x07\x1b\u{9b}\u{202e}gpj.exe"),
            "node\\u{1b}[2Jx\\u{d}fake\\u{1b}]52;c;aGk=\\u{7}\\u{1b}\\u{9b}\\u{202e}gpj.exe"
        );
        assert_eq!(terminal_safe("\x1b[31"), "\\u{1b}[31");
    }
}
