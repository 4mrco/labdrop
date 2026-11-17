use serde::{Deserialize, Serialize};

/// Supported UI languages for LabDrop and the USB Bootstrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lang {
    En,
    PtBr,
}

impl Lang {
    /// Detects the initial language from `LC_ALL` / `LANG` environment variables.
    pub fn detect_from_env() -> Self {
        for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(val) = std::env::var(var) {
                let lower = val.to_lowercase();
                if lower.starts_with("pt") {
                    return Lang::PtBr;
                }
                if !lower.is_empty() && lower != "c" && lower != "posix" {
                    return Lang::En;
                }
            }
        }
        Lang::En
    }

    pub fn toggle(self) -> Self {
        match self {
            Lang::En => Lang::PtBr,
            Lang::PtBr => Lang::En,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "EN",
            Lang::PtBr => "PT-BR",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::PtBr => "Português (BR)",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        let lower = s.trim().to_lowercase();
        if lower == "pt" || lower == "pt-br" || lower == "pt_br" || lower == "portuguese" {
            Some(Lang::PtBr)
        } else if lower == "en" || lower == "en-us" || lower == "english" {
            Some(Lang::En)
        } else {
            None
        }
    }
}
