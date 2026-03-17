//! Console output formatting with ANSI color support.
//!
//! Provides styled terminal output with automatic TTY detection
//! and respect for the NO_COLOR environment variable.

use std::io::{self, IsTerminal, Write};

/// ANSI style codes for terminal formatting.
#[derive(Debug, Clone, Copy)]
pub enum Style {
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
}

impl Style {
    /// Returns the ANSI escape code for this style.
    fn code(self) -> &'static str {
        match self {
            Style::Bold => "1",
            Style::Dim => "2",
            Style::Red => "31",
            Style::Green => "32",
            Style::Yellow => "33",
            Style::Blue => "34",
            Style::Magenta => "35",
            Style::Cyan => "36",
            Style::Gray => "90",
        }
    }
}

const RESET: &str = "\x1b[0m";

/// Console output handler with color support detection.
#[derive(Debug)]
pub struct Console {
    colors_enabled: bool,
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

impl Console {
    /// Creates a new Console instance, detecting color support.
    ///
    /// Colors are disabled if:
    /// - The `NO_COLOR` environment variable is set
    /// - stdout is not a terminal (TTY)
    pub fn new() -> Self {
        let colors_enabled = std::env::var("NO_COLOR").is_err() && io::stdout().is_terminal();

        Self { colors_enabled }
    }

    /// Creates a Console with colors explicitly enabled or disabled.
    pub fn with_colors(enabled: bool) -> Self {
        Self {
            colors_enabled: enabled,
        }
    }

    /// Applies ANSI styles to text if colors are enabled.
    pub fn style(&self, text: &str, styles: &[Style]) -> String {
        if !self.colors_enabled || styles.is_empty() {
            return text.to_string();
        }

        let codes: Vec<&str> = styles.iter().map(|s| s.code()).collect();
        format!("\x1b[{}m{}{}", codes.join(";"), text, RESET)
    }

    /// Creates a colored label like `[INFO]`.
    pub fn label(&self, label: &str, color: Style) -> String {
        let styled = self.style(label, &[color, Style::Bold]);
        format!("[{}]", styled)
    }

    /// Prints an info message with blue `[INFO]` label.
    pub fn info(&self, message: &str) {
        println!("{} {}", self.label("INFO", Style::Blue), message);
    }

    /// Prints a success message with green `[OK]` label.
    pub fn success(&self, message: &str) {
        println!("{} {}", self.label("OK", Style::Green), message);
    }

    /// Prints a warning message with yellow `[WARN]` label.
    pub fn warning(&self, message: &str) {
        if self.colors_enabled {
            print!("\r\n");
            let _ = io::stdout().flush();
            println!("{} {}", self.label("WARN", Style::Yellow), message);
            println!();
        } else {
            println!("{} {}", self.label("WARN", Style::Yellow), message);
        }
    }

    /// Prints an error message with red `[ERROR]` label.
    pub fn error(&self, message: &str) {
        if self.colors_enabled {
            print!("\r\n");
            let _ = io::stdout().flush();
            eprintln!("{} {}", self.label("ERROR", Style::Red), message);
            eprintln!();
        } else {
            eprintln!("{} {}", self.label("ERROR", Style::Red), message);
        }
    }

    /// Prints a step message with cyan `[STEP]` label.
    pub fn step(&self, message: &str) {
        println!("{} {}", self.label("STEP", Style::Cyan), message);
    }

    /// Prints a section header in magenta bold.
    pub fn section(&self, message: &str) {
        println!();
        println!("{}", self.style(message, &[Style::Magenta, Style::Bold]));
    }

    /// Returns text styled as muted (dim gray).
    pub fn muted(&self, text: &str) -> String {
        self.style(text, &[Style::Gray, Style::Dim])
    }

    /// Prints a progress message with cyan `[..]` label and flushes.
    pub fn progress(&self, message: &str) {
        print!("{} {}", self.label("..", Style::Cyan), message);
        let _ = io::stdout().flush();
    }

    /// Clears the current line (for progress updates).
    pub fn clear_line(&self) {
        if self.colors_enabled {
            print!("\r\x1b[2K");
            let _ = io::stdout().flush();
        }
    }

    /// Prints a progress update on the same line.
    pub fn progress_update(&self, message: &str) {
        self.clear_line();
        print!("{} {}", self.label("..", Style::Cyan), message);
        let _ = io::stdout().flush();
    }

    /// Formats a count with styling (e.g., for character counts).
    pub fn count(&self, n: usize) -> String {
        self.style(&n.to_string(), &[Style::Green, Style::Bold])
    }

    /// Formats a speed value with styling.
    pub fn speed(&self, chars_per_sec: f64) -> String {
        self.style(
            &format!("{:.0}/sec", chars_per_sec),
            &[Style::Yellow, Style::Bold],
        )
    }

    /// Formats chapter/chunk info with styling.
    pub fn chunk_info(&self, chapter: u32, chunk: u32, total_chunks: u32) -> String {
        self.style(
            &format!("[Chapter {}, Chunk {}/{}]", chapter, chunk, total_chunks),
            &[Style::Cyan, Style::Bold],
        )
    }
}

/// Global console instance for convenience.
/// Using a function instead of lazy_static for simplicity.
pub fn console() -> Console {
    Console::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_style_disabled() {
        let console = Console::with_colors(false);
        assert_eq!(console.style("hello", &[Style::Red]), "hello");
    }

    #[test]
    fn test_style_enabled() {
        let console = Console::with_colors(true);
        let styled = console.style("hello", &[Style::Red]);
        assert!(styled.contains("\x1b[31m"));
        assert!(styled.contains("hello"));
        assert!(styled.contains(RESET));
    }

    #[test]
    fn test_multiple_styles() {
        let console = Console::with_colors(true);
        let styled = console.style("hello", &[Style::Bold, Style::Red]);
        assert!(styled.contains("1;31"));
    }

    #[test]
    fn test_label() {
        let console = Console::with_colors(false);
        assert_eq!(console.label("INFO", Style::Blue), "[INFO]");
    }
}
