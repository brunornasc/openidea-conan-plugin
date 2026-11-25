//! Plugin language.
//!
//! Manifest and view text is written in **English** and translated by the IDE
//! from `locales/pt-BR.json` (English source → translation). Text the plugin
//! builds itself (notifications, prompts, errors, detail panes, process
//! messages sent to the HTML page) is not translated by the IDE: it uses the
//! same catalog, embedded with `include_str!`, and `host::locale()` to decide
//! the language. English (`en-US`) is the default; a `language` setting can
//! pin the plugin's own text.

use openidea_plugin::host;
use std::collections::BTreeMap;
use std::sync::OnceLock;

const PT_BR: &str = include_str!("../locales/pt-BR.json");

fn catalog() -> &'static BTreeMap<String, String> {
    static CATALOG: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    CATALOG.get_or_init(|| serde_json::from_str(PT_BR).unwrap_or_default())
}

/// The same catalog the IDE uses, shipped to the sandboxed HTML page.
pub fn catalog_json() -> &'static str {
    PT_BR
}

/// `auto` (the default) follows the IDE; the user can pin a language.
pub fn portuguese() -> bool {
    match host::setting_string("language").as_deref() {
        Some("pt-BR") | Some("pt") => return true,
        Some("en-US") | Some("en") => return false,
        _ => {}
    }
    host::locale().to_ascii_lowercase().starts_with("pt")
}

/// Translate a plain string. Text without a catalog entry stays English.
pub fn t(en: &str) -> String {
    if portuguese() {
        catalog().get(en).cloned().unwrap_or_else(|| en.into())
    } else {
        en.into()
    }
}

/// Translate a string with `{name}` placeholders.
pub fn tf(en: &str, args: &[(&str, &str)]) -> String {
    let mut out = t(en);
    for (key, value) in args {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use openidea_plugin::mock::MockHost;

    #[test]
    fn english_is_the_default() {
        let _host = MockHost::install();
        assert_eq!(t("Refresh"), "Refresh");
        assert_eq!(tf("Install {name}", &[("name", "zlib")]), "Install zlib");
    }

    #[test]
    fn follows_the_ide_locale() {
        let host = MockHost::install();
        host.set_locale("pt-BR");
        assert_eq!(t("Refresh"), "Atualizar");
        assert_eq!(tf("Install {name}", &[("name", "zlib")]), "Instalar zlib");
    }

    #[test]
    fn a_fixed_language_overrides_the_ide() {
        let host = MockHost::install();
        host.set_locale("pt-BR");
        host.set_setting("language", r#""en-US""#);
        assert_eq!(t("Refresh"), "Refresh");
    }

    #[test]
    fn unknown_text_falls_back_to_english() {
        let host = MockHost::install();
        host.set_locale("pt-BR");
        assert_eq!(
            t("Something not in the catalog"),
            "Something not in the catalog"
        );
    }
}
