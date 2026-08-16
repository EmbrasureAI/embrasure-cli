use std::io::IsTerminal;

#[derive(Debug, Clone, Copy)]
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn stdout() -> Self {
        Self::new(std::io::stdout().is_terminal())
    }

    pub fn stderr() -> Self {
        Self::new(std::io::stderr().is_terminal())
    }

    #[cfg(test)]
    pub const fn plain() -> Self {
        Self { enabled: false }
    }

    fn new(is_terminal: bool) -> Self {
        Self {
            enabled: is_terminal && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    pub fn good(&self, value: &str) -> String {
        self.paint("32", value)
    }

    pub fn bad(&self, value: &str) -> String {
        self.paint("31", value)
    }

    pub fn warn(&self, value: &str) -> String {
        self.paint("33", value)
    }

    pub fn bold(&self, value: &str) -> String {
        self.paint("1", value)
    }

    fn paint(&self, code: &str, value: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{value}\x1b[0m")
        } else {
            value.to_owned()
        }
    }
}

pub fn animation_enabled() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_styled_strings_are_predictable() {
        assert_eq!(Style::plain().good("PASS"), "PASS");
        let styled = Style { enabled: true };
        assert_eq!(styled.good("PASS"), "\x1b[32mPASS\x1b[0m");
        assert_eq!(styled.bad("FAIL"), "\x1b[31mFAIL\x1b[0m");
        assert_eq!(styled.warn("WARN"), "\x1b[33mWARN\x1b[0m");
        assert_eq!(styled.bold("READY"), "\x1b[1mREADY\x1b[0m");
    }
}
